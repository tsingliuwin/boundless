//! Element data model: shapes, arrows, freedraw and text, all in world coordinates.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type ElementId = Uuid;

/// A point in world coordinates (f64 for stability at deep zoom).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WPoint {
    pub x: f64,
    pub y: f64,
}

impl WPoint {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: WPoint) -> f64 {
        (self - other).hypot()
    }
}

impl std::ops::Sub for WPoint {
    type Output = WPoint;
    fn sub(self, rhs: WPoint) -> WPoint {
        WPoint::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Add for WPoint {
    type Output = WPoint;
    fn add(self, rhs: WPoint) -> WPoint {
        WPoint::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Mul<f64> for WPoint {
    type Output = WPoint;
    fn mul(self, k: f64) -> WPoint {
        WPoint::new(self.x * k, self.y * k)
    }
}

impl WPoint {
    fn hypot(self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

/// An axis-aligned bounding box in world coordinates. `w`/`h` are always >= 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WBounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl WBounds {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Build normalized bounds from two corner points (any order).
    pub fn from_corners(a: WPoint, b: WPoint) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Self {
            x,
            y,
            w: (a.x - b.x).abs(),
            h: (a.y - b.y).abs(),
        }
    }

    pub fn from_points(points: &[WPoint]) -> Self {
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for p in points {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        if points.is_empty() {
            return Self::default();
        }
        Self {
            x: min_x,
            y: min_y,
            w: max_x - min_x,
            h: max_y - min_y,
        }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn center(&self) -> WPoint {
        WPoint::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, p: WPoint) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    pub fn intersects(&self, other: &WBounds) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }

    pub fn inflate(&self, dx: f64, dy: f64) -> Self {
        Self {
            x: self.x - dx,
            y: self.y - dy,
            w: self.w + 2.0 * dx,
            h: self.h + 2.0 * dy,
        }
    }

    pub fn union(&self, other: &WBounds) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            w: self.right().max(other.right()) - x,
            h: self.bottom().max(other.bottom()) - y,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrokeStyle {
    #[default]
    Solid,
    Dashed,
}

/// Visual style shared by all elements. Colors are 0xRRGGBB.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ElementStyle {
    pub stroke: u32,
    pub background: Option<u32>,
    pub stroke_width: f64,
    pub roughness: f32,
    pub stroke_style: StrokeStyle,
    pub opacity: f32,
}

impl Default for ElementStyle {
    fn default() -> Self {
        Self {
            stroke: 0x1e1e1e,
            background: None,
            stroke_width: 2.0,
            roughness: 1.0,
            stroke_style: StrokeStyle::Solid,
            opacity: 1.0,
        }
    }
}

pub const DEFAULT_FONT_SIZE: f64 = 20.0;
pub const LINE_HEIGHT: f64 = 1.25;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ElementKind {
    Rectangle,
    Ellipse,
    Diamond,
    /// Polyline; points relative to the element origin.
    Line { points: Vec<WPoint> },
    /// Polyline with arrowheads; points relative to the element origin.
    Arrow {
        points: Vec<WPoint>,
        #[serde(default = "default_true")]
        end_arrowhead: bool,
        #[serde(default)]
        start_arrowhead: bool,
    },
    /// Freehand stroke; points relative to the element origin.
    Freedraw { points: Vec<WPoint> },
    Text {
        text: String,
        font_size: f64,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub id: ElementId,
    #[serde(flatten)]
    pub bounds: WBounds,
    pub seed: u64,
    #[serde(flatten)]
    pub style: ElementStyle,
    #[serde(flatten)]
    pub kind: ElementKind,
}

pub fn new_seed() -> u64 {
    Uuid::new_v4().as_u128() as u64
}

impl Element {
    pub fn new(kind: ElementKind, bounds: WBounds, style: ElementStyle) -> Self {
        Self {
            id: Uuid::new_v4(),
            bounds,
            seed: new_seed(),
            style,
            kind,
        }
    }

    /// Create a point-based element (line/arrow/freedraw) from absolute points:
    /// the bounds are computed and points made relative to the origin.
    pub fn from_absolute_points(
        kind_builder: impl FnOnce(Vec<WPoint>) -> ElementKind,
        mut points: Vec<WPoint>,
        style: ElementStyle,
    ) -> Self {
        let bounds = WBounds::from_points(&points);
        for p in &mut points {
            p.x -= bounds.x;
            p.y -= bounds.y;
        }
        Self::new(kind_builder(points), bounds, style)
    }

    pub fn new_text(origin: WPoint, text: String, style: ElementStyle) -> Self {
        let mut el = Self::new(
            ElementKind::Text {
                text,
                font_size: DEFAULT_FONT_SIZE,
            },
            WBounds::new(origin.x, origin.y, 0.0, DEFAULT_FONT_SIZE * LINE_HEIGHT),
            style,
        );
        // Text uses a smooth look by default.
        el.style.roughness = 0.0;
        el
    }

    pub fn is_point_based(&self) -> bool {
        matches!(
            self.kind,
            ElementKind::Line { .. } | ElementKind::Arrow { .. } | ElementKind::Freedraw { .. }
        )
    }

    pub fn is_text(&self) -> bool {
        matches!(self.kind, ElementKind::Text { .. })
    }

    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            ElementKind::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// Absolute (world-space) points for point-based elements.
    pub fn absolute_points(&self) -> Vec<WPoint> {
        let origin = WPoint::new(self.bounds.x, self.bounds.y);
        match &self.kind {
            ElementKind::Line { points }
            | ElementKind::Arrow { points, .. }
            | ElementKind::Freedraw { points } => points.iter().map(|p| origin + *p).collect(),
            _ => Vec::new(),
        }
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.bounds.x += dx;
        self.bounds.y += dy;
    }

    /// Scale the element so its bounds become `target`, where `target` is
    /// derived from `original_bounds` of the whole selection.
    pub fn rescale(&mut self, sx: f64, sy: f64, pivot: WPoint) {
        self.bounds.x = pivot.x + (self.bounds.x - pivot.x) * sx;
        self.bounds.y = pivot.y + (self.bounds.y - pivot.y) * sy;
        self.bounds.w *= sx.abs();
        self.bounds.h *= sy.abs();
        match &mut self.kind {
            ElementKind::Line { points }
            | ElementKind::Arrow { points, .. }
            | ElementKind::Freedraw { points } => {
                for p in points.iter_mut() {
                    p.x *= sx.abs();
                    p.y *= sy.abs();
                }
            }
            ElementKind::Text { font_size, .. } => {
                *font_size = (*font_size * sy.abs().max(0.05)).clamp(4.0, 400.0);
            }
            _ => {}
        }
    }

    /// Recompute bounds of point-based elements after edits, keeping the
    /// stored points relative to the (possibly new) origin.
    pub fn normalize_point_bounds(&mut self) {
        if !self.is_point_based() {
            return;
        }
        let old_origin = WPoint::new(self.bounds.x, self.bounds.y);
        let abs = self.absolute_points();
        if abs.is_empty() {
            return;
        }
        let new_bounds = WBounds::from_points(&abs);
        let new_origin = WPoint::new(new_bounds.x, new_bounds.y);
        if let ElementKind::Line { points }
        | ElementKind::Arrow { points, .. }
        | ElementKind::Freedraw { points } = &mut self.kind
        {
            for p in points.iter_mut() {
                *p = old_origin + *p - new_origin;
            }
        }
        self.bounds = new_bounds;
    }

    /// Hit test in world coordinates. `tol` is a world-space tolerance.
    pub fn hit_test(&self, p: WPoint, tol: f64) -> bool {
        let stroke_tol = (self.style.stroke_width / 2.0).max(2.0) + tol;
        match &self.kind {
            // Closed shapes: the whole bounding area is hit-testable (so a
            // click anywhere on the shape selects/moves it), regardless of
            // fill — matching Excalidraw. The border still wins for precise
            // edge hits when the shape is filled.
            ElementKind::Rectangle => self.bounds.inflate(tol, tol).contains(p),
            ElementKind::Diamond => {
                let poly = diamond_polygon(&self.bounds);
                point_in_polygon(p, &poly)
                    || distance_to_polygon(p, &poly, true) <= stroke_tol
                    || self.bounds.inflate(tol, tol).contains(p)
            }
            ElementKind::Ellipse => {
                self.bounds.inflate(tol, tol).contains(p)
                    || point_near_ellipse(p, &self.bounds, stroke_tol, true)
            }
            ElementKind::Line { .. } | ElementKind::Arrow { .. } | ElementKind::Freedraw { .. } => {
                let pts = self.absolute_points();
                distance_to_polygon(p, &pts, false) <= stroke_tol
            }
            ElementKind::Text { .. } => self.bounds.inflate(tol, tol).contains(p),
        }
    }
}

pub fn diamond_polygon(b: &WBounds) -> Vec<WPoint> {
    vec![
        WPoint::new(b.x + b.w / 2.0, b.y),
        WPoint::new(b.right(), b.y + b.h / 2.0),
        WPoint::new(b.x + b.w / 2.0, b.bottom()),
        WPoint::new(b.x, b.y + b.h / 2.0),
    ]
}

fn point_near_ellipse(p: WPoint, b: &WBounds, tol: f64, filled: bool) -> bool {
    let rx = b.w / 2.0;
    let ry = b.h / 2.0;
    if rx <= 0.0 || ry <= 0.0 {
        return false;
    }
    let c = b.center();
    let nx = (p.x - c.x) / rx;
    let ny = (p.y - c.y) / ry;
    let d = (nx * nx + ny * ny).sqrt();
    if filled && d <= 1.0 {
        return true;
    }
    // Approximate border distance in normalized space, scaled back.
    let approx_world_dist = (d - 1.0).abs() * rx.min(ry);
    approx_world_dist <= tol
}

pub fn point_in_polygon(p: WPoint, poly: &[WPoint]) -> bool {
    let mut inside = false;
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let a = poly[i];
        let b = poly[j];
        if ((a.y > p.y) != (b.y > p.y)) && (p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

pub fn point_segment_distance(p: WPoint, a: WPoint, b: WPoint) -> f64 {
    let ab = b - a;
    let len_sq = ab.x * ab.x + ab.y * ab.y;
    if len_sq <= f64::EPSILON {
        return p.distance(a);
    }
    let t = (((p.x - a.x) * ab.x + (p.y - a.y) * ab.y) / len_sq).clamp(0.0, 1.0);
    let proj = a + ab * t;
    p.distance(proj)
}

/// Minimum distance from p to a polyline (or polygon if `closed`).
pub fn distance_to_polygon(p: WPoint, pts: &[WPoint], closed: bool) -> f64 {
    if pts.is_empty() {
        return f64::INFINITY;
    }
    if pts.len() == 1 {
        return p.distance(pts[0]);
    }
    let mut min = f64::INFINITY;
    for w in pts.windows(2) {
        min = min.min(point_segment_distance(p, w[0], w[1]));
    }
    if closed {
        min = min.min(point_segment_distance(p, *pts.last().unwrap(), pts[0]));
    }
    min
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_style() -> ElementStyle {
        ElementStyle::default()
    }

    #[test]
    fn rect_hit_test_border_and_fill() {
        let mut el = Element::new(
            ElementKind::Rectangle,
            WBounds::new(0.0, 0.0, 100.0, 50.0),
            rect_style(),
        );
        // Interior is a hit even without fill, so a click anywhere on the
        // shape selects/moves it (Excalidraw behavior).
        assert!(el.hit_test(WPoint::new(50.0, 25.0), 1.0));
        assert!(el.hit_test(WPoint::new(1.0, 25.0), 1.0));
        // Outside the shape is not a hit.
        assert!(!el.hit_test(WPoint::new(200.0, 200.0), 1.0));
        // With fill: same behavior.
        el.style.background = Some(0xff0000);
        assert!(el.hit_test(WPoint::new(50.0, 25.0), 1.0));
    }

    #[test]
    fn ellipse_hit_test() {
        let el = Element::new(
            ElementKind::Ellipse,
            WBounds::new(0.0, 0.0, 100.0, 100.0),
            rect_style(),
        );
        assert!(el.hit_test(WPoint::new(50.0, 1.0), 1.5)); // top border
        assert!(el.hit_test(WPoint::new(50.0, 50.0), 1.0)); // center, selectable
        assert!(!el.hit_test(WPoint::new(-5.0, -5.0), 1.0)); // outside bounds
    }

    #[test]
    fn line_hit_test() {
        let el = Element::from_absolute_points(
            |points| ElementKind::Line { points },
            vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)],
            rect_style(),
        );
        assert!(el.hit_test(WPoint::new(50.0, 2.0), 1.0));
        assert!(!el.hit_test(WPoint::new(50.0, 20.0), 1.0));
    }

    #[test]
    fn serde_roundtrip() {
        let mut elements = vec![
            Element::new(
                ElementKind::Rectangle,
                WBounds::new(10.0, 20.0, 100.0, 50.0),
                rect_style(),
            ),
            Element::from_absolute_points(
                |points| ElementKind::Arrow {
                    points,
                    end_arrowhead: true,
                    start_arrowhead: false,
                },
                vec![WPoint::new(0.0, 0.0), WPoint::new(50.0, 30.0)],
                rect_style(),
            ),
            Element::new_text(WPoint::new(5.0, 5.0), "你好 boundless".to_string(), rect_style()),
        ];
        elements[0].style.background = Some(0xa5d8ff);
        let json = serde_json::to_string_pretty(&elements).unwrap();
        let back: Vec<Element> = serde_json::from_str(&json).unwrap();
        assert_eq!(elements, back);
    }

    #[test]
    fn rescale_scales_points_and_font() {
        let mut el = Element::from_absolute_points(
            |points| ElementKind::Freedraw { points },
            vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 100.0)],
            rect_style(),
        );
        el.rescale(2.0, 2.0, WPoint::new(0.0, 0.0));
        assert_eq!(el.bounds.w, 200.0);
        let pts = el.absolute_points();
        assert_eq!(pts[1], WPoint::new(200.0, 200.0));

        let mut t = Element::new_text(WPoint::new(0.0, 0.0), "abc".into(), rect_style());
        t.rescale(2.0, 2.0, WPoint::new(0.0, 0.0));
        match t.kind {
            ElementKind::Text { font_size, .. } => {
                assert!((font_size - DEFAULT_FONT_SIZE * 2.0).abs() < 1e-9)
            }
            _ => panic!(),
        }
    }
}
