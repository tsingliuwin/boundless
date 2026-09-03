//! Element-level render cache: keeps the expensive, camera-independent part
//! of painting alive across frames so pans, idle repaints, and unrelated
//! edits only pay for what actually changed.
//!
//! Invalidation is by *element fingerprint* — a 64-bit FNV-1a hash over every
//! rendering-relevant input (id, seed, bounds bits, style fields, kind tag,
//! and all payload bits). Each frame compares the current fingerprint against
//! the cached one; any mutation — move, restyle, in-bounds vertex edit, AI
//! update, label rebind, undo/redo — changes at least one hashed value and
//! forces regeneration. This needs zero cooperation from mutation sites,
//! which matters because elements reach the scene through several paths,
//! including direct pub-field writes (bound-label loops in board.rs) and
//! history's whole-`elements`-Vec swap on undo/redo.
//!
//! Two caches share this fingerprint machinery:
//!
//! * [`RenderCache`] — world-space geometry ([`WorldGeom`], from
//!   `crate::render::rough::world_geometry`). Pan/zoom never invalidates it;
//!   the per-frame stage only transforms to screen and tessellates.
//! * [`TextCache`] — shaped text lines. Shaping runs at screen font size
//!   (`font_size × zoom`), so the zoom is mixed into the fingerprint: panning
//!   hits, zooming re-shapes.

use std::{cell::RefCell, collections::HashMap, sync::Arc};

use gpui::Pixels;
use uuid::Uuid;

use crate::camera::Camera;
use crate::render::rough::WorldGeom;
use crate::scene::{Element, ElementId, ElementKind, LineType, TextAlign, WPoint};

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit hasher — fast, order-sensitive, and stable across runs.
struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Fnv(Self::OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }

    /// Hashes the IEEE bits, not the numeric value, so NaN/-0/precision
    /// differences all count as changes.
    fn write_f64(&mut self, v: f64) {
        self.write_u64(v.to_bits());
    }

    fn write_bool(&mut self, v: bool) {
        self.write(&[u8::from(v)]);
    }

    /// 0xff terminator so "ab" + "c" never collides with "a" + "bc".
    fn write_str(&mut self, s: &str) {
        self.write(s.as_bytes());
        self.write(&[0xff]);
    }

    fn write_opt_f64(&mut self, v: Option<f64>) {
        match v {
            Some(x) => {
                self.write(&[1]);
                self.write_f64(x);
            }
            None => self.write(&[0]),
        }
    }

    fn write_opt_uuid(&mut self, v: Option<Uuid>) {
        match v {
            Some(u) => {
                self.write(&[1]);
                self.write(u.as_bytes());
            }
            None => self.write(&[0]),
        }
    }
}

fn hash_points(h: &mut Fnv, points: &[WPoint]) {
    h.write_u64(points.len() as u64);
    for p in points {
        h.write_f64(p.x);
        h.write_f64(p.y);
    }
}

/// Extend `h` with every rendering-relevant input of `el`. Shared by the
/// geometry fingerprint and the text fingerprint (which adds shaping-only
/// inputs on top).
fn fingerprint_into(h: &mut Fnv, el: &Element) {
    h.write(el.id.as_bytes());
    h.write_u64(el.seed);
    for v in [el.bounds.x, el.bounds.y, el.bounds.w, el.bounds.h] {
        h.write_f64(v);
    }
    let s = &el.style;
    h.write_u64(u64::from(s.stroke));
    match s.background {
        Some(c) => {
            h.write_bool(true);
            h.write_u64(u64::from(c));
        }
        None => h.write_bool(false),
    }
    h.write_f64(s.stroke_width);
    h.write_f64(f64::from(s.roughness));
    h.write_bool(s.stroke_style == crate::scene::StrokeStyle::Dashed);
    h.write_bool(s.line_type == LineType::Curved);
    h.write_f64(f64::from(s.opacity));
    match &el.kind {
        ElementKind::Rectangle => h.write_u64(1),
        ElementKind::Ellipse => h.write_u64(2),
        ElementKind::Diamond => h.write_u64(3),
        ElementKind::Line { points } => {
            h.write_u64(4);
            hash_points(h, points);
        }
        ElementKind::Arrow {
            points,
            end_arrowhead,
            start_arrowhead,
        } => {
            h.write_u64(5);
            hash_points(h, points);
            h.write_bool(*end_arrowhead);
            h.write_bool(*start_arrowhead);
        }
        ElementKind::Freedraw { points, widths } => {
            h.write_u64(6);
            hash_points(h, points);
            for w in widths {
                h.write_f64(*w);
            }
        }
        ElementKind::Polygon { points } => {
            h.write_u64(8);
            hash_points(h, points);
        }
        ElementKind::Text {
            text,
            font_size,
            font_family,
            wrap_width,
            min_height,
            container_id,
            text_align,
        } => {
            h.write_u64(7);
            h.write_str(text);
            h.write_f64(*font_size);
            h.write_str(font_family);
            h.write_opt_f64(*wrap_width);
            h.write_opt_f64(*min_height);
            h.write_opt_uuid(*container_id);
            h.write_u64(match text_align {
                TextAlign::Left => 0,
                TextAlign::Center => 1,
                TextAlign::Right => 2,
            });
        }
    }
}

/// 64-bit fingerprint over every rendering-relevant input of `el`. Collision
/// probability across realistic edits is negligible: floats hash their full
/// bit patterns and strings are length-delimited, so any meaningful edit
/// changes at least one hashed bit.
pub fn fingerprint(el: &Element) -> u64 {
    let mut h = Fnv::new();
    fingerprint_into(&mut h, el);
    h.0
}

/// Fingerprint for shaping `text` at the given screen parameters: everything
/// `shape_text` consumes, and nothing else — so two elements with identical
/// text and styling share one cache entry, and the editing overlay's three
/// same-frame reshapes collapse into one.
fn text_fingerprint(
    text: &str,
    font_size_world: f64,
    font_family: &str,
    wrap_width_world: Option<f64>,
    color: gpui::Hsla,
    camera: &Camera,
) -> u64 {
    let mut h = Fnv::new();
    h.write_str(text);
    h.write_f64(font_size_world);
    h.write_str(font_family);
    h.write_opt_f64(wrap_width_world);
    // The color is baked into the shaped lines.
    h.write_f64(f64::from(color.h));
    h.write_f64(f64::from(color.s));
    h.write_f64(f64::from(color.l));
    h.write_f64(f64::from(color.a));
    h.write_f64(camera.zoom);
    h.0
}

// ---------------------------------------------------------------------------
// Geometry cache
// ---------------------------------------------------------------------------

/// Cache capacity. Scenes with more distinct elements than this simply clear
/// and recompute on overflow — always correct, rarely hit.
const MAX_ENTRIES: usize = 8192;

struct GeometryEntry {
    fingerprint: u64,
    geom: Arc<WorldGeom>,
}

/// Per-element world-geometry cache, owned by the board view. `build_paint`
/// runs with `&self`, so interior mutability (RefCell) keeps lookups
/// allocation-free; gpui entities are single-threaded, so this is sound.
#[derive(Default)]
pub struct RenderCache {
    entries: RefCell<HashMap<ElementId, GeometryEntry>>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// World geometry for `el`, from cache when the fingerprint matches.
    pub fn geometry(&self, el: &Element) -> Arc<WorldGeom> {
        let fp = fingerprint(el);
        if let Some(hit) = self.entries.borrow().get(&el.id) {
            if hit.fingerprint == fp {
                return hit.geom.clone();
            }
        }
        let geom = Arc::new(super::rough::world_geometry(el));
        let mut entries = self.entries.borrow_mut();
        if entries.len() >= MAX_ENTRIES {
            entries.clear();
        }
        entries.insert(
            el.id,
            GeometryEntry {
                fingerprint: fp,
                geom: geom.clone(),
            },
        );
        geom
    }
}

// ---------------------------------------------------------------------------
// Text shaping cache
// ---------------------------------------------------------------------------

pub struct ShapedText {
    pub lines: Arc<Vec<super::ShapedTextLine>>,
    pub line_height: Pixels,
}

struct TextEntry {
    shaped: ShapedText,
}

/// Shaped-text cache, keyed by the shaping fingerprint itself (text + style +
/// zoom) rather than by element, so identical text dedupes across elements
/// and the editing overlay's repeated reshapes collapse into one per change.
#[derive(Default)]
pub struct TextCache {
    entries: RefCell<HashMap<u64, TextEntry>>,
}

impl TextCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shaped lines for `text` at the current zoom. While the text editor is
    /// open the session buffer is cached alongside (different color than the
    /// committed text) — keyed by fingerprint, so the two states coexist
    /// without thrashing.
    #[allow(clippy::too_many_arguments)]
    pub fn shaped(
        &self,
        text: &str,
        font_size_world: f64,
        font_family: &str,
        wrap_width_world: Option<f64>,
        color: gpui::Hsla,
        camera: &Camera,
        window: &gpui::Window,
    ) -> ShapedText {
        let fp = text_fingerprint(
            text,
            font_size_world,
            font_family,
            wrap_width_world,
            color,
            camera,
        );
        if let Some(hit) = self.entries.borrow().get(&fp) {
            return ShapedText {
                lines: hit.shaped.lines.clone(),
                line_height: hit.shaped.line_height,
            };
        }
        let (lines, line_height) = super::shape_text(
            text,
            font_size_world,
            camera,
            color,
            wrap_width_world,
            font_family,
            window,
        );
        let shaped = ShapedText {
            lines: Arc::new(lines),
            line_height,
        };
        let mut entries = self.entries.borrow_mut();
        if entries.len() >= MAX_ENTRIES {
            entries.clear();
        }
        entries.insert(
            fp,
            TextEntry {
                shaped: ShapedText {
                    lines: shaped.lines.clone(),
                    line_height: shaped.line_height,
                },
            },
        );
        shaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample_element() -> Element {
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
            crate::scene::ElementStyle::default(),
        );
        el.seed = 42;
        el
    }

    #[test]
    fn fingerprint_is_stable_for_unchanged_element() {
        let el = sample_element();
        assert_eq!(fingerprint(&el), fingerprint(&el));
    }

    #[test]
    fn fingerprint_changes_on_move() {
        let mut el = sample_element();
        let before = fingerprint(&el);
        el.translate(5.0, 7.0);
        assert_ne!(before, fingerprint(&el));
    }

    #[test]
    fn fingerprint_changes_on_restyle() {
        let mut el = sample_element();
        let before = fingerprint(&el);
        el.style.stroke_width = 9.0;
        assert_ne!(before, fingerprint(&el));
    }

    #[test]
    fn fingerprint_changes_on_seed_change() {
        let mut el = sample_element();
        let before = fingerprint(&el);
        el.seed = 43;
        assert_ne!(before, fingerprint(&el));
    }

    #[test]
    fn fingerprint_changes_on_in_bounds_vertex_edit() {
        // The danger case for cheap "bounds changed?" invalidation: the
        // bounds-wrapped element contains the points (0,0),(10,0),(10,10),
        // (0,10); moving one vertex to (5,5) keeps the bounds identical but
        // MUST change the fingerprint.
        let mut el = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![
                WPoint::new(0.0, 0.0),
                WPoint::new(10.0, 0.0),
                WPoint::new(10.0, 10.0),
                WPoint::new(0.0, 10.0),
            ],
            crate::scene::ElementStyle::default(),
        );
        let before = fingerprint(&el);
        let (bx, by, bw, bh) = (el.bounds.x, el.bounds.y, el.bounds.w, el.bounds.h);
        el.set_absolute_point(3, WPoint::new(5.0, 5.0));
        assert_eq!(
            (el.bounds.x, el.bounds.y, el.bounds.w, el.bounds.h),
            (bx, by, bw, bh)
        );
        assert_ne!(
            before,
            fingerprint(&el),
            "in-bounds vertex edit must invalidate"
        );
    }

    #[test]
    fn fingerprint_changes_on_text_content() {
        let mut el = Element::new_text(
            WPoint::new(0.0, 0.0),
            "hello".into(),
            crate::scene::ElementStyle::default(),
        );
        let before = fingerprint(&el);
        if let ElementKind::Text { text, .. } = &mut el.kind {
            *text = "world".into();
        }
        assert_ne!(before, fingerprint(&el));
    }

    #[test]
    fn geometry_cache_hits_until_fingerprint_changes() {
        let cache = RenderCache::new();
        let el = sample_element();

        let first = cache.geometry(&el);
        let second = cache.geometry(&el);
        assert!(Arc::ptr_eq(&first, &second), "hit must return the same Arc");

        let mut moved = el.clone();
        moved.translate(3.0, 3.0);
        let third = cache.geometry(&moved);
        assert!(!Arc::ptr_eq(&first, &third), "mutation must recompute");

        // The old entry was replaced (keyed by id): querying the moved
        // element again hits.
        assert!(Arc::ptr_eq(&third, &cache.geometry(&moved)));
    }

    #[test]
    fn bounds_are_part_of_every_fingerprint() {
        // Zero-size bounds (a dot) must differ from another zero-size dot
        // elsewhere on the canvas.
        let a = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![WPoint::new(0.0, 0.0)],
            crate::scene::ElementStyle::default(),
        );
        let mut b = a.clone();
        b.translate(10.0, 0.0);
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn text_fingerprint_covers_every_shaping_input() {
        let camera = Camera::default();
        let black = gpui::hsla(0.0, 0.0, 0.0, 1.0);
        let base = text_fingerprint("abc", 20.0, "Excalifont", None, black, &camera);
        assert_eq!(
            base,
            text_fingerprint("abc", 20.0, "Excalifont", None, black, &camera)
        );
        assert_ne!(
            base,
            text_fingerprint("abd", 20.0, "Excalifont", None, black, &camera)
        );
        assert_ne!(
            base,
            text_fingerprint("abc", 21.0, "Excalifont", None, black, &camera)
        );
        assert_ne!(
            base,
            text_fingerprint("abc", 20.0, "KaiTi", None, black, &camera)
        );
        assert_ne!(
            base,
            text_fingerprint("abc", 20.0, "Excalifont", Some(40.0), black, &camera)
        );
        assert_ne!(
            base,
            text_fingerprint(
                "abc",
                20.0,
                "Excalifont",
                None,
                gpui::hsla(0.0, 1.0, 0.5, 1.0),
                &camera
            )
        );
        // Zoom participates (shaping runs at screen size).
        let mut zoomed = camera.clone();
        zoomed.zoom = 2.0;
        assert_ne!(
            base,
            text_fingerprint("abc", 20.0, "Excalifont", None, black, &zoomed)
        );
    }
}
