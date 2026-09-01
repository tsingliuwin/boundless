//! Incremental ink capture: turns a raw pointer stream into a smoothed,
//! variable-width stroke while the user draws.
//!
//! The collector is the single capture path for freehand strokes: the board
//! feeds it every pointer move during a pen drag and both the live preview
//! and the committed element are built from its output, which keeps the
//! existing "committed == previewed" invariant.
//!
//! Per sample it performs, in order:
//!   1. min-distance decimation (same 2-screen-px rule the board used
//!      before, now derived from the zoom captured at stroke start),
//!   2. velocity estimation (screen px/ms, zoom-normalized so the feel is
//!      identical at any zoom level),
//!   3. simulated pressure via [`crate::ink::pressure`], blended through an
//!      EMA so widths don't flicker,
//!   4. EMA position smoothing (kills single-pixel hand jitter; alpha is
//!      high enough that the lag is imperceptible).
//!
//! Widths are stored as *ratios* of the base width (`style.stroke_width`),
//! so the element stays compact and later restyling of the base width
//! rescales the whole stroke. With tapering disabled the collector emits an
//! empty width list and the element degrades to the legacy uniform stroke —
//! identical to the pre-ink pipeline.
//!
//! Pressure source per sample: when the platform supplies a hardware stylus
//! pressure (Windows Ink hook, read by board.rs per pointer event) it
//! overrides the velocity simulation — digitizer packets stream continuously
//! while a pen draws, so each captured sample pairs with the freshest
//! hardware reading; mouse input falls back to the velocity model.
//!
//! The pure [`InkCollector::push_at`] primitive takes an explicit timestamp
//! and pressure so the whole capture pipeline is testable without real time
//! or a real digitizer; [`InkCollector::push_with_pressure`] is the
//! wall-clock wrapper the board uses.

use std::time::Instant;

use crate::ink::pressure::{self, PRESSURE_START};
use crate::ink::smooth::smooth_point;
use crate::scene::WPoint;

/// Minimum pointer travel (screen px) between captured samples. Matches the
/// decimation the board applied before the ink module existed.
pub const MIN_SAMPLE_DIST_SCREEN: f64 = 2.0;
/// Position EMA blend factor: 0.5 halves the jitter amplitude per sample
/// while introducing at most half-a-sample of lag.
pub const SMOOTH_ALPHA: f64 = 0.5;

/// The finished stroke: absolute world-space centerline plus per-point
/// width ratios (parallel arrays; empty `widths` = uniform legacy stroke).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkStroke {
    pub points: Vec<WPoint>,
    pub widths: Vec<f64>,
}

#[derive(Debug)]
pub struct InkCollector {
    zoom: f64,
    taper: bool,
    start: Instant,
    min_dist: f64,
    /// Last EMA-smoothed position (also the last emitted point).
    smooth_pt: WPoint,
    /// Last raw position + timestamp, for velocity estimation.
    last_raw: Option<(WPoint, f64)>,
    /// Running pressure EMA.
    p_ema: f64,
    stroke: InkStroke,
}

impl InkCollector {
    /// Start collecting a stroke. `zoom` fixes the sample decimation for the
    /// whole stroke (zoom changes mid-stroke don't re-parametrize it);
    /// `taper` off yields the legacy uniform-width stroke (empty widths).
    pub fn new(zoom: f64, taper: bool) -> Self {
        let min_dist = if zoom > 0.0 {
            MIN_SAMPLE_DIST_SCREEN / zoom
        } else {
            MIN_SAMPLE_DIST_SCREEN
        };
        Self {
            zoom,
            taper,
            start: Instant::now(),
            min_dist,
            smooth_pt: WPoint::default(),
            last_raw: None,
            p_ema: PRESSURE_START,
            stroke: InkStroke::default(),
        }
    }

    /// Number of captured points so far.
    pub fn len(&self) -> usize {
        self.stroke.points.len()
    }

    pub fn points(&self) -> &[WPoint] {
        &self.stroke.points
    }

    /// Per-point width ratios; empty when tapering is off.
    pub fn widths(&self) -> &[f64] {
        &self.stroke.widths
    }

    /// Consume the collector into its finished stroke.
    pub fn finish(self) -> InkStroke {
        self.stroke
    }

    /// Wall-clock capture with velocity-simulated pressure. Prefer
    /// [`push_with_pressure`], which accepts hardware stylus pressure.
    #[allow(dead_code)]
    pub fn push(&mut self, raw: WPoint) -> bool {
        let t_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        self.push_at(raw, t_ms, None)
    }

    /// Wall-clock capture with an optional hardware stylus pressure
    /// (`0..=1`, read from the platform pen hook). `Some(pressure)` overrides
    /// the velocity simulation for this sample; `None` keeps it.
    pub fn push_with_pressure(&mut self, raw: WPoint, hw_pressure: Option<f64>) -> bool {
        let t_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        self.push_at(raw, t_ms, hw_pressure)
    }

    /// Capture one raw pointer sample at an explicit timestamp (ms since the
    /// collector was created). `hw_pressure` is the hardware stylus pressure
    /// when the platform has a fresh one. Returns `true` when the sample
    /// survived decimation and a point was emitted.
    pub fn push_at(&mut self, raw: WPoint, t_ms: f64, hw_pressure: Option<f64>) -> bool {
        if self.stroke.points.is_empty() {
            // First sample: no smoothing history, no velocity yet. Hardware
            // pressure (if any) blends in from the synthetic start value so
            // the stroke still tapers in.
            self.smooth_pt = raw;
            self.stroke.points.push(raw);
            if let Some(p) = hw_pressure {
                self.p_ema = pressure::smooth_pressure(self.p_ema, p);
            }
            if self.taper {
                self.stroke
                    .widths
                    .push(width_ratio(self.p_ema, hw_pressure.is_some()));
            }
            self.last_raw = Some((raw, t_ms));
            return true;
        }

        // 1. Decimation on raw travel since the last *emitted* point.
        if self.smooth_pt.distance(raw) < self.min_dist {
            return false;
        }

        // 2. Pressure update. Hardware pressure wins when present; velocity
        // in screen px/ms (zoom-normalized) drives the simulation otherwise.
        match hw_pressure {
            Some(p) => self.p_ema = pressure::smooth_pressure(self.p_ema, p),
            None => {
                if let Some((last_pt, last_t)) = self.last_raw {
                    let dt = t_ms - last_t;
                    if dt > 0.0 {
                        let speed = (last_pt.distance(raw) / dt) * self.zoom;
                        let p = pressure::speed_to_pressure(speed);
                        self.p_ema = pressure::smooth_pressure(self.p_ema, p);
                    }
                }
            }
        }
        self.last_raw = Some((raw, t_ms));

        // 3+4. Smooth the position, then emit it with the current width.
        self.smooth_pt = smooth_point(self.smooth_pt, raw, SMOOTH_ALPHA);
        self.stroke.points.push(self.smooth_pt);
        if self.taper {
            self.stroke
                .widths
                .push(width_ratio(self.p_ema, hw_pressure.is_some()));
        }
        true
    }
}

/// Width ratio for the running pressure EMA under the active pressure source
/// (hardware uses the lower width floor so light touches stay delicate).
fn width_ratio(p_ema: f64, hardware: bool) -> f64 {
    if hardware {
        pressure::width_from_hardware_pressure(p_ema)
    } else {
        pressure::width_ratio_from_smooth_pressure(p_ema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_always_emits() {
        let mut c = InkCollector::new(1.0, true);
        assert!(c.push_at(WPoint::new(5.0, 5.0), 0.0, None));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn decimation_drops_close_points() {
        let mut c = InkCollector::new(1.0, true);
        assert!(c.push_at(WPoint::new(0.0, 0.0), 0.0, None));
        // 1px away < 2px min dist at zoom 1 → dropped.
        assert!(!c.push_at(WPoint::new(1.0, 0.0), 10.0, None));
        // 10px away → emitted.
        assert!(c.push_at(WPoint::new(10.0, 0.0), 20.0, None));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn decimation_zooms_with_the_camera() {
        // At zoom 8 the world-space min distance is 2/8 = 0.25.
        let mut c = InkCollector::new(8.0, true);
        assert!(c.push_at(WPoint::new(0.0, 0.0), 0.0, None));
        assert!(!c.push_at(WPoint::new(0.1, 0.0), 10.0, None));
        assert!(c.push_at(WPoint::new(0.5, 0.0), 20.0, None));
    }

    #[test]
    fn slow_stroke_is_thicker_than_fast_stroke() {
        let mut slow = InkCollector::new(1.0, true);
        // 2px every 20ms = 0.1 px/ms → near-full pressure.
        slow.push_at(WPoint::new(0.0, 0.0), 0.0, None);
        for i in 1..10 {
            slow.push_at(WPoint::new(i as f64 * 2.0, 0.0), i as f64 * 20.0, None);
        }
        let mut fast = InkCollector::new(1.0, true);
        // 8px every 2ms = 4 px/ms → minimum pressure.
        fast.push_at(WPoint::new(0.0, 0.0), 0.0, None);
        for i in 1..10 {
            fast.push_at(WPoint::new(i as f64 * 8.0, 0.0), i as f64 * 2.0, None);
        }
        let slow_last = *slow.widths().last().unwrap();
        let fast_last = *fast.widths().last().unwrap();
        assert!(
            slow_last > fast_last + 0.3,
            "slow {slow_last} should clearly exceed fast {fast_last}"
        );
        // The slow stroke converges near full width; the fast stroke trends
        // thin (the EMA keeps it slightly above the floor after 9 samples).
        assert!(
            slow_last > 0.9,
            "slow {slow_last} should approach full width"
        );
        assert!(
            fast_last < 0.5,
            "fast {fast_last} should approach the width floor"
        );
    }

    #[test]
    fn smoothing_damps_jitter() {
        // Perfect straight line with ±4px perpendicular noise: emitted
        // points should stay much closer to the line than the raw input.
        let mut c = InkCollector::new(1.0, false);
        let mut max_dev = 0.0f64;
        for i in 0..50 {
            let noise = if i % 2 == 0 { 4.0 } else { -4.0 };
            let emitted = c.push_at(WPoint::new(i as f64 * 5.0, noise), i as f64 * 10.0, None);
            if emitted && i > 0 {
                max_dev = max_dev.max(c.points()[c.len() - 1].y.abs());
            }
        }
        assert!(max_dev < 4.0, "smoothing left {max_dev}px of jitter");
    }

    #[test]
    fn taper_off_yields_empty_widths() {
        let mut c = InkCollector::new(1.0, false);
        c.push_at(WPoint::new(0.0, 0.0), 0.0, None);
        c.push_at(WPoint::new(50.0, 0.0), 50.0, None);
        assert_eq!(c.len(), 2);
        assert!(c.widths().is_empty());
    }

    #[test]
    fn points_and_widths_stay_parallel() {
        let mut c = InkCollector::new(1.0, true);
        for i in 0..20 {
            c.push_at(WPoint::new(i as f64 * 3.0, 0.0), i as f64 * 10.0, None);
        }
        assert_eq!(c.points().len(), c.widths().len());
        let stroke = c.finish();
        assert_eq!(stroke.points.len(), stroke.widths.len());
    }

    #[test]
    fn zero_dt_keeps_previous_pressure() {
        // Two events in the same millisecond: no velocity can be computed,
        // the collector must not divide by zero or panic.
        let mut c = InkCollector::new(1.0, true);
        assert!(c.push_at(WPoint::new(0.0, 0.0), 0.0, None));
        assert!(c.push_at(WPoint::new(100.0, 0.0), 0.0, None));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn widths_are_ratios_within_bounds() {
        let mut c = InkCollector::new(1.0, true);
        for i in 0..30 {
            c.push_at(WPoint::new(i as f64 * 4.0, 0.0), i as f64 * 10.0, None);
        }
        let lo = crate::ink::pressure::WIDTH_MIN_RATIO - 1e-9;
        for &w in c.widths() {
            assert!((lo..=1.0).contains(&w), "width ratio {w} out of range");
        }
    }

    #[test]
    fn hardware_pressure_overrides_velocity() {
        // Same fast movement in both strokes; only the hardware pressure
        // differs. Velocity alone would trend the fast stroke to its floor —
        // the hardware value must win instead.
        let mut heavy = InkCollector::new(1.0, true);
        let mut light = InkCollector::new(1.0, true);
        for i in 0..12 {
            let raw = WPoint::new(i as f64 * 8.0, 0.0); // 4 px/ms — fast
            let t = i as f64 * 2.0;
            heavy.push_at(raw, t, Some(1.0));
            light.push_at(raw, t, Some(0.0));
        }
        let heavy_last = *heavy.widths().last().unwrap();
        let light_last = *light.widths().last().unwrap();
        // Heavy press converges near full width despite the speed.
        assert!(heavy_last > 0.9, "hard press {heavy_last} should widen");
        // Zero press converges near the hardware floor.
        assert!(light_last < 0.4, "light press {light_last} should thin");
        assert!(heavy_last > light_last + 0.4);
    }

    #[test]
    fn hardware_floor_is_lower_than_sim_floor() {
        // A zero-pressure hardware sample thins below the velocity-sim
        // floor (0.35) once the EMA settles — light touches stay delicate.
        let mut c = InkCollector::new(1.0, true);
        for i in 0..40 {
            c.push_at(WPoint::new(i as f64 * 2.0, 0.0), i as f64 * 20.0, Some(0.0));
        }
        let last = *c.widths().last().unwrap();
        assert!(last < crate::ink::pressure::WIDTH_MIN_RATIO);
    }

    #[test]
    fn hardware_pressure_first_sample_tapers_in() {
        // The first sample blends the hardware value with the synthetic
        // start, so a full-press stroke still starts narrower than it ends.
        let mut c = InkCollector::new(1.0, true);
        c.push_at(WPoint::new(0.0, 0.0), 0.0, Some(1.0));
        c.push_at(WPoint::new(10.0, 0.0), 10.0, Some(1.0));
        let w = c.widths().to_vec();
        assert!(
            w[0] < w[1],
            "first width {} should taper in from {}",
            w[0],
            w[1]
        );
        assert!(
            w[0] > 0.5,
            "blend {} should stay close to the pen value",
            w[0]
        );
    }
}
