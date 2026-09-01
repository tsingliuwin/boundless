//! Pure smoothing helpers for the ink pipeline.
//!
//! Everything here is a stateless numeric function so it can be unit-tested
//! without any rendering or platform dependency. The collector
//! ([`crate::ink::collector`]) applies these incrementally while a stroke is
//! being drawn; keeping the math in free functions makes the behavior easy to
//! reason about and tune independently of the capture loop.

use crate::scene::WPoint;

/// One-pole exponential smoothing: returns the new smoothed value after
/// blending the incoming sample `next` into the previous smoothed value
/// `prev`. `alpha` is the blend factor in `0..=1` — 1.0 means "no smoothing"
/// (output equals input), smaller values lag more but suppress more jitter.
pub fn exp_smooth(prev: f64, next: f64, alpha: f64) -> f64 {
    debug_assert!((0.0..=1.0).contains(&alpha), "alpha must be in 0..=1");
    prev + alpha * (next - prev)
}

/// Componentwise [`exp_smooth`] on points.
pub fn smooth_point(prev: WPoint, next: WPoint, alpha: f64) -> WPoint {
    WPoint::new(
        exp_smooth(prev.x, next.x, alpha),
        exp_smooth(prev.y, next.y, alpha),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_one_is_identity() {
        assert_eq!(exp_smooth(10.0, 42.0, 1.0), 42.0);
    }

    #[test]
    fn alpha_zero_never_moves() {
        assert_eq!(exp_smooth(10.0, 42.0, 0.0), 10.0);
    }

    #[test]
    fn half_alpha_is_midpoint() {
        assert!((exp_smooth(0.0, 10.0, 0.5) - 5.0).abs() < 1e-12);
    }

    #[test]
    fn repeated_smoothing_converges_to_input() {
        let mut v = 0.0;
        for _ in 0..200 {
            v = exp_smooth(v, 100.0, 0.3);
        }
        assert!((v - 100.0).abs() < 1e-3);
    }

    #[test]
    fn smoothing_reduces_jitter_variance() {
        // A straight signal with ±5 noise: the EMA output should have
        // strictly less variance than the raw input.
        let raw: Vec<f64> = (0..100)
            .map(|i| 100.0 + if i % 2 == 0 { 5.0 } else { -5.0 })
            .collect();
        let mut smoothed = Vec::with_capacity(raw.len());
        let mut v = raw[0];
        for &r in &raw[1..] {
            v = exp_smooth(v, r, 0.5);
            smoothed.push(v);
        }
        let variance = |xs: &[f64]| {
            let mean = xs.iter().sum::<f64>() / xs.len() as f64;
            xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64
        };
        assert!(variance(&smoothed) < variance(&raw) * 0.5);
    }

    #[test]
    fn smooth_point_moves_both_axes() {
        let out = smooth_point(WPoint::new(0.0, 0.0), WPoint::new(10.0, -10.0), 0.25);
        assert!((out.x - 2.5).abs() < 1e-12);
        assert!((out.y + 2.5).abs() < 1e-12);
    }
}
