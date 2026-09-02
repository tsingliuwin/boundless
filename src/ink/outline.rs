//! Variable-width stroke geometry: builds a closed fillable outline
//! ("ribbon") around a centerline with per-point widths.
//!
//! This is the geometry half of variable-width rendering. It is pure math —
//! the renderer converts the returned world-space polygon to screen space
//! and hands it to a GPUI fill path. Keeping it here means the shape of an
//! ink stroke can be tested without any rendering stack: straight-segment
//! width invariants, cap extents, degenerate inputs, all plain assertions.
//!
//! The outline is one closed loop: start cap (semicircle), left offset side
//! forward, end cap, right offset side backward. Rendered with a NonZero
//! fill rule so self-intersections at sharp turns stay filled (they would
//! punch holes under EvenOdd).

use crate::scene::WPoint;

/// Tessellation steps per semicircular cap. 8 is visually smooth at
/// handwriting stroke sizes and keeps vertex counts low.
pub const CAP_SEGMENTS: usize = 8;

fn normalize(v: WPoint) -> Option<WPoint> {
    let len = (v.x * v.x + v.y * v.y).sqrt();
    if len < 1e-12 {
        None
    } else {
        Some(WPoint::new(v.x / len, v.y / len))
    }
}

/// Left-pointing normal of a direction (rotate +90°).
fn perp(v: WPoint) -> WPoint {
    WPoint::new(-v.y, v.x)
}

/// Width at index `i`, padding with the last known width when the array is
/// shorter than the points (callers normally pass parallel arrays; padding
/// keeps the geometry robust instead of panicking).
fn width_at(widths: &[f64], i: usize) -> f64 {
    if widths.is_empty() {
        1.0
    } else {
        widths[i.min(widths.len() - 1)]
    }
}

/// Build the closed outline polygon around `points` with per-point
/// `widths` (world units, parallel to `points`). Returns an empty vec for
/// fewer than 2 points.
pub fn ribbon_outline(points: &[WPoint], widths: &[f64]) -> Vec<WPoint> {
    let n = points.len();
    if n < 2 {
        return Vec::new();
    }

    // Segment directions.
    let dirs: Vec<WPoint> = (0..n - 1)
        .map(|i| normalize(points[i + 1] - points[i]).unwrap_or_else(|| WPoint::new(1.0, 0.0)))
        .collect();

    // Per-vertex normals: averaged between adjacent segments (a cheap miter)
    // so corners don't pinch; endpoints use their single segment.
    let mut normals: Vec<WPoint> = Vec::with_capacity(n);
    for i in 0..n {
        let nrm = match (
            i.checked_sub(1).map(|j| perp(dirs[j])),
            perp(dirs[i.min(n - 2)]),
        ) {
            (Some(a), b) => normalize(a + b).unwrap_or_else(|| {
                // u-turn: adjacent segments point opposite — fall back to the
                // outgoing (or incoming at the last vertex) normal.
                if i < n - 1 {
                    perp(dirs[i])
                } else {
                    perp(dirs[n - 2])
                }
            }),
            (None, b) => b,
        };
        normals.push(nrm);
    }

    let left = |i: usize| points[i] + normals[i] * (width_at(widths, i) / 2.0);
    let right = |i: usize| points[i] - normals[i] * (width_at(widths, i) / 2.0);

    let mut out: Vec<WPoint> = Vec::with_capacity(2 * n + 2 * CAP_SEGMENTS - 2);

    // Start cap: semicircle around points[0] from the right side to the
    // left side, bulging backwards (against the stroke direction).
    let w0 = width_at(widths, 0) / 2.0;
    let r0 = normals[0] * (-w0);
    let back = dirs[0] * (-w0);
    for t in 0..=CAP_SEGMENTS {
        let theta = std::f64::consts::PI * t as f64 / CAP_SEGMENTS as f64;
        out.push(points[0] + r0 * theta.cos() + back * theta.sin());
    }

    // Left side, interior vertices only (cap endpoints already cover 0/n-1).
    for i in 1..n - 1 {
        out.push(left(i));
    }

    // End cap: from the left side around the tip to the right side.
    let last = n - 1;
    let wl = width_at(widths, last) / 2.0;
    let l_end = normals[last] * wl;
    let fwd = dirs[n - 2] * wl;
    for t in 0..=CAP_SEGMENTS {
        let theta = std::f64::consts::PI * t as f64 / CAP_SEGMENTS as f64;
        out.push(points[last] + l_end * theta.cos() + fwd * theta.sin());
    }

    // Right side, backward, interior vertices only.
    for i in (1..n - 1).rev() {
        out.push(right(i));
    }

    out
}

/// Tessellation steps for [`dot_outline`]. 24 makes a pen tap visually round
/// at any zoom a dot is drawn at, at negligible vertex cost.
pub const DOT_SEGMENTS: usize = 24;

/// Closed circular outline for a pen tap (dot) of `diameter` world units
/// around `center`. A single-point stroke can't go through
/// [`ribbon_outline`] (it needs ≥ 2 points), so dots get their own polygon.
pub fn dot_outline(center: WPoint, diameter: f64) -> Vec<WPoint> {
    let r = (diameter / 2.0).max(0.25);
    (0..DOT_SEGMENTS)
        .map(|i| {
            let theta = std::f64::consts::TAU * i as f64 / DOT_SEGMENTS as f64;
            WPoint::new(center.x + r * theta.cos(), center.y + r * theta.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(pts: &[WPoint]) -> (f64, f64, f64, f64) {
        let (mut x0, mut y0, mut x1, mut y1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for p in pts {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        (x0, y0, x1, y1)
    }

    #[test]
    fn fewer_than_two_points_is_empty() {
        assert!(ribbon_outline(&[], &[]).is_empty());
        assert!(ribbon_outline(&[WPoint::new(1.0, 1.0)], &[2.0]).is_empty());
    }

    #[test]
    fn straight_line_width_is_preserved() {
        // Horizontal line with uniform width 4: the body must sit at
        // y = ±2, and the caps extend exactly half a width past the ends.
        let pts = [WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)];
        let outline = ribbon_outline(&pts, &[4.0, 4.0]);
        let (x0, y0, x1, y1) = bbox(&outline);
        assert!((y0 + 2.0).abs() < 1e-9, "top {y0}");
        assert!((y1 - 2.0).abs() < 1e-9, "bottom {y1}");
        assert!((x0 + 2.0).abs() < 1e-9, "cap back {x0}");
        assert!((x1 - 102.0).abs() < 1e-9, "cap tip {x1}");
    }

    #[test]
    fn outline_closes_back_at_the_start_cap() {
        // The first vertex is the right-side offset of points[0]; the loop
        // must return to it when closed by the renderer.
        let pts = [
            WPoint::new(0.0, 0.0),
            WPoint::new(50.0, 0.0),
            WPoint::new(50.0, 50.0),
        ];
        let outline = ribbon_outline(&pts, &[2.0, 2.0, 2.0]);
        assert!(!outline.is_empty());
        let first = outline[0];
        // right_0 for a +x direction: normal is (0, 1) → right offset is
        // (0, -1) * width/2.
        assert!((first.x - 0.0).abs() < 1e-9);
        assert!((first.y + 1.0).abs() < 1e-9);
    }

    #[test]
    fn vertex_count_matches_plan() {
        let n = 7;
        let pts: Vec<WPoint> = (0..n).map(|i| WPoint::new(i as f64 * 3.0, 0.0)).collect();
        let widths = vec![2.0; n];
        let outline = ribbon_outline(&pts, &widths);
        assert_eq!(outline.len(), 2 * n + 2 * CAP_SEGMENTS - 2);
    }

    #[test]
    fn tapering_ends_are_thinner_than_the_middle() {
        // Widths grow 1 → 4: at the thin end the cap cannot reach further
        // than half of width 1, at the thick end it reaches half of 4.
        let pts = [WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)];
        let outline = ribbon_outline(&pts, &[1.0, 4.0]);
        let (_, y0, _, y1) = bbox(&outline);
        assert!((y0 + 2.0).abs() < 1e-9, "thick end top {y0}");
        assert!((y1 - 2.0).abs() < 1e-9, "thick end bottom {y1}");
        // The thin start cap: all start-cap vertices stay within 0.5 of the
        // centerline in y.
        let start_cap_max_y = outline
            .iter()
            .take(CAP_SEGMENTS + 1)
            .map(|p| p.y.abs())
            .fold(0.0, f64::max);
        assert!(
            (start_cap_max_y - 0.5).abs() < 1e-9,
            "thin cap {start_cap_max_y}"
        );
    }

    #[test]
    fn short_widths_are_padded_not_panicking() {
        let pts = [
            WPoint::new(0.0, 0.0),
            WPoint::new(10.0, 0.0),
            WPoint::new(20.0, 0.0),
        ];
        let outline = ribbon_outline(&pts, &[2.0]);
        assert!(!outline.is_empty());
        // Empty widths fall back to a unit width.
        let outline = ribbon_outline(&pts, &[]);
        assert!(!outline.is_empty());
    }

    #[test]
    fn zero_length_segment_does_not_divide_by_zero() {
        let pts = [
            WPoint::new(0.0, 0.0),
            WPoint::new(0.0, 0.0),
            WPoint::new(10.0, 0.0),
        ];
        let outline = ribbon_outline(&pts, &[2.0, 2.0, 2.0]);
        assert!(!outline.is_empty());
        // u-turn: segments fold back on themselves
        let pts = [
            WPoint::new(0.0, 0.0),
            WPoint::new(10.0, 0.0),
            WPoint::new(0.0, 0.0),
        ];
        let outline = ribbon_outline(&pts, &[2.0, 2.0, 2.0]);
        assert!(!outline.is_empty());
    }

    #[test]
    fn two_point_stroke_produces_a_valid_loop() {
        let pts = [WPoint::new(0.0, 0.0), WPoint::new(10.0, 0.0)];
        let outline = ribbon_outline(&pts, &[4.0, 4.0]);
        // Two caps only: 2 * (CAP_SEGMENTS + 1) vertices.
        assert_eq!(outline.len(), 2 * (CAP_SEGMENTS + 1));
        let (x0, y0, x1, y1) = bbox(&outline);
        assert!((x0 + 2.0).abs() < 1e-9);
        assert!((x1 - 12.0).abs() < 1e-9);
        assert!((y0 + 2.0).abs() < 1e-9);
        assert!((y1 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn dot_outline_is_a_circle_of_the_requested_diameter() {
        let dot = dot_outline(WPoint::new(100.0, 50.0), 4.0);
        assert_eq!(dot.len(), DOT_SEGMENTS);
        let (x0, y0, x1, y1) = bbox(&dot);
        assert!((x0 - 98.0).abs() < 1e-9, "left {x0}");
        assert!((x1 - 102.0).abs() < 1e-9, "right {x1}");
        assert!((y0 - 48.0).abs() < 1e-9, "top {y0}");
        assert!((y1 - 52.0).abs() < 1e-9, "bottom {y1}");
        // Every vertex sits on the circle.
        for p in &dot {
            let d = (p.x - 100.0).hypot(p.y - 50.0);
            assert!((d - 2.0).abs() < 1e-9, "radius {d}");
        }
    }

    #[test]
    fn dot_outline_clamps_tiny_diameters() {
        // Degenerate input still yields a visible circle.
        let dot = dot_outline(WPoint::new(0.0, 0.0), 0.0);
        let (_, y0, _, y1) = bbox(&dot);
        assert!((y0 + 0.25).abs() < 1e-9);
        assert!((y1 - 0.25).abs() < 1e-9);
    }
}
