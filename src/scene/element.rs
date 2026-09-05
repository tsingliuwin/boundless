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

/// How a polyline (line/arrow) connects its points: sharp straight segments
/// or a smooth curve through them (Excalidraw's "line type" property).
/// Only affects Line/Arrow rendering; freedraw strokes are always curved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineType {
    #[default]
    Straight,
    Curved,
}

/// How shape backgrounds are filled: hachure sketch lines (the classic
/// Excalidraw look, default), dense overlapping hachure (near-solid), or a
/// solid flat block (chalk-paste panels on a blackboard poster).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillStyle {
    #[default]
    Hachure,
    /// Ultra-dense hachure: the strokes overlap so much the fill reads
    /// almost as a flat patch while keeping the hand-drawn texture.
    Dense,
    Solid,
    /// Watercolor wash: several translucent hachure layers at different
    /// angles plus darkened edge pooling — soft, layered, hand-painted.
    Watercolor,
    /// Vertical linear gradient: the fill color fades into a darkened
    /// variant toward the bottom (sky panels, depth shading).
    Gradient,
}

/// Stroke rendering style for freehand pen strokes: solid ink (default),
/// grainy pencil, or a dry 飞白 brush with broken strokes.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Brush {
    #[default]
    Ink,
    Pencil,
    DryBrush,
}

/// Soft hand-drawn shadow under a closed shape: a hachure-shaded copy of
/// the outline, offset by (dx, dy) and painted translucent dark.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shadow {
    pub dx: f64,
    pub dy: f64,
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
    /// Straight vs curved polylines (lines/arrows only).
    #[serde(default)]
    pub line_type: LineType,
    /// Hachure sketch fill vs solid flat fill (shape backgrounds only).
    #[serde(default)]
    pub fill_style: FillStyle,
    /// Soft offset shadow under closed shapes. None = no shadow.
    #[serde(default)]
    pub shadow: Option<Shadow>,
    /// Pen stroke style (freedraw strokes only). None = solid ink.
    #[serde(default)]
    pub brush: Option<Brush>,
    /// Fine-grained fill tuning (agent-level). When set, each overrides the
    /// fill_style preset's derived value: fill line spacing (world units),
    /// fill line stroke width (>= 2x the gap reads nearly solid), and line
    /// angle in degrees (default -41). None = use the preset.
    #[serde(default)]
    pub hachure_gap: Option<f64>,
    #[serde(default)]
    pub fill_weight: Option<f64>,
    #[serde(default)]
    pub hachure_angle: Option<f64>,
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
            line_type: LineType::Straight,
            fill_style: FillStyle::Hachure,
            shadow: None,
            brush: None,
            hachure_gap: None,
            fill_weight: None,
            hachure_angle: None,
        }
    }
}

pub const DEFAULT_FONT_SIZE: f64 = 20.0;
pub const LINE_HEIGHT: f64 = 1.25;

/// Horizontal alignment of text within its box (bound label or wrapped box).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ElementKind {
    Rectangle,
    Ellipse,
    Diamond,
    /// Polyline; points relative to the element origin.
    Line {
        points: Vec<WPoint>,
    },
    /// Polyline with arrowheads; points relative to the element origin.
    Arrow {
        points: Vec<WPoint>,
        #[serde(default = "default_true")]
        end_arrowhead: bool,
        #[serde(default)]
        start_arrowhead: bool,
    },
    /// Freehand stroke; points relative to the element origin.
    ///
    /// `widths` holds one stroke-width *ratio* (relative to
    /// `style.stroke_width`) per point, parallel to `points`, produced by the
    /// ink pipeline (`crate::ink`). An empty vec is the legacy uniform stroke:
    /// it renders exactly like the pre-ink pipeline (single
    /// `style.stroke_width` line) and is what old scene files deserialize to.
    /// Ratios rather than absolute widths keep restyling natural — changing
    /// the base width rescales the whole stroke — and serialization compact.
    /// Like `stroke_width`, widths are not scaled by [`Element::rescale`].
    Freedraw {
        points: Vec<WPoint>,
        #[serde(default)]
        widths: Vec<f64>,
    },
    /// Closed polygon with optional fill; points relative to the element
    /// origin. The irregular-shape workhorse for the ink-wash style
    /// (mountains, land masses) — the fill renders through the dense-hachure
    /// stroke pipeline like the other closed shapes.
    Polygon {
        points: Vec<WPoint>,
        /// Closed smooth spline (Catmull-Rom through the points) instead of
        /// straight edges — the AI's organic/blob shape (petals, clouds).
        #[serde(default)]
        smooth: bool,
    },
    Text {
        text: String,
        font_size: f64,
        /// Font family name ("Caveat" hand-drawn, ".SystemUIFont" plain).
        #[serde(default = "default_font_family")]
        font_family: String,
        /// Max line width in world units; when set, lines wrap at this width.
        /// None = no wrapping (natural width).
        #[serde(default)]
        wrap_width: Option<f64>,
        /// Manual minimum height in world units. When set, the element's
        /// height is at least this (extra space is blank below the text).
        /// None = height determined by content.
        #[serde(default)]
        min_height: Option<f64>,
        /// The shape this text labels (Excalidraw-style bound text): the
        /// label is centered on the container, follows it when moved, and
        /// is removed with it. None = standalone text.
        #[serde(default)]
        container_id: Option<ElementId>,
        /// Horizontal alignment within the text box.
        #[serde(default)]
        text_align: TextAlign,
        /// Positioning anchor. None/"top-left" = x is the box's left edge
        /// (the default). "center" = x is the box's horizontal CENTER line,
        /// so centering a title on a page is `x = page center` — no mental
        /// math, and re-measurement re-centers instead of drifting right.
        #[serde(default)]
        anchor: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

/// Default font family for new text elements (hand-drawn Caveat).
fn default_font_family() -> String {
    crate::render::HANDWRITTEN_FONT.to_string()
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

    /// Like [`new`] but with a caller-supplied id. Used by AI draw tools that
    /// pre-generate the id so they can report it back to the model.
    pub fn new_with_id(id: Uuid, kind: ElementKind, bounds: WBounds, style: ElementStyle) -> Self {
        Self {
            id,
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
        points: Vec<WPoint>,
        style: ElementStyle,
    ) -> Self {
        Self::from_absolute_points_with_id(Uuid::new_v4(), kind_builder, points, style)
    }

    /// Like [`from_absolute_points`] but with a caller-supplied id.
    pub fn from_absolute_points_with_id(
        id: Uuid,
        kind_builder: impl FnOnce(Vec<WPoint>) -> ElementKind,
        mut points: Vec<WPoint>,
        style: ElementStyle,
    ) -> Self {
        let bounds = WBounds::from_points(&points);
        for p in &mut points {
            p.x -= bounds.x;
            p.y -= bounds.y;
        }
        Self::new_with_id(id, kind_builder(points), bounds, style)
    }

    pub fn new_text(origin: WPoint, text: String, style: ElementStyle) -> Self {
        let mut el = Self::new(
            ElementKind::Text {
                text,
                font_size: DEFAULT_FONT_SIZE,
                font_family: default_font_family(),
                wrap_width: None,
                min_height: None,
                container_id: None,
                text_align: TextAlign::Left,
                anchor: None,
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
            ElementKind::Line { .. }
                | ElementKind::Arrow { .. }
                | ElementKind::Freedraw { .. }
                | ElementKind::Polygon { .. }
        )
    }

    pub fn is_text(&self) -> bool {
        matches!(self.kind, ElementKind::Text { .. })
    }

    /// True for shapes that can carry a bound text label.
    pub fn is_container(&self) -> bool {
        matches!(
            self.kind,
            ElementKind::Rectangle
                | ElementKind::Ellipse
                | ElementKind::Diamond
                | ElementKind::Arrow { .. }
                | ElementKind::Line { .. }
        )
    }

    /// The container this text is bound to, if it is a label.
    pub fn container_id(&self) -> Option<ElementId> {
        match &self.kind {
            ElementKind::Text { container_id, .. } => *container_id,
            _ => None,
        }
    }

    /// Horizontal alignment of a text element (Left for non-text).
    pub fn text_align(&self) -> TextAlign {
        match &self.kind {
            ElementKind::Text { text_align, .. } => *text_align,
            _ => TextAlign::Left,
        }
    }

    pub fn wrap_width(&self) -> Option<f64> {
        match &self.kind {
            ElementKind::Text { wrap_width, .. } => *wrap_width,
            _ => None,
        }
    }

    /// True when the text is center-anchored: x is the box's horizontal
    /// center line, so after (re)measurement the box must be re-centered on
    /// `x + w/2` as measured before the width change.
    pub fn is_center_anchored(&self) -> bool {
        matches!(&self.kind,
            ElementKind::Text { anchor: Some(a), .. } if a == "center")
    }

    #[allow(dead_code)]
    pub fn min_height(&self) -> Option<f64> {
        match &self.kind {
            ElementKind::Text { min_height, .. } => *min_height,
            _ => None,
        }
    }

    pub fn font_family(&self) -> &str {
        match &self.kind {
            ElementKind::Text { font_family, .. } => font_family,
            _ => crate::render::SYSTEM_FONT,
        }
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
            | ElementKind::Freedraw { points, .. }
            | ElementKind::Polygon { points, .. } => points.iter().map(|p| origin + *p).collect(),
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
        // Text has no independent width/height — both are determined by the
        // font size — so scale it uniformly (average of sx/sy) to avoid the
        // selection frame shrinking below the text when dragging an edge.
        let is_text = matches!(self.kind, ElementKind::Text { .. });
        let (esx, esy) = if is_text {
            let s = (sx.abs() + sy.abs()) / 2.0;
            (s, s)
        } else {
            (sx.abs(), sy.abs())
        };
        self.bounds.x = pivot.x + (self.bounds.x - pivot.x) * esx;
        self.bounds.y = pivot.y + (self.bounds.y - pivot.y) * esy;
        self.bounds.w *= esx;
        self.bounds.h *= esy;
        match &mut self.kind {
            ElementKind::Line { points }
            | ElementKind::Arrow { points, .. }
            | ElementKind::Freedraw { points, .. }
            | ElementKind::Polygon { points, .. } => {
                for p in points.iter_mut() {
                    p.x *= sx.abs();
                    p.y *= sy.abs();
                }
            }
            ElementKind::Text {
                font_size,
                wrap_width,
                ..
            } => {
                *font_size = (*font_size * esx.max(0.05)).clamp(4.0, 400.0);
                if let Some(w) = wrap_width {
                    *w = (*w * esx).max(4.0);
                }
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
        | ElementKind::Freedraw { points, .. }
        | ElementKind::Polygon { points, .. } = &mut self.kind
        {
            for p in points.iter_mut() {
                *p = old_origin + *p - new_origin;
            }
        }
        self.bounds = new_bounds;
    }

    /// Set the `index`-th vertex of a point-based element to the absolute
    /// (world-space) point `p`, then renormalize bounds (the origin may move
    /// when the vertex leaves the old bounds). No-op for non-point-based
    /// elements and out-of-range indices.
    pub fn set_absolute_point(&mut self, index: usize, p: WPoint) {
        let origin = WPoint::new(self.bounds.x, self.bounds.y);
        match &mut self.kind {
            ElementKind::Line { points }
            | ElementKind::Arrow { points, .. }
            | ElementKind::Freedraw { points, .. }
            | ElementKind::Polygon { points, .. } => match points.get_mut(index) {
                Some(pt) => *pt = p - origin,
                None => return,
            },
            _ => return,
        }
        self.normalize_point_bounds();
    }

    /// Insert the absolute (world-space) point `p` after segment `seg` (as
    /// the new vertex at index `seg + 1`), then renormalize bounds. No-op
    /// for non-point-based elements and out-of-range segment indices.
    pub fn insert_absolute_point_after(&mut self, seg: usize, p: WPoint) {
        let origin = WPoint::new(self.bounds.x, self.bounds.y);
        match &mut self.kind {
            ElementKind::Line { points } | ElementKind::Arrow { points, .. } => {
                if seg + 1 > points.len() {
                    return;
                }
                points.insert(seg + 1, p - origin);
            }
            ElementKind::Freedraw { points, widths } => {
                if seg + 1 > points.len() {
                    return;
                }
                // Only touch widths when they're actually parallel (legacy
                // uniform strokes carry an empty vec).
                let parallel = widths.len() == points.len();
                points.insert(seg + 1, p - origin);
                if parallel {
                    let w = widths
                        .get(seg + 1)
                        .copied()
                        .or_else(|| widths.last().copied());
                    if let Some(w) = w {
                        widths.insert(seg + 1, w);
                    }
                }
            }
            _ => return,
        }
        self.normalize_point_bounds();
    }

    /// Remove the `index`-th vertex of a point-based element, keeping at
    /// least two points (a line needs both ends). No-op for non-point-based
    /// elements, out-of-range indices, or when only two points remain.
    pub fn remove_point(&mut self, index: usize) {
        match &mut self.kind {
            ElementKind::Line { points } | ElementKind::Arrow { points, .. } => {
                if points.len() <= 2 || index >= points.len() {
                    return;
                }
                points.remove(index);
            }
            ElementKind::Freedraw { points, widths } => {
                if points.len() <= 2 || index >= points.len() {
                    return;
                }
                points.remove(index);
                if widths.len() == points.len() + 1 {
                    widths.remove(index);
                }
            }
            _ => return,
        }
        self.normalize_point_bounds();
    }

    /// Hit test in world coordinates. `tol` is a world-space tolerance.
    pub fn hit_test(&self, p: WPoint, tol: f64) -> bool {
        let stroke_tol = (self.effective_stroke_width() / 2.0).max(2.0) + tol;
        match &self.kind {
            // 封闭多边形：内部可点选 + 边缘可精确命中（与矩形一致）。
            ElementKind::Polygon { .. } => {
                let abs = self.absolute_points();
                point_in_polygon(p, &abs) || distance_to_polygon(p, &abs, true) <= stroke_tol
            }
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
                // Curved lines/arrows render as a Catmull-Rom spline that
                // bulges away from the vertex-to-vertex chords, so hit-testing
                // against the raw points would miss the visible stroke (you'd
                // have to click on the invisible chord instead). Densify into
                // a sampled polyline that follows the rendered curve.
                // Freedraw strokes are already dense, so they're left as-is.
                let abs = self.absolute_points();
                let pts = if self.style.line_type == LineType::Curved
                    && matches!(
                        self.kind,
                        ElementKind::Line { .. } | ElementKind::Arrow { .. }
                    )
                    && abs.len() >= 2
                {
                    curve_samples(&abs, 16)
                } else {
                    abs
                };
                distance_to_polygon(p, &pts, false) <= stroke_tol
            }
            ElementKind::Text { .. } => self.bounds.inflate(tol, tol).contains(p),
        }
    }

    /// Widest stroke width in world units across this element: the base
    /// `style.stroke_width`, or — for ink strokes carrying per-point width
    /// ratios — `stroke_width × max(ratio)`. Used for hit-test tolerance so
    /// thick tapered sections stay clickable.
    pub fn effective_stroke_width(&self) -> f64 {
        let base = self.style.stroke_width;
        match &self.kind {
            ElementKind::Freedraw { widths, .. } if !widths.is_empty() => {
                base * widths.iter().cloned().fold(0.0f64, f64::max)
            }
            _ => base,
        }
    }

    /// Per-point width ratios of a freehand stroke (parallel to its points);
    /// empty for uniform legacy strokes and all other element kinds.
    pub fn ink_widths(&self) -> &[f64] {
        match &self.kind {
            ElementKind::Freedraw { widths, .. } => widths,
            _ => &[],
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

/// Densely sample the smoothing spline a curved line/arrow renders as, for
/// hit-testing. Mirrors roughr's `_curve`: Catmull-Rom (tightness 0) converted
/// to cubic Béziers, with endpoints duplicated (so the end tangents follow the
/// first/last segment). `samples_per_seg` controls fidelity. The renderer also
/// applies seeded roughness jitter (±2×roughness world units), which the hit
/// tolerance absorbs.
pub fn curve_samples(points: &[WPoint], samples_per_seg: usize) -> Vec<WPoint> {
    let n = points.len();
    if n < 2 || samples_per_seg == 0 {
        return points.to_vec();
    }
    let last = n - 1;
    let mut out = Vec::with_capacity(last * samples_per_seg + 1);
    out.push(points[0]);
    for seg in 0..last {
        let p0 = points[seg.saturating_sub(1)];
        let p1 = points[seg];
        let p2 = points[seg + 1];
        let p3 = points[(seg + 2).min(last)];
        // Catmull-Rom -> cubic Bézier control points (s = 1, tightness 0).
        let c1 = WPoint::new(p1.x + (p2.x - p0.x) / 6.0, p1.y + (p2.y - p0.y) / 6.0);
        let c2 = WPoint::new(p2.x - (p3.x - p1.x) / 6.0, p2.y - (p3.y - p1.y) / 6.0);
        for k in 1..=samples_per_seg {
            let t = k as f64 / samples_per_seg as f64;
            let u = 1.0 - t;
            let x = u * u * u * p1.x
                + 3.0 * u * u * t * c1.x
                + 3.0 * u * t * t * c2.x
                + t * t * t * p2.x;
            let y = u * u * u * p1.y
                + 3.0 * u * u * t * c1.y
                + 3.0 * u * t * t * c2.y
                + t * t * t * p2.y;
            out.push(WPoint::new(x, y));
        }
    }
    out
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
            Element::new_text(
                WPoint::new(5.0, 5.0),
                "你好 boundless".to_string(),
                rect_style(),
            ),
        ];
        elements[0].style.background = Some(0xa5d8ff);
        let json = serde_json::to_string_pretty(&elements).unwrap();
        let back: Vec<Element> = serde_json::from_str(&json).unwrap();
        assert_eq!(elements, back);
    }

    #[test]
    fn text_container_id_serde() {
        // Bound label: container_id round-trips.
        let mut label = Element::new_text(WPoint::new(0.0, 0.0), "标签".into(), rect_style());
        let container = Element::new(
            ElementKind::Rectangle,
            WBounds::new(0.0, 0.0, 100.0, 80.0),
            rect_style(),
        );
        if let ElementKind::Text { container_id, .. } = &mut label.kind {
            *container_id = Some(container.id);
        }
        let json = serde_json::to_string(&label).unwrap();
        let back: Element = serde_json::from_str(&json).unwrap();
        assert_eq!(label, back);
        assert_eq!(back.container_id(), Some(container.id));

        // Old scene files have no container_id / text_align: they must
        // default to None / Left.
        let legacy = r#"{"id":"00000000-0000-0000-0000-000000000001","x":0.0,"y":0.0,"w":10.0,"h":25.0,"seed":1,"stroke":0,"background":null,"stroke_width":2.0,"roughness":0.0,"stroke_style":"solid","opacity":1.0,"kind":"text","text":"旧","font_size":20.0}"#;
        let parsed: Element = serde_json::from_str(legacy).unwrap();
        assert!(parsed.is_text());
        assert_eq!(parsed.container_id(), None);
        assert_eq!(parsed.text_align(), TextAlign::Left);
    }

    #[test]
    fn rescale_scales_points_and_font() {
        let mut el = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
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

    fn freedraw_with_widths() -> Element {
        let mut el = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![
                WPoint::new(10.0, 10.0),
                WPoint::new(60.0, 10.0),
                WPoint::new(60.0, 60.0),
            ],
            rect_style(),
        );
        if let ElementKind::Freedraw { widths, .. } = &mut el.kind {
            *widths = vec![0.5, 1.0, 0.8];
        }
        el
    }

    #[test]
    fn freedraw_widths_roundtrip() {
        let el = freedraw_with_widths();
        let json = serde_json::to_string(&el).unwrap();
        let back: Element = serde_json::from_str(&json).unwrap();
        assert_eq!(el, back);
        assert_eq!(back.ink_widths(), &[0.5, 1.0, 0.8]);
    }

    #[test]
    fn legacy_freedraw_without_widths_defaults_to_uniform() {
        // Pre-ink scene files carry only "points" for freedraw: they must
        // parse with empty widths (uniform legacy rendering).
        let legacy = r#"{"id":"00000000-0000-0000-0000-000000000002","x":0.0,"y":0.0,"w":100.0,"h":0.0,"seed":1,"stroke":1973790,"background":null,"stroke_width":2.0,"roughness":1.0,"stroke_style":"solid","opacity":1.0,"kind":"freedraw","points":[{"x":0.0,"y":0.0},{"x":100.0,"y":0.0}]}"#;
        let parsed: Element = serde_json::from_str(legacy).unwrap();
        assert!(parsed.ink_widths().is_empty());
    }

    #[test]
    fn ink_hit_test_tolerance_follows_widest_point() {
        // A stroke with a wide middle section: a click just outside the base
        // width, within the widest section, must still hit.
        let mut el = freedraw_with_widths();
        el.style.stroke_width = 2.0;
        let wide_tol = el.effective_stroke_width();
        assert!((wide_tol - 2.0).abs() < 1e-9, "max ratio 1.0 × base 2.0");

        // Shrink the base width: the effective width scales with it.
        el.style.stroke_width = 10.0;
        assert!((el.effective_stroke_width() - 10.0).abs() < 1e-9);

        // Legacy uniform strokes report exactly the base width.
        let uniform = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)],
            rect_style(),
        );
        assert!((uniform.effective_stroke_width() - rect_style().stroke_width).abs() < 1e-9);
    }

    #[test]
    fn point_edits_keep_widths_parallel() {
        let mut el = freedraw_with_widths();

        // Insert after segment 0 → widths grow, inheriting the following
        // vertex's width (the inserted point splits its segment).
        el.insert_absolute_point_after(0, WPoint::new(35.0, 10.0));
        assert_eq!(el.ink_widths().len(), 4);
        assert_eq!(el.ink_widths()[1], 1.0);

        // Remove that vertex again → widths shrink back in step.
        el.remove_point(1);
        assert_eq!(el.ink_widths().len(), 3);
        assert_eq!(el.ink_widths().to_vec(), vec![0.5, 1.0, 0.8]);

        // Legacy uniform strokes stay uniform through edits.
        let mut uniform = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)],
            rect_style(),
        );
        uniform.insert_absolute_point_after(0, WPoint::new(50.0, 0.0));
        assert!(uniform.ink_widths().is_empty());
        uniform.remove_point(1);
        assert!(uniform.ink_widths().is_empty());
    }

    #[test]
    fn rescale_leaves_widths_untouched() {
        // Consistent with stroke_width: resizing geometry does not rescale
        // ink width ratios.
        let mut el = freedraw_with_widths();
        el.rescale(2.0, 2.0, WPoint::new(0.0, 0.0));
        assert_eq!(el.ink_widths().to_vec(), vec![0.5, 1.0, 0.8]);
    }

    #[test]
    fn text_rescale_uses_uniform_scale() {
        // Dragging only the right edge (sx=0.5, sy=1) must still change the
        // font size uniformly — otherwise the frame shrinks below the text.
        let mut t = Element::new_text(WPoint::new(0.0, 0.0), "abc".into(), rect_style());
        let orig_w = t.bounds.w;
        t.rescale(0.5, 1.0, WPoint::new(0.0, 0.0));
        // font_size scales by the average (0.5+1.0)/2 = 0.75
        match t.kind {
            ElementKind::Text { font_size, .. } => {
                assert!((font_size - DEFAULT_FONT_SIZE * 0.75).abs() < 1e-9);
            }
            _ => panic!(),
        }
        // bounds.w scales by the same uniform factor (not sx alone), so the
        // frame never becomes narrower than the (rescaled) text.
        assert!((t.bounds.w - orig_w * 0.75).abs() < 1e-9);
    }

    #[test]
    fn text_rescale_corner_scales_wrap_width() {
        // Corner drag scales both font_size and wrap_width uniformly.
        let mut t = Element::new_text(WPoint::new(0.0, 0.0), "abc".into(), rect_style());
        if let ElementKind::Text { wrap_width, .. } = &mut t.kind {
            *wrap_width = Some(100.0);
        }
        t.rescale(2.0, 2.0, WPoint::new(0.0, 0.0));
        match t.kind {
            ElementKind::Text {
                font_size,
                wrap_width,
                ..
            } => {
                assert!((font_size - DEFAULT_FONT_SIZE * 2.0).abs() < 1e-9);
                assert!((wrap_width.unwrap() - 200.0).abs() < 1e-9);
            }
            _ => panic!(),
        }
    }

    fn line(points: Vec<WPoint>) -> Element {
        Element::from_absolute_points(|points| ElementKind::Line { points }, points, rect_style())
    }

    #[test]
    fn set_absolute_point_inside_bounds_keeps_origin() {
        let mut el = line(vec![
            WPoint::new(0.0, 0.0),
            WPoint::new(50.0, 20.0),
            WPoint::new(100.0, 0.0),
        ]);
        // Move the middle vertex while staying inside the current bounds.
        el.set_absolute_point(1, WPoint::new(50.0, 10.0));
        assert_eq!(
            el.absolute_points(),
            vec![
                WPoint::new(0.0, 0.0),
                WPoint::new(50.0, 10.0),
                WPoint::new(100.0, 0.0)
            ]
        );
        // Origin unchanged (bounds still start at the same min corner).
        assert_eq!((el.bounds.x, el.bounds.y), (0.0, 0.0));
    }

    #[test]
    fn set_absolute_point_outside_bounds_moves_origin() {
        let mut el = line(vec![WPoint::new(50.0, 50.0), WPoint::new(100.0, 60.0)]);
        // Drag the first vertex up-left past the current bounds.
        el.set_absolute_point(0, WPoint::new(-20.0, -10.0));
        let abs = el.absolute_points();
        assert_eq!(
            abs,
            vec![WPoint::new(-20.0, -10.0), WPoint::new(100.0, 60.0)]
        );
        // Bounds renormalized to enclose the new points exactly.
        assert_eq!((el.bounds.x, el.bounds.y), (-20.0, -10.0));
        assert_eq!((el.bounds.w, el.bounds.h), (120.0, 70.0));
    }

    #[test]
    fn insert_absolute_point_after_bends_segment() {
        let mut el = line(vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)]);
        el.insert_absolute_point_after(0, WPoint::new(50.0, 40.0));
        assert_eq!(
            el.absolute_points(),
            vec![
                WPoint::new(0.0, 0.0),
                WPoint::new(50.0, 40.0),
                WPoint::new(100.0, 0.0)
            ]
        );
        // Bounds grew to include the new vertex.
        assert_eq!((el.bounds.h, el.bounds.y), (40.0, 0.0));
        // Out-of-range segment index is a no-op.
        el.insert_absolute_point_after(5, WPoint::new(0.0, 0.0));
        assert_eq!(el.absolute_points().len(), 3);
    }

    #[test]
    fn remove_point_keeps_at_least_two() {
        let mut el = line(vec![
            WPoint::new(0.0, 0.0),
            WPoint::new(50.0, 40.0),
            WPoint::new(100.0, 0.0),
        ]);
        el.remove_point(1);
        assert_eq!(
            el.absolute_points(),
            vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)]
        );
        // Bounds shrink after the peak vertex is gone.
        assert_eq!(el.bounds.h, 0.0);
        // Only two points left: further removal is refused.
        el.remove_point(0);
        assert_eq!(el.absolute_points().len(), 2);
        // Out-of-range index is a no-op.
        el.remove_point(9);
        assert_eq!(el.absolute_points().len(), 2);
    }

    #[test]
    fn legacy_element_without_line_type_loads_as_straight() {
        // Elements saved before the `line_type` style field existed must
        // still deserialize (flattened style + serde default).
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "x": 10.0, "y": 20.0, "w": 100.0, "h": 50.0,
            "seed": 7,
            "stroke": 1973790, "background": null, "stroke_width": 2.0,
            "roughness": 1.0, "stroke_style": "solid", "opacity": 1.0,
            "kind": "line",
            "points": [{"x": 0.0, "y": 0.0}, {"x": 100.0, "y": 50.0}]
        }"#;
        let el: Element = serde_json::from_str(json).unwrap();
        assert_eq!(el.style.line_type, LineType::Straight);
        assert_eq!(el.absolute_points().len(), 2);
    }

    #[test]
    fn curved_line_hit_test_follows_the_spline_not_the_chord() {
        let mut style = ElementStyle::default();
        style.line_type = LineType::Curved;
        let el = Element::from_absolute_points(
            |points| ElementKind::Line { points },
            vec![
                WPoint::new(0.0, 0.0),
                WPoint::new(50.0, 50.0),
                WPoint::new(100.0, 0.0),
            ],
            style,
        );
        // The spline bulges up to (21.875, 28.125) at t=0.5 of segment 0.
        // With tol=1 the stroke_tol is 3; the nearest chord (segment 0, the
        // line y=x) is ~4.42 away, so a chord-only test would miss - but the
        // sampled curve hits.
        let on_curve = WPoint::new(21.875, 28.125);
        assert!(el.hit_test(on_curve, 1.0));
        // The straight-line version of the same element does NOT hit that
        // point at this tolerance (it's off the chord).
        let mut straight = Element::from_absolute_points(
            |points| ElementKind::Line { points },
            vec![
                WPoint::new(0.0, 0.0),
                WPoint::new(50.0, 50.0),
                WPoint::new(100.0, 0.0),
            ],
            ElementStyle::default(),
        );
        straight.seed = el.seed;
        assert!(!straight.hit_test(on_curve, 1.0));
    }

    #[test]
    fn point_edits_noop_for_shapes() {
        let mut el = Element::new(
            ElementKind::Rectangle,
            WBounds::new(0.0, 0.0, 100.0, 50.0),
            rect_style(),
        );
        el.set_absolute_point(0, WPoint::new(5.0, 5.0));
        el.insert_absolute_point_after(0, WPoint::new(5.0, 5.0));
        el.remove_point(0);
        assert_eq!(el.bounds, WBounds::new(0.0, 0.0, 100.0, 50.0));
    }
}
