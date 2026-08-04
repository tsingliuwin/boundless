//! Painting helpers: dot grid, text shaping, selection overlay geometry.

pub mod rough;

use gpui::{
    fill, point, px, Bounds, Font, Hsla, PaintQuad, Pixels, Point, ShapedLine, Size, TextRun,
    Window,
};

use crate::camera::Camera;
use crate::scene::{WBounds, WPoint, LINE_HEIGHT};

/// Font family name for the hand-drawn text style (Excalifont, embedded).
pub const HANDWRITTEN_FONT: &str = "Excalifont";
/// Font family for the plain (system UI) text style.
pub const SYSTEM_FONT: &str = ".SystemUIFont";

/// Font used for canvas text elements. Defaults to the hand-drawn Caveat;
/// CJK glyphs fall back to the system UI font via GPUI's fallback chain.
pub fn canvas_font() -> Font {
    gpui::font(HANDWRITTEN_FONT)
}

/// Build a font for the given family with CJK fallback to a handwriting-
/// style system font (KaiTi 楷体 on Windows) so Chinese text also has a
/// hand-drawn look. Latin glyphs come from the primary family (Patrick Hand).
pub fn canvas_font_with(family: &str) -> Font {
    let mut f = gpui::font(family.to_string());
    // KaiTi (楷体) is a brush-style system font shipped with Windows; it
    // gives Chinese characters a hand-written feel. Microsoft YaHei is the
    // fallback if KaiTi isn't installed.
    f.fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
        "KaiTi".to_string(),
        "Microsoft YaHei".to_string(),
    ]));
    f
}

pub const GRID_SPACING: f64 = 20.0;

/// Build tiny quads for the dot grid covering the viewport.
pub fn dot_grid(
    camera: &Camera,
    viewport: Bounds<Pixels>,
    color: Hsla,
) -> Vec<PaintQuad> {
    // Keep the on-screen spacing in a comfortable range by doubling the
    // world-space step when zoomed far out.
    let mut spacing = GRID_SPACING;
    while spacing * camera.zoom < 14.0 {
        spacing *= 2.0;
    }
    while spacing * camera.zoom > 56.0 {
        spacing /= 2.0;
    }

    let dot_size = px(2.0);
    let origin = viewport.origin;
    let top_left_world = camera.screen_to_world(origin, origin);
    let bottom_right_world = camera.screen_to_world(
        point(origin.x + viewport.size.width, origin.y + viewport.size.height),
        origin,
    );

    let start_x = (top_left_world.x / spacing).floor() * spacing;
    let start_y = (top_left_world.y / spacing).floor() * spacing;

    let mut quads = Vec::new();
    let mut wy = start_y;
    while wy <= bottom_right_world.y {
        let mut wx = start_x;
        while wx <= bottom_right_world.x {
            let s = camera.world_to_screen(WPoint::new(wx, wy), origin);
            quads.push(fill(
                Bounds {
                    origin: point(s.x - dot_size * 0.5, s.y - dot_size * 0.5),
                    size: Size {
                        width: dot_size,
                        height: dot_size,
                    },
                },
                color,
            ));
            wx += spacing;
        }
        wy += spacing;
    }
    quads
}

/// One shaped line of canvas text plus its world-space origin (top-left).
pub struct ShapedTextLine {
    pub line: ShapedLine,
    /// Byte range of this line within the full text (excluding the newline).
    pub byte_range: std::ops::Range<usize>,
    /// Width of the shaped line in screen px.
    pub width: Pixels,
}

/// Shape every line of a text element at the current zoom. When
/// `wrap_width_world` is set, over-long paragraphs wrap at that width
/// (measured in world units, scaled to screen px here).
pub fn shape_text(
    text: &str,
    font_size_world: f64,
    camera: &Camera,
    color: Hsla,
    wrap_width_world: Option<f64>,
    font_family: &str,
    window: &Window,
) -> (Vec<ShapedTextLine>, Pixels) {
    let font_size = camera.scale(font_size_world).max(px(1.0));
    let line_height = font_size * LINE_HEIGHT as f32;
    let wrap_px = wrap_width_world.map(|w| camera.scale(w).max(px(1.0)));
    let font = canvas_font_with(font_family);
    let text_system = window.text_system();

    let shape_one = |s: &str| {
        let line_text = if s.is_empty() { " " } else { s };
        let shaped = text_system.shape_line(
            line_text.to_string().into(),
            font_size,
            &[TextRun {
                len: line_text.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        if s.is_empty() {
            shaped.with_len(0)
        } else {
            shaped
        }
    };

    let mut lines = Vec::new();
    let mut offset = 0usize;
    for raw in text.split('\n') {
        let segments = wrap_segment(raw, wrap_px, &shape_one);
        for seg in segments {
            let shaped = shape_one(&seg);
            let width = shaped.width;
            lines.push(ShapedTextLine {
                line: shaped,
                byte_range: offset..offset + seg.len(),
                width,
            });
            offset += seg.len();
        }
        offset += 1; // the '\n'
    }
    if lines.is_empty() {
        let shaped = text_system.shape_line(
            " ".into(),
            font_size,
            &[TextRun {
                len: 1,
                font,
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        lines.push(ShapedTextLine {
            line: shaped.with_len(0),
            byte_range: 0..0,
            width: px(0.0),
        });
    }
    (lines, line_height)
}

/// Split a single paragraph (no embedded newlines) into wrap segments that
/// each fit within `wrap_px`. Splits on character boundaries; if a single
/// character exceeds the width it still gets its own line.
fn wrap_segment(
    s: &str,
    wrap_px: Option<Pixels>,
    shape: &impl Fn(&str) -> ShapedLine,
) -> Vec<String> {
    let Some(wrap) = wrap_px else {
        return vec![s.to_string()];
    };
    if s.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Vec::new();
    let mut start = 0usize; // byte offset of current line start
    let mut last_space = None; // byte offset after last breakable space
    for i in 0..chars.len() {
        let (byte_off, ch) = chars[i];
        // Tentative line from start to this char (inclusive).
        let candidate = &s[start..byte_off + ch.len_utf8()];
        let width = shape(candidate).width;
        if width > wrap && byte_off > start {
            // Wrap: break before this char. Prefer breaking after the last
            // space if one exists in the current line.
            let break_at = last_space.unwrap_or(byte_off);
            out.push(s[start..break_at].to_string());
            start = break_at;
            // Skip a single space at the break so we don't start a line with it.
            if s[start..].starts_with(' ') {
                start += 1;
            }
            last_space = None;
        }
        if ch == ' ' {
            last_space = Some(byte_off + 1);
        }
    }
    if start < s.len() {
        out.push(s[start..].to_string());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Measure a text element's world-space size (max line width, total height).
/// Uses a zoom of 1 so results are in world units. When `min_height` is set,
/// the returned height is at least that value.
pub fn measure_text(
    text: &str,
    font_size_world: f64,
    wrap_width: Option<f64>,
    min_height: Option<f64>,
    font_family: &str,
    window: &Window,
) -> (f64, f64) {
    let camera = Camera {
        x: 0.0,
        y: 0.0,
        zoom: 1.0,
    };
    let (lines, line_height) = shape_text(
        text,
        font_size_world,
        &camera,
        gpui::hsla(0., 0., 0., 1.),
        wrap_width,
        font_family,
        window,
    );
    let max_width = lines
        .iter()
        .map(|l| l.width.to_f64())
        .fold(0.0f64, f64::max);
    let content_height = lines.len() as f64 * line_height.to_f64();
    let height = min_height.map_or(content_height, |mh| content_height.max(mh));
    (max_width, height)
}

/// The eight resize handles of a selection box, in screen space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}

pub const HANDLES: [Handle; 8] = [
    Handle::Nw,
    Handle::N,
    Handle::Ne,
    Handle::E,
    Handle::Se,
    Handle::S,
    Handle::Sw,
    Handle::W,
];

impl Handle {
    /// Position of the handle center as a fraction of the bounds (0..1).
    pub fn fraction(&self) -> (f64, f64) {
        match self {
            Handle::Nw => (0.0, 0.0),
            Handle::N => (0.5, 0.0),
            Handle::Ne => (1.0, 0.0),
            Handle::E => (1.0, 0.5),
            Handle::Se => (1.0, 1.0),
            Handle::S => (0.5, 1.0),
            Handle::Sw => (0.0, 1.0),
            Handle::W => (0.0, 0.5),
        }
    }

    /// Compute new world bounds for a drag of this handle to `to` (world),
    /// keeping the opposite edge/corner anchored.
    pub fn resize_bounds(&self, original: WBounds, to: WPoint) -> WBounds {
        let (fx, fy) = self.fraction();
        let anchor = WPoint::new(
            original.x + original.w * (1.0 - fx),
            original.y + original.h * (1.0 - fy),
        );
        let mut b = WBounds::from_corners(anchor, to);
        // Edge handles only resize along one axis.
        match self {
            Handle::N | Handle::S => {
                b.x = original.x;
                b.w = original.w;
            }
            Handle::E | Handle::W => {
                b.y = original.y;
                b.h = original.h;
            }
            _ => {}
        }
        b
    }
}

/// Screen-space rects for the resize handles around the given screen bounds.
pub fn handle_rects(screen_bounds: Bounds<Pixels>) -> Vec<(Handle, Bounds<Pixels>)> {
    let hs = px(8.0); // handle box size
    HANDLES
        .iter()
        .map(|h| {
            let (fx, fy) = h.fraction();
            let cx = screen_bounds.origin.x + screen_bounds.size.width * fx as f32;
            let cy = screen_bounds.origin.y + screen_bounds.size.height * fy as f32;
            (
                *h,
                Bounds {
                    origin: point(cx - hs * 0.5, cy - hs * 0.5),
                    size: Size {
                        width: hs,
                        height: hs,
                    },
                },
            )
        })
        .collect()
}

/// Screen-space handle rects for the control points of a selected line/arrow
/// polyline: one square per vertex (same size as resize handles) plus a
/// smaller square per segment midpoint. Dragging a midpoint inserts a new
/// vertex there (Excalidraw-style bending). Vertices come first so
/// hit-testing prioritizes them over midpoints where they overlap.
///
/// `curved` must match the element's rendered line type: for curved lines
/// the midpoint is evaluated ON the smoothing spline (the chord midpoint
/// would float off the visible stroke, increasingly so for sharp bends).
pub fn point_handle_rects(
    screen_points: &[Point<Pixels>],
    curved: bool,
) -> Vec<(crate::tools::PointTarget, Bounds<Pixels>)> {
    let vs = px(8.0); // vertex handle box size
    let ms = px(6.0); // midpoint handle box size
    let mut out = Vec::with_capacity(screen_points.len() * 2);
    for (i, p) in screen_points.iter().enumerate() {
        out.push((
            crate::tools::PointTarget::Vertex(i),
            Bounds {
                origin: point(p.x - vs * 0.5, p.y - vs * 0.5),
                size: Size {
                    width: vs,
                    height: vs,
                },
            },
        ));
    }
    for seg in 0..screen_points.len().saturating_sub(1) {
        let mid = if curved {
            curve_segment_midpoint(screen_points, seg)
        } else {
            let a = screen_points[seg];
            let b = screen_points[seg + 1];
            point((a.x + b.x) * 0.5, (a.y + b.y) * 0.5)
        };
        out.push((
            crate::tools::PointTarget::Midpoint(seg),
            Bounds {
                origin: point(mid.x - ms * 0.5, mid.y - ms * 0.5),
                size: Size {
                    width: ms,
                    height: ms,
                },
            },
        ));
    }
    out
}

/// Midpoint of segment `seg` evaluated ON the smoothed curve the renderer
/// draws. Mirrors roughr's `_curve`: a Catmull-Rom spline converted to cubic
/// Béziers with tightness 0 (control points at 1/6 of the neighbor span) and
/// duplicated endpoints. Bézier evaluation is affine-invariant, so doing this
/// in screen space matches the rendered curve exactly (up to the renderer's
/// seeded roughness jitter, which is part of the hand-drawn look).
fn curve_segment_midpoint(pts: &[Point<Pixels>], seg: usize) -> Point<Pixels> {
    let last = pts.len() - 1;
    let get = |i: usize| {
        let p = pts[i.min(last)];
        (f32::from(p.x), f32::from(p.y))
    };
    let p0 = get(seg.saturating_sub(1));
    let p1 = get(seg);
    let p2 = get(seg + 1);
    let p3 = get(seg + 2);
    let c1 = (p1.0 + (p2.0 - p0.0) / 6.0, p1.1 + (p2.1 - p0.1) / 6.0);
    let c2 = (p2.0 - (p3.0 - p1.0) / 6.0, p2.1 - (p3.1 - p1.1) / 6.0);
    // Cubic Bézier at t = 0.5: (p1 + 3·c1 + 3·c2 + p2) / 8.
    point(
        px((p1.0 + 3.0 * c1.0 + 3.0 * c2.0 + p2.0) / 8.0),
        px((p1.1 + 3.0 * c1.1 + 3.0 * c2.1 + p2.1) / 8.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_resize_keeps_anchor() {
        let original = WBounds::new(0.0, 0.0, 100.0, 50.0);
        let resized = Handle::Se.resize_bounds(original, WPoint::new(150.0, 80.0));
        assert_eq!(resized, WBounds::new(0.0, 0.0, 150.0, 80.0));

        let resized = Handle::Nw.resize_bounds(original, WPoint::new(-20.0, 10.0));
        assert_eq!(resized, WBounds::new(-20.0, 10.0, 120.0, 40.0));

        // Dragging past the anchor normalizes.
        let flipped = Handle::Se.resize_bounds(original, WPoint::new(-30.0, -10.0));
        assert_eq!(flipped, WBounds::new(-30.0, -10.0, 30.0, 10.0));

        // Edge handles only move one axis.
        let n = Handle::N.resize_bounds(original, WPoint::new(999.0, -20.0));
        assert_eq!(n, WBounds::new(0.0, -20.0, 100.0, 70.0));
    }

    #[test]
    fn curve_midpoint_stays_on_line_for_collinear_segment() {
        // Collinear points: the spline follows the straight chord (y = 0),
        // though its parametric midpoint is not the geometric one
        // (Catmull-Rom tangents use the neighbor span, so t=0.5 sits at
        // x = 21.875 here, not 25).
        let pts = vec![
            point(px(0.0), px(0.0)),
            point(px(50.0), px(0.0)),
            point(px(100.0), px(0.0)),
        ];
        let m = curve_segment_midpoint(&pts, 0);
        assert!((f32::from(m.x) - 21.875).abs() < 0.01);
        assert!(f32::from(m.y).abs() < 0.01);
    }

    #[test]
    fn curve_midpoint_leaves_the_chord_on_bends() {
        // A bent segment: the spline bulges upward, so the on-curve midpoint
        // (where the handle goes) sits above the chord midpoint (25, 25).
        // Hand-computed Catmull-Rom (tightness 0, endpoints duplicated):
        // c1 = (50/6, 50/6), c2 = (50-100/6, 50) → B(0.5) = (21.875, 28.125).
        let pts = vec![
            point(px(0.0), px(0.0)),
            point(px(50.0), px(50.0)),
            point(px(100.0), px(0.0)),
        ];
        let m = curve_segment_midpoint(&pts, 0);
        assert!((f32::from(m.x) - 21.875).abs() < 0.01);
        assert!((f32::from(m.y) - 28.125).abs() < 0.01);
    }
}
