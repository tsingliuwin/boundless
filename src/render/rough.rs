//! Converts scene elements into GPUI paths with a hand-drawn look,
//! powered by rough.js' Rust port (roughr / rough_piet).

use gpui::{px, Hsla, Path, PathBuilder, Pixels, Point};
use rough_piet::KurboGenerator;
use roughr::Srgba;

use crate::camera::Camera;
use crate::scene::{
    curve_samples, diamond_polygon, Element, ElementKind, ElementStyle, LineType, StrokeStyle,
    WPoint,
};
use roughr::core::{FillStyle, LineCap, LineJoin, OpSetType, Options};

/// One tessellated path ready to paint.
pub struct ReadyPath {
    pub path: Path<Pixels>,
    pub color: Hsla,
}

/// Convert a 0xRRGGBB color + opacity into an Hsla.
pub fn color_u32(rgb: u32, opacity: f32) -> Hsla {
    let r = ((rgb >> 16) & 0xff) as f32 / 255.0;
    let g = ((rgb >> 8) & 0xff) as f32 / 255.0;
    let b = (rgb & 0xff) as f32 / 255.0;
    gpui::Rgba { r, g, b, a: opacity }.into()
}

fn srgba(rgb: u32, opacity: f32) -> Srgba {
    Srgba::new(
        ((rgb >> 16) & 0xff) as f32 / 255.0,
        ((rgb >> 8) & 0xff) as f32 / 255.0,
        (rgb & 0xff) as f32 / 255.0,
        opacity,
    )
}

fn to_euclid(p: WPoint) -> euclid::default::Point2D<f64> {
    euclid::default::Point2D::new(p.x, p.y)
}

fn options_for(style: &ElementStyle, seed: u64, is_freedraw: bool) -> Options {
    let mut options = Options::default();
    let roughness = style.roughness.clamp(0.0, 3.0);
    options.roughness = Some(roughness);
    options.seed = Some(seed);
    options.stroke = Some(srgba(style.stroke, style.opacity));
    options.stroke_width = Some(style.stroke_width as f32);
    // Only disable multi-stroke for the "architect" (smooth) look. Freedraw
    // keeps the double-pass: two slightly-offset strokes are what give
    // hand-drawn ink its characteristic texture (Excalidraw does the same).
    options.disable_multi_stroke = Some(roughness <= 0.01);
    // Round caps/joins for all strokes — smoother endpoints and corners,
    // matching Excalidraw's look. The default (Butt/Miter) looks harsh.
    options.line_cap = Some(LineCap::Round);
    options.line_join = Some(LineJoin::Round);
    // Freedraw: keep the actual pointer vertices fixed so the stroke
    // follows the user's hand precisely, while the control-point jitter
    // between vertices still gives the hand-drawn texture. Without this
    // every sample point is randomly offset → a jittery, unstable
    // line (Excalidraw sets preserveVertices: true for freedraw).
    if is_freedraw {
        options.preserve_vertices = Some(true);
        // Subtler jitter than the shape default: the multi-stroke double-pass
        // is kept (gives a hand-drawn feel), but the offset is small so the
        // two passes stay close together → fine, delicate texture rather
        // than a visibly shaky line. Excalidraw's pen uses a similar low
        // amplitude (its roughness for freedraw ≈ 0.5-1.0 with reduced
        // randomness).
        options.max_randomness_offset = Some(0.6);
        // No bowing for freedraw: the curve already follows the hand, and
        // bowing would add a perpendicular bulge that looks wrong on a
        // freehand stroke.
        options.bowing = Some(0.0);
    }
    if let Some(bg) = style.background {
        options.fill = Some(srgba(bg, style.opacity));
        options.fill_style = Some(FillStyle::Hachure);
        options.fill_weight = Some((style.stroke_width as f32 * 0.5).max(0.5));
        options.hachure_gap = Some((style.stroke_width as f32 * 4.0).max(4.0));
        options.disable_multi_stroke_fill = Some(true);
    }
    options
}

/// Append the segments of a kurbo BezPath (world coords) to a PathBuilder,
/// flattening curves to line segments.
fn append_bez_path(
    builder: &mut PathBuilder,
    bez: &kurbo::BezPath,
    camera: &Camera,
    origin: Point<Pixels>,
) {
    let tol = (0.6 / camera.zoom).clamp(0.05, 1.0);
    kurbo::flatten(bez.elements().iter().copied(), tol, |el| match el {
        kurbo::PathEl::MoveTo(p) => {
            builder.move_to(camera.world_to_screen(WPoint::new(p.x, p.y), origin))
        }
        kurbo::PathEl::LineTo(p) => {
            builder.line_to(camera.world_to_screen(WPoint::new(p.x, p.y), origin))
        }
        kurbo::PathEl::ClosePath => builder.close(),
        // flatten() only emits the three variants above.
        _ => {}
    });
}

/// Generate the rough-styled paint paths for one element.
/// Returned in paint order: fills first, then strokes.
pub fn paths_for_element(
    el: &Element,
    camera: &Camera,
    canvas_origin: Point<Pixels>,
) -> Vec<ReadyPath> {
    let style = &el.style;
    let b = &el.bounds;
    let mut out: Vec<ReadyPath> = Vec::new();

    let stroke_color = color_u32(style.stroke, style.opacity);
    let fill_color = style
        .background
        .map(|c| color_u32(c, style.opacity))
        .unwrap_or(stroke_color);

    let stroke_width_px = camera.scale(style.stroke_width).max(px(0.5));
    let fill_weight_px = camera.scale(style.stroke_width * 0.5).max(px(0.5));
    let dashed = style.stroke_style == StrokeStyle::Dashed;

    let mut push_drawable = |drawable: &rough_piet::KurboDrawable<f64>, out: &mut Vec<ReadyPath>| {
        for set in &drawable.sets {
            match set.op_set_type {
                OpSetType::FillPath => {
                    let mut builder = PathBuilder::fill();
                    append_bez_path(&mut builder, &set.ops, camera, canvas_origin);
                    if let Ok(path) = builder.build() {
                        out.push(ReadyPath {
                            path,
                            color: fill_color,
                        });
                    }
                }
                OpSetType::FillSketch => {
                    let mut builder = PathBuilder::stroke(fill_weight_px);
                    append_bez_path(&mut builder, &set.ops, camera, canvas_origin);
                    if let Ok(path) = builder.build() {
                        out.push(ReadyPath {
                            path,
                            color: fill_color,
                        });
                    }
                }
                OpSetType::Path => {
                    let mut builder = PathBuilder::stroke(stroke_width_px);
                    if dashed {
                        let dash = camera.scale(12.0);
                        let gap = camera.scale(8.0);
                        builder = builder.dash_array(&[dash, gap]);
                    }
                    append_bez_path(&mut builder, &set.ops, camera, canvas_origin);
                    if let Ok(path) = builder.build() {
                        out.push(ReadyPath {
                            path,
                            color: stroke_color,
                        });
                    }
                }
            }
        }
    };

    match &el.kind {
        ElementKind::Rectangle => {
            if b.w < 0.01 || b.h < 0.01 {
                return out;
            }
            let gen = KurboGenerator::new(options_for(style, el.seed, false));
            push_drawable(&gen.rectangle(b.x, b.y, b.w, b.h), &mut out);
        }
        ElementKind::Ellipse => {
            if b.w < 0.01 || b.h < 0.01 {
                return out;
            }
            let gen = KurboGenerator::new(options_for(style, el.seed, false));
            // roughr's ellipse treats (x, y) as the CENTER and width/height as
            // diameters, but our bounds.x/y is the top-left corner. Translate.
            let center = b.center();
            push_drawable(&gen.ellipse(center.x, center.y, b.w, b.h), &mut out);
        }
        ElementKind::Diamond => {
            if b.w < 0.01 || b.h < 0.01 {
                return out;
            }
            let gen = KurboGenerator::new(options_for(style, el.seed, false));
            let points: Vec<_> = diamond_polygon(b).iter().map(|p| to_euclid(*p)).collect();
            push_drawable(&gen.polygon(&points), &mut out);
        }
        ElementKind::Line { .. } | ElementKind::Arrow { .. } | ElementKind::Freedraw { .. } => {
            let points = el.absolute_points();
            if points.len() < 2 {
                return out;
            }
            let is_freedraw = matches!(el.kind, ElementKind::Freedraw { .. });
            // Lines/arrows render as straight polylines by default; the
            // "curved" line type fits a smooth curve through the points
            // (Excalidraw). Freedraw strokes are always smoothed.
            let curved = is_freedraw || style.line_type == LineType::Curved;

            if is_freedraw {
                // Freedraw bypasses roughr entirely: roughr's curve()
                // applies random control-point jitter that, while seeded,
                // produces slightly different paths across re-renders (the
                // flatten tolerance changes with zoom, and the multi-stroke
                // second pass inherits the first pass's advanced RNG state).
                // Instead, sample a deterministic Catmull-Rom spline through
                // the pointer points and stroke it directly. A committed
                // stroke never moves.
                let samples = curve_samples(&points, 12);
                let mut builder = PathBuilder::stroke(stroke_width_px);
                let origin = canvas_origin;
                let screen = |p: &WPoint| camera.world_to_screen(*p, origin);
                if let Some(first) = samples.first() {
                    builder.move_to(screen(first));
                    for p in samples.iter().skip(1) {
                        builder.line_to(screen(p));
                    }
                    if let Ok(path) = builder.build() {
                        out.push(ReadyPath {
                            path,
                            color: stroke_color,
                        });
                    }
                }
            } else {
                let gen = KurboGenerator::new(options_for(style, el.seed, false));
                let euclid_points: Vec<_> = points.iter().map(|p| to_euclid(*p)).collect();
                if curved {
                    push_drawable(&gen.curve(&euclid_points), &mut out);
                } else {
                    push_drawable(&gen.linear_path(&euclid_points, false), &mut out);
                }
            }

            if let ElementKind::Arrow {
                end_arrowhead,
                start_arrowhead,
                ..
            } = &el.kind
            {
                let gen = KurboGenerator::new(options_for(style, el.seed, false));
                let head_len = (16.0 + style.stroke_width * 3.0).max(20.0);
                if *end_arrowhead && points.len() >= 2 {
                    push_arrowhead(
                        &gen,
                        &points,
                        points.len() - 1,
                        head_len,
                        &mut push_drawable,
                        &mut out,
                    );
                }
                if *start_arrowhead && points.len() >= 2 {
                    push_arrowhead(&gen, &points, 0, head_len, &mut push_drawable, &mut out);
                }
            }
        }
        ElementKind::Text { .. } => {
            // Text is shaped and painted separately (see render::text).
        }
    }

    out
}

fn push_arrowhead(
    gen: &KurboGenerator,
    points: &[WPoint],
    tip_index: usize,
    head_len: f64,
    push: &mut impl FnMut(&rough_piet::KurboDrawable<f64>, &mut Vec<ReadyPath>),
    out: &mut Vec<ReadyPath>,
) {
    let tip = points[tip_index];
    // Direction of the segment at the tip: for the end arrowhead it's the
    // incoming direction, for the start arrowhead it's the reverse of the
    // outgoing segment (both are "tip minus its neighbor"). This is correct
    // for BOTH straight and curved lines: roughr's `curve` duplicates the
    // first/last points, which clamps the spline's end tangents to the
    // first/last segment - so the curve's tip tangent IS the last segment
    // direction, not some averaged neighbor direction.
    let neighbor = if tip_index == 0 {
        points[1.min(points.len() - 1)]
    } else {
        points[tip_index - 1]
    };
    let dir = tip - neighbor;
    let len = (dir.x * dir.x + dir.y * dir.y).sqrt();
    if len < 1e-6 {
        return;
    }
    let ux = dir.x / len;
    let uy = dir.y / len;
    let angle = 30f64.to_radians();
    for sign in [1.0, -1.0] {
        let a = angle * sign;
        let (sin, cos) = a.sin_cos();
        // Rotate the *incoming* direction backwards by ±30°.
        let dx = ux * cos - uy * sin;
        let dy = ux * sin + uy * cos;
        let end = WPoint::new(tip.x - dx * head_len, tip.y - dy * head_len);
        push(&gen.line(tip.x, tip.y, end.x, end.y), out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::WBounds;

    #[test]
    fn generates_paths_for_all_shape_kinds() {
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let style = ElementStyle {
            background: Some(0xa5d8ff),
            ..Default::default()
        };
        let kinds = vec![
            Element::new(ElementKind::Rectangle, WBounds::new(0.0, 0.0, 100.0, 80.0), style.clone()),
            Element::new(ElementKind::Ellipse, WBounds::new(0.0, 0.0, 100.0, 80.0), style.clone()),
            Element::new(ElementKind::Diamond, WBounds::new(0.0, 0.0, 100.0, 80.0), style.clone()),
            Element::from_absolute_points(
                |points| ElementKind::Arrow {
                    points,
                    end_arrowhead: true,
                    start_arrowhead: false,
                },
                vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 60.0)],
                style.clone(),
            ),
            Element::from_absolute_points(
                |points| ElementKind::Freedraw { points },
                (0..20)
                    .map(|i| WPoint::new(i as f64 * 5.0, (i as f64 * 0.7).sin() * 10.0))
                    .collect(),
                style,
            ),
        ];
        for el in &kinds {
            let paths = paths_for_element(el, &camera, origin);
            assert!(
                !paths.is_empty(),
                "element {:?} produced no paths",
                std::mem::discriminant(&el.kind)
            );
        }
    }

    #[test]
    fn deterministic_with_same_seed() {
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let mut el = Element::new(
            ElementKind::Rectangle,
            WBounds::new(0.0, 0.0, 100.0, 80.0),
            ElementStyle::default(),
        );
        el.seed = 42;
        let a = paths_for_element(&el, &camera, origin);
        let b = paths_for_element(&el, &camera, origin);
        assert_eq!(a.len(), b.len(), "same seed must give same path count");
    }

    #[test]
    fn curved_line_type_renders_for_lines_and_arrows() {
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let pts = || {
            vec![
                WPoint::new(0.0, 0.0),
                WPoint::new(50.0, 40.0),
                WPoint::new(100.0, 0.0),
            ]
        };
        let mut style = ElementStyle::default();
        style.line_type = LineType::Curved;
        let line = Element::from_absolute_points(
            |points| ElementKind::Line { points },
            pts(),
            style.clone(),
        );
        assert!(!paths_for_element(&line, &camera, origin).is_empty());
        // A curved arrow also renders both arrowheads (end + start).
        let arrow = Element::from_absolute_points(
            |points| ElementKind::Arrow {
                points,
                end_arrowhead: true,
                start_arrowhead: true,
            },
            pts(),
            style,
        );
        // stroke path + 2 heads × 2 lines each.
        assert!(paths_for_element(&arrow, &camera, origin).len() >= 5);
    }

    #[test]
    fn straight_is_the_default_line_type() {
        // Old scene files (no line_type field) and fresh styles are straight.
        assert_eq!(ElementStyle::default().line_type, LineType::Straight);
        let style: ElementStyle = serde_json::from_str(
            r#"{"stroke":0,"background":null,"stroke_width":2.0,"roughness":1.0,"stroke_style":"solid","opacity":1.0}"#,
        )
        .unwrap();
        assert_eq!(style.line_type, LineType::Straight);
    }
}
