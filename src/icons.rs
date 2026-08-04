//! Vector icon set drawn with GPUI's PathBuilder.
//!
//! Each icon is drawn in a 0..20 logical coordinate space, then translated
//! to the button's origin and stroked with the caller-provided color. Buttons
//! are fixed at 20×20 logical px so no scaling is needed.

use gpui::{canvas, point, px, Hsla, IntoElement, PathBuilder, Styled};

/// Logical icon drawing space.
pub const S: f32 = 20.0;
/// Default icon stroke width.
pub const SW: f32 = 1.6;

/// Helper: create a stroked PathBuilder with the given width.
fn stroked(width: f32) -> PathBuilder {
    PathBuilder::stroke(px(width))
}

/// A trait the icon draw closures are generic over: lets the same drawing
/// code run against a real PathBuilder (in paint) and is trivially just
/// PathBuilder itself. Kept for clarity / future scaling support.
pub trait IconCanvas {
    fn mv(&mut self, x: f32, y: f32);
    fn ln(&mut self, x: f32, y: f32);
    fn cp(&mut self);
}

impl IconCanvas for PathBuilder {
    fn mv(&mut self, x: f32, y: f32) {
        PathBuilder::move_to(self, point(px(x), px(y)));
    }
    fn ln(&mut self, x: f32, y: f32) {
        PathBuilder::line_to(self, point(px(x), px(y)));
    }
    fn cp(&mut self) {
        PathBuilder::close(self);
    }
}

/// Build an icon element: a square canvas that strokes `draw` (in 0..S coords)
/// translated to the canvas origin, in `color`. The button must be S×S.
pub fn icon(
    color: Hsla,
    draw: impl Fn(&mut PathBuilder) + 'static,
) -> impl IntoElement {
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds, _, window, _cx| {
            let mut b = stroked(SW);
            draw(&mut b);
            // Translate from 0..S space to the button origin.
            b.translate(bounds.origin);
            if let Ok(path) = b.build() {
                window.paint_path(path, color);
            }
        },
    )
    .size_full()
}

// ---------------------------------------------------------------------
// Tool icons (0..20 logical space)
// ---------------------------------------------------------------------

pub fn select(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(4.0, 3.0);
        b.ln(4.0, 16.0);
        b.ln(7.5, 12.5);
        b.ln(10.0, 17.0);
        b.ln(12.0, 16.0);
        b.ln(9.5, 11.5);
        b.ln(14.0, 11.5);
        b.cp();
    })
}

pub fn hand(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        // Palm
        b.mv(5.0, 18.0);
        b.ln(5.0, 11.0);
        b.ln(15.0, 11.0);
        b.ln(15.0, 18.0);
        b.cp();
        // Fingers
        b.mv(6.5, 11.0);
        b.ln(6.5, 5.0);
        b.mv(9.0, 11.0);
        b.ln(9.0, 4.0);
        b.mv(11.0, 11.0);
        b.ln(11.0, 4.0);
        b.mv(13.5, 11.0);
        b.ln(13.5, 5.0);
        // Thumb
        b.mv(5.0, 13.0);
        b.ln(3.0, 9.0);
    })
}

pub fn rectangle(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(3.0, 4.0);
        b.ln(17.0, 4.0);
        b.ln(17.0, 16.0);
        b.ln(3.0, 16.0);
        b.cp();
    })
}

pub fn diamond(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(10.0, 3.0);
        b.ln(17.0, 10.0);
        b.ln(10.0, 17.0);
        b.ln(3.0, 10.0);
        b.cp();
    })
}

pub fn ellipse(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        let (cx, cy, rx, ry) = (10.0, 10.0, 7.0, 6.0);
        let n = 16;
        for i in 0..=n {
            let a = std::f32::consts::TAU * (i as f32) / (n as f32);
            let x = cx + rx * a.cos();
            let y = cy + ry * a.sin();
            if i == 0 {
                b.mv(x, y);
            } else {
                b.ln(x, y);
            }
        }
        b.cp();
    })
}

pub fn arrow(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(4.0, 16.0);
        b.ln(15.0, 5.0);
        b.mv(15.0, 5.0);
        b.ln(9.0, 5.0);
        b.mv(15.0, 5.0);
        b.ln(15.0, 11.0);
    })
}

pub fn line(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(4.0, 16.0);
        b.ln(16.0, 4.0);
    })
}

pub fn pen(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(14.5, 3.5);
        b.ln(16.5, 5.5);
        b.ln(7.0, 15.0);
        b.ln(4.5, 15.5);
        b.ln(5.0, 13.0);
        b.cp();
        b.mv(5.0, 13.0);
        b.ln(7.0, 15.0);
    })
}

pub fn text(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(4.0, 5.0);
        b.ln(16.0, 5.0);
        b.mv(10.0, 5.0);
        b.ln(10.0, 16.0);
    })
}

pub fn eraser(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(6.0, 14.0);
        b.ln(10.0, 4.0);
        b.ln(15.0, 6.0);
        b.ln(11.0, 16.0);
        b.cp();
        b.mv(5.0, 16.0);
        b.ln(15.0, 16.0);
    })
}

// ---------------------------------------------------------------------
// Operation icons
// ---------------------------------------------------------------------

pub fn undo(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(15.0, 6.0);
        b.ln(7.0, 6.0);
        b.ln(7.0, 4.0);
        b.ln(4.0, 7.0);
        b.ln(7.0, 10.0);
        b.ln(7.0, 8.0);
        b.ln(12.0, 8.0);
        b.mv(15.0, 8.0);
        b.ln(15.0, 13.0);
        b.ln(6.0, 13.0);
    })
}

pub fn redo(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(5.0, 6.0);
        b.ln(13.0, 6.0);
        b.ln(13.0, 4.0);
        b.ln(16.0, 7.0);
        b.ln(13.0, 10.0);
        b.ln(13.0, 8.0);
        b.ln(8.0, 8.0);
        b.mv(5.0, 8.0);
        b.ln(5.0, 13.0);
        b.ln(14.0, 13.0);
    })
}

pub fn save(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(4.0, 4.0);
        b.ln(16.0, 4.0);
        b.ln(16.0, 16.0);
        b.ln(4.0, 16.0);
        b.cp();
        b.mv(7.0, 4.0);
        b.ln(7.0, 9.0);
        b.ln(13.0, 9.0);
        b.ln(13.0, 4.0);
        b.mv(7.0, 11.0);
        b.ln(13.0, 11.0);
        b.ln(13.0, 14.0);
        b.ln(7.0, 14.0);
        b.cp();
    })
}

pub fn open(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(3.0, 6.0);
        b.ln(8.0, 6.0);
        b.ln(10.0, 8.0);
        b.ln(17.0, 8.0);
        b.ln(17.0, 15.0);
        b.ln(3.0, 15.0);
        b.cp();
    })
}

pub fn ai(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        b.mv(10.0, 3.0);
        b.ln(11.5, 8.5);
        b.ln(17.0, 10.0);
        b.ln(11.5, 11.5);
        b.ln(10.0, 17.0);
        b.ln(8.5, 11.5);
        b.ln(3.0, 10.0);
        b.ln(8.5, 8.5);
        b.cp();
        b.mv(15.5, 4.5);
        b.ln(16.0, 6.0);
        b.ln(17.5, 6.5);
        b.ln(16.0, 7.0);
        b.ln(15.5, 8.5);
        b.ln(15.0, 7.0);
        b.ln(13.5, 6.5);
        b.ln(15.0, 6.0);
        b.cp();
    })
}

// ---------------------------------------------------------------------
// Zoom bar icons
// ---------------------------------------------------------------------

/// Fit to screen: an outer viewport rectangle with inward corner brackets,
/// the conventional "fit content to view" affordance.
pub fn zoom_fit(c: Hsla) -> impl IntoElement {
    icon(c, |b| {
        // Outer frame.
        b.mv(3.0, 4.0);
        b.ln(17.0, 4.0);
        b.ln(17.0, 16.0);
        b.ln(3.0, 16.0);
        b.cp();
        // Inward corner brackets hinting "shrink to fit".
        b.mv(6.0, 7.0);
        b.ln(6.0, 10.0);
        b.ln(9.0, 10.0);
        b.mv(14.0, 7.0);
        b.ln(14.0, 10.0);
        b.ln(11.0, 10.0);
        b.mv(6.0, 13.0);
        b.ln(9.0, 13.0);
        b.mv(14.0, 13.0);
        b.ln(11.0, 13.0);
    })
}

// ---------------------------------------------------------------------
// Stroke-width visualization (replaces "细/中/粗" text)
// ---------------------------------------------------------------------

pub fn stroke_width_icon(c: Hsla, width: f32) -> impl IntoElement {
    canvas(
        |_b, _w, _cx| (),
        move |bounds, _, window, _cx| {
            let mut b = stroked(width);
            // Draw in absolute screen coordinates (no extra translate).
            let y = f32::from(bounds.origin.y) + f32::from(bounds.size.height) * 0.5;
            let x0 = f32::from(bounds.origin.x) + 3.0;
            let x1 = f32::from(bounds.origin.x) + f32::from(bounds.size.width) - 3.0;
            b.move_to(point(px(x0), px(y)));
            b.line_to(point(px(x1), px(y)));
            if let Ok(path) = b.build() {
                window.paint_path(path, c);
            }
        },
    )
    .size_full()
}

// ---------------------------------------------------------------------
// Roughness visualization (replaces "光滑/手绘/奔放" text)
// ---------------------------------------------------------------------

pub fn roughness_icon(c: Hsla, roughness: f32) -> impl IntoElement {
    canvas(
        |_b, _w, _cx| (),
        move |bounds, _, window, _cx| {
            let mut b = stroked(SW);
            let y0 = f32::from(bounds.origin.y) + f32::from(bounds.size.height) * 0.5;
            let x0 = f32::from(bounds.origin.x) + 3.0;
            let x1 = f32::from(bounds.origin.x) + f32::from(bounds.size.width) - 3.0;
            let n = 8usize;
            for i in 0..=n {
                let t = i as f32 / n as f32;
                let x = x0 + (x1 - x0) * t;
                let amp = roughness * 3.0;
                let dy = amp * (t * std::f32::consts::TAU * 2.0).sin();
                if i == 0 {
                    b.move_to(point(px(x), px(y0 + dy)));
                } else {
                    b.line_to(point(px(x), px(y0 + dy)));
                }
            }
            if let Ok(path) = b.build() {
                window.paint_path(path, c);
            }
        },
    )
    .size_full()
}

/// Line-type icon for linear elements: a sharp polyline corner (straight)
/// or a smooth arc through the same endpoints (curved, drawn by sampling a
/// quadratic Bézier). One draw closure with a runtime branch keeps a single
/// opaque return type (same trick as [`align_icon`]).
pub fn line_type_icon(c: Hsla, line_type: crate::scene::LineType) -> impl IntoElement {
    icon(c, move |b| match line_type {
        crate::scene::LineType::Straight => {
            b.mv(3.0, 14.0);
            b.ln(10.0, 5.0);
            b.ln(17.0, 14.0);
        }
        crate::scene::LineType::Curved => {
            // Quadratic Bézier from (3,14) via control (10,3) to (17,14).
            let n = 12;
            for i in 0..=n {
                let t = i as f32 / n as f32;
                let u = 1.0 - t;
                let x = u * u * 3.0 + 2.0 * u * t * 10.0 + t * t * 17.0;
                let y = u * u * 14.0 + 2.0 * u * t * 3.0 + t * t * 14.0;
                if i == 0 {
                    b.mv(x, y);
                } else {
                    b.ln(x, y);
                }
            }
        }
    })
}

/// Text alignment icon: three horizontal bars aligned to the given side.
pub fn align_icon(c: Hsla, align: crate::scene::TextAlign) -> impl IntoElement {
    use crate::scene::TextAlign;
    icon(c, move |b| {
        for (i, len) in [14.0f32, 9.0, 14.0].iter().enumerate() {
            let y = 5.0 + i as f32 * 5.0;
            let (x0, x1) = match align {
                TextAlign::Left => (3.0, 3.0 + len),
                TextAlign::Center => (10.0 - len / 2.0, 10.0 + len / 2.0),
                TextAlign::Right => (17.0 - len, 17.0),
            };
            b.move_to(point(px(x0), px(y)));
            b.line_to(point(px(x1), px(y)));
        }
    })
}

// Send/stop icons were moved to gpui-component's IconName enum (ArrowUp / Close).
