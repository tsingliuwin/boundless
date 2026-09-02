//! Pressure (stroke-width) models for the ink pipeline.
//!
//! Two independent sources feed the same width model:
//!
//! * **Velocity simulation** (always available): pointer speed maps inversely
//!   to width — slow strokes are thick, fast strokes are thin — which is the
//!   classic calligraphic "笔锋" feel and works with a plain mouse.
//! * **Hardware pressure** (live on Windows): the WM_POINTER hook in
//!   `crate::platform` feeds the stylus digitizer pressure through
//!   [`width_from_hardware_pressure`], overriding the simulation per sample
//!   while a pen draws.
//!
//! All functions return a *width ratio* relative to the stroke's base width
//! (`style.stroke_width`), never an absolute width. Storing ratios in the
//! element keeps serialization small and makes later restyling (changing the
//! base width) scale the whole stroke naturally.
//!
//! # Tuning guide (手感调参)
//!
//! All knobs live here; none of the capture/render code embeds magic feel
//! numbers. Adjust, then `cargo test ink::` — the tests assert structural
//! invariants (monotonicity, floors, taper-in), not exact values, so tuning
//! won't break them:
//!
//! | Symptom | Knob | Direction |
//! |---|---|---|
//! | Fast strokes too thin / vanish | `WIDTH_MIN_RATIO` | ↑ (0.35 → 0.45) |
//! | Slow strokes not thick enough | `PRESSURE_GAMMA` | ↓ (0.5 → 0.35) |
//! | Width flickers on direction changes | `PRESSURE_EMA_ALPHA` | ↓ (0.3 → 0.2) |
//! | Width lags the hand too much | `PRESSURE_EMA_ALPHA` | ↑ |
//! | Even slow writing looks thin overall | `SPEED_MAX` | ↑ (4.0 → 6.0) |
//! | Every stroke starts as a wide blob | `PRESSURE_START` | ↓ (0.6 → 0.45) |
//! | Strokes feel over-smoothed / laggy | `collector::SMOOTH_ALPHA` | ↑ (0.5 → 0.65) |
//! | Hand jitter still visible | `collector::SMOOTH_ALPHA` | ↓ |
//! | Light pen touches not delicate enough | `HARDWARE_MIN_RATIO` | ↓ (0.25 → 0.15) |
//!
//! These values were chosen for mouse-driven writing at 100% zoom; real
//! digitizer feedback may warrant revisiting `SPEED_MAX` first (touchpad and
//! pen speeds differ a lot).

use crate::ink::smooth::exp_smooth;

// Tuning constants. All in one place so the handwriting feel can be adjusted
// without touching logic.
/// Pointer speed (screen px/ms) at which the width reaches its minimum.
/// Strokes faster than this stay at [`WIDTH_MIN_RATIO`].
pub const SPEED_MAX: f64 = 4.0;
/// Width floor as a fraction of the base width (fast strokes never vanish).
pub const WIDTH_MIN_RATIO: f64 = 0.35;
/// Exponent shaping the pressure→width response. <1 widens the mid-range
/// (more of the stroke is visibly thick), >1 narrows it.
pub const PRESSURE_GAMMA: f64 = 0.5;
/// Blend factor for the pressure EMA: lower = smoother width transitions
/// along the stroke, higher = more immediate response.
pub const PRESSURE_EMA_ALPHA: f64 = 0.3;
/// Initial simulated pressure at pen-down. Below 1.0 so strokes taper in
/// from a thin start instead of beginning as a wide blob.
pub const PRESSURE_START: f64 = 0.6;
/// Width floor for hardware pressure: a full-pressure stylus should be able
/// to go thinner than the simulated floor so light touches stay delicate.
pub const HARDWARE_MIN_RATIO: f64 = 0.25;

/// Map pointer speed (screen px/ms) to a simulated pressure in `0..=1`:
/// stationary = 1 (widest), [`SPEED_MAX`] or faster = 0 (thinnest).
pub fn speed_to_pressure(speed_px_per_ms: f64) -> f64 {
    (1.0 - speed_px_per_ms / SPEED_MAX).clamp(0.0, 1.0)
}

/// Exponentially smooth a pressure sample into the running value.
pub fn smooth_pressure(prev: f64, next: f64) -> f64 {
    exp_smooth(prev, next, PRESSURE_EMA_ALPHA)
}

/// Pressure (0..=1) → width ratio, shaped by `gamma` and floored at
/// `min_ratio`. Monotonically increasing in `p`.
fn pressure_to_ratio(p: f64, gamma: f64, min_ratio: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    min_ratio + (1.0 - min_ratio) * p.powf(gamma)
}

/// Width ratio for a simulated (velocity-derived) pressure sample. One-shot
/// variant of the collector's EMA path; kept public for tuning experiments
/// and the v2 hardware-pressure swap.
#[allow(dead_code)]
pub fn width_ratio_from_speed(speed_px_per_ms: f64) -> f64 {
    pressure_to_ratio(
        speed_to_pressure(speed_px_per_ms),
        PRESSURE_GAMMA,
        WIDTH_MIN_RATIO,
    )
}

/// Width ratio for a smooth pressure value that has already been through
/// [`smooth_pressure`] (e.g. the collector's running EMA).
pub fn width_ratio_from_smooth_pressure(p: f64) -> f64 {
    pressure_to_ratio(p, PRESSURE_GAMMA, WIDTH_MIN_RATIO)
}

/// Width ratio for a hardware stylus pressure sample in `0..=1` (fed by the
/// Windows WM_POINTER hook via the collector; digitizers report 0..1024 which
/// the hook normalizes). Uses a lower width floor than the simulation.
pub fn width_from_hardware_pressure(p: f64) -> f64 {
    pressure_to_ratio(p, PRESSURE_GAMMA, HARDWARE_MIN_RATIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn slow_is_thick_fast_is_thin() {
        // Stationary pointer → full width; at SPEED_MAX → floor.
        assert!(approx(width_ratio_from_speed(0.0), 1.0));
        assert!(approx(width_ratio_from_speed(SPEED_MAX), WIDTH_MIN_RATIO));
        assert!(approx(
            width_ratio_from_speed(SPEED_MAX * 3.0),
            WIDTH_MIN_RATIO
        ));
    }

    #[test]
    fn width_ratio_decreases_monotonically_with_speed() {
        let mut prev = f64::INFINITY;
        for step in 0..=20 {
            let v = SPEED_MAX * step as f64 / 20.0;
            let w = width_ratio_from_speed(v);
            assert!(w <= prev + 1e-12, "width increased at v={v}");
            prev = w;
        }
    }

    #[test]
    fn ratio_stays_within_floor_and_one() {
        for step in 0..=100 {
            let v = SPEED_MAX * 2.0 * step as f64 / 100.0;
            let w = width_ratio_from_speed(v);
            assert!((WIDTH_MIN_RATIO..=1.0).contains(&w));
        }
    }

    #[test]
    fn gamma_widens_midrange() {
        // gamma < 1 should put the mid pressure above the linear midpoint.
        let linear_mid = WIDTH_MIN_RATIO + (1.0 - WIDTH_MIN_RATIO) * 0.5;
        let shaped = pressure_to_ratio(0.5, PRESSURE_GAMMA, WIDTH_MIN_RATIO);
        assert!(shaped > linear_mid);
    }

    #[test]
    fn pressure_ema_moves_toward_input() {
        let s = smooth_pressure(0.2, 1.0);
        assert!(s > 0.2 && s < 1.0);
        // And repeated smoothing converges to the input.
        let mut v = 0.2;
        for _ in 0..200 {
            v = smooth_pressure(v, 1.0);
        }
        assert!((v - 1.0).abs() < 1e-3);
    }

    #[test]
    fn hardware_pressure_endpoints() {
        assert!(approx(width_from_hardware_pressure(1.0), 1.0));
        assert!(approx(
            width_from_hardware_pressure(0.0),
            HARDWARE_MIN_RATIO
        ));
        assert!(width_from_hardware_pressure(0.0) < WIDTH_MIN_RATIO);
    }
}
