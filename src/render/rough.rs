//! Converts scene elements into GPUI paths with a hand-drawn look,
//! powered by rough.js' Rust port (roughr / rough_piet).

use gpui::{px, FillOptions, Hsla, Path, PathBuilder, PathStyle, Pixels, Point};
use rough_piet::KurboGenerator;
use roughr::Srgba;

use crate::camera::Camera;
use crate::scene::{
    curve_samples, diamond_polygon, Element, ElementKind, ElementStyle,
    FillStyle as SceneFillStyle, LineType, StrokeStyle, WPoint,
};
use roughr::core::{FillStyle, LineCap, LineJoin, OpSetType, Options};

/// One tessellated path ready to paint.
pub struct ReadyPath {
    pub path: Path<Pixels>,
    pub color: Hsla,
}

/// One roughr op set, decoupled from rough_piet's drawable type so the
/// geometry cache can store them without lifetime entanglement.
#[derive(Debug)]
pub struct RoughOpSet {
    pub op_set_type: OpSetType,
    pub ops: kurbo::BezPath,
    /// Per-opset paint override (world units). None = derive color/width
    /// from the element style by op type (the classic single-style look).
    /// Watercolor layers and edge pooling set this.
    pub paint: Option<PaintOverride>,
}

/// Paint override for one op set. `color` replaces the type-derived color;
/// `width` replaces the type-derived stroke width (world units, camera
/// scaled at paint time). Both optional so an override can tweak one.
#[derive(Debug, Clone, Copy)]
pub struct PaintOverride {
    pub color: Option<Hsla>,
    pub width: Option<f64>,
}

/// Collect the op sets of a roughr drawable into cache-friendly form.
fn sets_of(drawable: &rough_piet::KurboDrawable<f64>) -> Vec<RoughOpSet> {
    drawable
        .sets
        .iter()
        .map(|s| RoughOpSet {
            op_set_type: s.op_set_type.clone(),
            ops: s.ops.clone(),
            paint: None,
        })
        .collect()
}

/// World-space render geometry: the expensive, camera-independent part of an
/// element's paint (roughr seeded generation, spline sampling, ribbon/dot
/// outlines). Deterministic per (seed, style, geometry), so it caches cleanly
/// across pans; the per-frame stage ([`paint_world_geom`]) only transforms to
/// screen space and tessellates.
#[derive(Debug)]
pub enum WorldGeom {
    /// Nothing to paint (degenerate/empty element).
    Empty,
    /// roughr op sets in paint order (fills first, then strokes): shapes,
    /// lines, arrows.
    Rough(Vec<RoughOpSet>),
    /// Legacy freedraw: jittered sample polylines, one per rough pass.
    Sampled(Vec<Vec<WPoint>>),
    /// Ink freedraw: closed world-space outline polygon (ribbon or dot).
    Outline(Vec<WPoint>),
}

/// Convert a 0xRRGGBB color + opacity into an Hsla.
pub fn color_u32(rgb: u32, opacity: f32) -> Hsla {
    let r = ((rgb >> 16) & 0xff) as f32 / 255.0;
    let g = ((rgb >> 8) & 0xff) as f32 / 255.0;
    let b = (rgb & 0xff) as f32 / 255.0;
    gpui::Rgba {
        r,
        g,
        b,
        a: opacity,
    }
    .into()
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
        // gpui 0.2.2 在 Windows 上 PathBuilder::fill() 的产物不可见（最小
        // 复现工程 fill_repro 证实：同窗口 quad 正常、fill 路径消失）。因此
        // 所有填充统一走已被证明可靠的排线（FillSketch→stroke）管线。
        // 间距/线宽参数统一由 fill_params 决定；注意 fill_weight 必须在
        // 绘制阶段以 PaintOverride.width 回传（见 paint_world_geom）——
        // roughr 生成期不会把线宽烘焙进几何。
        let (gap, weight) = fill_params(style);
        options.fill_style = Some(FillStyle::Hachure);
        options.fill_weight = Some(weight);
        options.hachure_gap = Some(gap);
        if let Some(angle) = style.hachure_angle {
            options.set_hachure_angle(Some(angle as f32));
        }
        options.disable_multi_stroke_fill = Some(true);
    }
    options
}

/// Fill hachure parameters per style: (spacing, stroke width) in world
/// units. Density ordering: Hachure < Dense < Solid; Watercolor's base
/// wash uses Dense-like coverage (its layer passes override these).
fn fill_params(style: &ElementStyle) -> (f32, f32) {
    let sw = style.stroke_width as f32;
    let (gap, weight) = match style.fill_style {
        SceneFillStyle::Hachure => ((sw * 4.0).max(4.0), (sw * 0.5).max(0.5)),
        // 密：线宽 ≈ 0.7× 间距 —— 排线明显更密，但保留白色呼吸感
        // （coverage ~0.7，介于纹 0.125 与实 3.5 之间）。
        SceneFillStyle::Dense => {
            let gap = (sw * 1.6).max(1.6);
            (gap, gap * 0.7)
        }
        // 实：3.5× 深度重叠，近乎纯色。
        SceneFillStyle::Solid => {
            let gap = (sw * 0.5).max(0.6);
            (gap, gap * 3.5)
        }
        SceneFillStyle::Watercolor => {
            let gap = (sw * 1.1).max(1.2);
            (gap, gap * 1.7)
        }
    };
    // Agent 级细粒度参数逐项覆盖预设派生值（字段为 f64 世界单位）。
    (
        style.hachure_gap.map_or(gap, |v| v as f32),
        style.fill_weight.map_or(weight, |v| v as f32),
    )
}

/// Scale a Bézier path around `center` by `k`, then offset by (dx, dy) —
/// the shadow tucks behind the shape and pokes out only on the offset side.
fn shadow_transform(
    bez: &kurbo::BezPath,
    center: WPoint,
    dx: f64,
    dy: f64,
    k: f64,
) -> kurbo::BezPath {
    bez.iter()
        .map(|el| match el {
            kurbo::PathEl::MoveTo(p) => kurbo::PathEl::MoveTo(kurbo::Point::new(
                center.x + (p.x - center.x) * k + dx,
                center.y + (p.y - center.y) * k + dy,
            )),
            kurbo::PathEl::LineTo(p) => kurbo::PathEl::LineTo(kurbo::Point::new(
                center.x + (p.x - center.x) * k + dx,
                center.y + (p.y - center.y) * k + dy,
            )),
            kurbo::PathEl::QuadTo(c, p) => kurbo::PathEl::QuadTo(
                kurbo::Point::new(
                    center.x + (c.x - center.x) * k + dx,
                    center.y + (c.y - center.y) * k + dy,
                ),
                kurbo::Point::new(
                    center.x + (p.x - center.x) * k + dx,
                    center.y + (p.y - center.y) * k + dy,
                ),
            ),
            kurbo::PathEl::CurveTo(c1, c2, p) => kurbo::PathEl::CurveTo(
                kurbo::Point::new(
                    center.x + (c1.x - center.x) * k + dx,
                    center.y + (c1.y - center.y) * k + dy,
                ),
                kurbo::Point::new(
                    center.x + (c2.x - center.x) * k + dx,
                    center.y + (c2.y - center.y) * k + dy,
                ),
                kurbo::Point::new(
                    center.x + (p.x - center.x) * k + dx,
                    center.y + (p.y - center.y) * k + dy,
                ),
            ),
            other => other.clone(),
        })
        .collect()
}

/// Translate every point of a Bézier path by (dx, dy).
fn translate_bez(bez: &kurbo::BezPath, dx: f64, dy: f64) -> kurbo::BezPath {
    bez.iter()
        .map(|el| match el {
            kurbo::PathEl::MoveTo(p) => {
                kurbo::PathEl::MoveTo(kurbo::Point::new(p.x + dx, p.y + dy))
            }
            kurbo::PathEl::LineTo(p) => {
                kurbo::PathEl::LineTo(kurbo::Point::new(p.x + dx, p.y + dy))
            }
            kurbo::PathEl::QuadTo(c, p) => kurbo::PathEl::QuadTo(
                kurbo::Point::new(c.x + dx, c.y + dy),
                kurbo::Point::new(p.x + dx, p.y + dy),
            ),
            kurbo::PathEl::CurveTo(c1, c2, p) => kurbo::PathEl::CurveTo(
                kurbo::Point::new(c1.x + dx, c1.y + dy),
                kurbo::Point::new(c2.x + dx, c2.y + dy),
                kurbo::Point::new(p.x + dx, p.y + dy),
            ),
            other => other.clone(),
        })
        .collect()
}

/// Closed Catmull-Rom spline through `pts` as a cubic Bézier path — the
/// smooth organic outline behind the AI's blob/curve shapes.
fn closed_catmull_rom_bez(pts: &[WPoint]) -> kurbo::BezPath {
    let n = pts.len();
    let mut bez = kurbo::BezPath::new();
    let at = |i: usize| {
        let p = pts[i % n];
        kurbo::Point::new(p.x, p.y)
    };
    bez.move_to(at(0));
    for i in 0..n {
        let p0 = at((i + n - 1) % n);
        let p1 = at(i);
        let p2 = at((i + 1) % n);
        let p3 = at((i + 2) % n);
        let c1 = kurbo::Point::new(p1.x + (p2.x - p0.x) / 6.0, p1.y + (p2.y - p0.y) / 6.0);
        let c2 = kurbo::Point::new(p2.x - (p3.x - p1.x) / 6.0, p2.y - (p3.y - p1.y) / 6.0);
        bez.curve_to(c1, c2, p2);
    }
    bez.close_path();
    bez
}

/// Per-channel darkening of a 0xRRGGBB color (watercolor edge pooling).
fn darken(rgb: u32, factor: f32) -> u32 {
    let r = (((rgb >> 16) & 0xff) as f32 * factor).round() as u32;
    let g = (((rgb >> 8) & 0xff) as f32 * factor).round() as u32;
    let b = ((rgb & 0xff) as f32 * factor).round() as u32;
    (r << 16) | (g << 8) | b
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
/// Returned in paint order: fills first, then strokes. Composition of
/// [`world_geometry`] + [`paint_world_geom`]; the board's render cache calls
/// the two stages separately so the geometry stage is skipped on hit.
pub fn paths_for_element(
    el: &Element,
    camera: &Camera,
    canvas_origin: Point<Pixels>,
) -> Vec<ReadyPath> {
    paint_world_geom(&world_geometry(el), el, camera, canvas_origin)
}

/// Compute the world-space geometry of one element. Pure and deterministic —
/// the cache key is the element fingerprint, so this runs only when the
/// element (or its style/seed) actually changed.
/// Build the rough geometry for a closed shape. When the style is the
/// near-solid 实 fill, a second cross-hatch pass (perpendicular hachure
/// angle) is merged in: single-pass hand-drawn hachure always has jittered
/// white seams, and only a crossing pass covers them reliably.
fn rough_shape(
    style: &ElementStyle,
    seed: u64,
    center: WPoint,
    draw: impl Fn(&KurboGenerator) -> rough_piet::KurboDrawable<f64>,
) -> WorldGeom {
    let fill_on = style.background.is_some();
    let watercolor = fill_on && style.fill_style == SceneFillStyle::Watercolor;
    let sw = style.stroke_width as f32;

    // Base pass: outline + the style's fill — except watercolor, whose fill
    // comes entirely from the layered washes below (stroke-only here).
    let base_opts = if watercolor {
        let mut o = options_for(style, seed, false);
        o.fill = None;
        o
    } else {
        options_for(style, seed, false)
    };
    let gen = KurboGenerator::new(base_opts);
    let mut sets = sets_of(&draw(&gen));

    // Soft shadow: two stacked, offset copies of the shape shrunk slightly
    // around its center (so the shading tucks behind the outline instead of
    // spiking out) with jitter disabled — clean lines read as depth, jittery
    // ones read as scribbles. Fake gaussian: no blur in this pipeline.
    if let Some(sh) = &style.shadow {
        for (mult, alpha) in [(0.5, 0.09), (1.0, 0.13)] {
            let mut o = options_for(style, seed.wrapping_add(61), false);
            o.fill = Some(srgba(0x1e1e1e, alpha));
            o.stroke = None;
            o.roughness = Some(0.0);
            o.bowing = Some(0.0);
            o.fill_weight = Some((sw * 0.8).max(0.7));
            o.hachure_gap = Some((sw * 1.1).max(1.0));
            let gen_sh = KurboGenerator::new(o);
            let mut sh_sets = sets_of(&draw(&gen_sh));
            let k = 1.0 - 0.06 * mult; // 6% shrink per stack step
            for s in &mut sh_sets {
                s.ops = shadow_transform(
                    &s.ops,
                    center,
                    sh.dx * mult as f64,
                    sh.dy * mult as f64,
                    k,
                );
                s.paint = Some(PaintOverride {
                    color: Some(color_u32(0x1e1e1e, alpha)),
                    width: Some((sw * 0.8).max(0.7) as f64),
                });
            }
            sets.splice(0..0, sh_sets);
        }
    }

    if fill_on {
        let (_, weight) = fill_params(style);
        // Honor the generated fill stroke width at paint time — the paint
        // stage otherwise falls back to a fixed 线宽×0.5 for every op set,
        // which silently thinned the denser styles.
        for set in &mut sets {
            if set.op_set_type == OpSetType::FillSketch {
                set.paint = Some(PaintOverride {
                    color: None,
                    width: Some(weight as f64),
                });
            }
        }

        match style.fill_style {
            // 实心：交叉第二遍（垂直角度、错开 seed）盖住单遍排线的抖动
            // 缝隙 —— 单遍手绘线无论多重都会局部露白。
            SceneFillStyle::Solid => {
                let mut cross = options_for(style, seed.wrapping_add(1), false);
                cross.set_hachure_angle(Some(49.0));
                let gen2 = KurboGenerator::new(cross);
                sets.extend(
                    sets_of(&draw(&gen2))
                        .into_iter()
                        .filter(|s| s.op_set_type == OpSetType::FillSketch),
                );
            }
            // 水彩：三层不同角度/透明度的淡彩晕染，再加两道逐层加宽、
            // 逐层变淡的边缘描边（颜色取填充色加深）—— 颜料在纸缘沉积。
            SceneFillStyle::Watercolor => {
                let bg = style.background.unwrap_or(0);
                let op = style.opacity;
                let base_gap = style
                    .hachure_gap
                    .map_or((sw * 1.1).max(1.2) as f64, |v| v);
                let washes = [
                    (-41.0, (base_gap * 1.2).max(1.4), 1.7, 0.60, 7u64),
                    (49.0, (base_gap * 0.8).max(1.0), 1.5, 0.45, 14),
                    (-8.0, (base_gap * 1.7).max(2.0), 1.2, 0.32, 21),
                ];
                for (angle, gap, wf, af, so) in washes {
                    let gap = gap as f32;
                    let mut o = options_for(style, seed.wrapping_add(so), false);
                    o.fill = Some(srgba(bg, op * af));
                    o.fill_weight = Some(gap * wf);
                    o.hachure_gap = Some(gap);
                    o.set_hachure_angle(Some(angle));
                    let gen2 = KurboGenerator::new(o);
                    sets.extend(
                        sets_of(&draw(&gen2))
                            .into_iter()
                            .filter(|s| s.op_set_type == OpSetType::FillSketch)
                            .map(|mut s| {
                                s.paint = Some(PaintOverride {
                                    color: Some(color_u32(bg, op * af)),
                                    width: Some((gap * wf) as f64),
                                });
                                s
                            }),
                    );
                }
                let edges = [(1.8, 0.72, 0.30, 28u64), (2.6, 0.55, 0.16, 35)];
                for (wf, df, af, so) in edges {
                    let mut o = options_for(style, seed.wrapping_add(so), false);
                    o.fill = None;
                    o.stroke = Some(srgba(darken(bg, df), op * af));
                    o.stroke_width = Some(sw * wf);
                    let gen2 = KurboGenerator::new(o);
                    sets.extend(
                        sets_of(&draw(&gen2))
                            .into_iter()
                            .filter(|s| s.op_set_type == OpSetType::Path)
                            .map(|mut s| {
                                s.paint = Some(PaintOverride {
                                    color: Some(color_u32(darken(bg, df), op * af)),
                                    width: Some((sw * wf) as f64),
                                });
                                s
                            }),
                    );
                }
            }
            _ => {}
        }
    }
    WorldGeom::Rough(sets)
}

pub fn world_geometry(el: &Element) -> WorldGeom {
    let style = &el.style;
    let b = &el.bounds;
    match &el.kind {
        ElementKind::Rectangle => {
            if b.w < 0.01 || b.h < 0.01 {
                return WorldGeom::Empty;
            }
            rough_shape(style, el.seed, b.center(), |gen| {
                gen.rectangle(b.x, b.y, b.w, b.h)
            })
        }
        ElementKind::Ellipse => {
            if b.w < 0.01 || b.h < 0.01 {
                return WorldGeom::Empty;
            }
            // roughr's ellipse treats (x, y) as the CENTER and width/height as
            // diameters, but our bounds.x/y is the top-left corner. Translate.
            let center = b.center();
            rough_shape(style, el.seed, center, |gen| {
                gen.ellipse(center.x, center.y, b.w, b.h)
            })
        }
        ElementKind::Diamond => {
            if b.w < 0.01 || b.h < 0.01 {
                return WorldGeom::Empty;
            }
            let points: Vec<_> = diamond_polygon(b).iter().map(|p| to_euclid(*p)).collect();
            rough_shape(style, el.seed, b.center(), |gen: &KurboGenerator| {
                gen.polygon(&points)
            })
        }
        // 封闭多边形（水墨山形/岸块等不规则形状）：roughr polygon，
        // 填充走 options_for 的统一管线（solid → 密排线 FillSketch）。
        // smooth = 平滑闭合样条（AI 的有机形态：花瓣/云朵/鹅卵石）。
        ElementKind::Polygon { smooth, .. } => {
            let points = el.absolute_points();
            if points.len() < 3 {
                return WorldGeom::Empty;
            }
            let pts: Vec<_> = points.iter().map(|p| to_euclid(*p)).collect();
            if *smooth {
                let bez = closed_catmull_rom_bez(&points);
                rough_shape(style, el.seed, b.center(), |gen| {
                    gen.bez_path(bez.clone())
                })
            } else {
                rough_shape(style, el.seed, b.center(), |gen: &KurboGenerator| {
                gen.polygon(&pts)
            })
            }
        }
        // Variable-width ink stroke (from the crate::ink pipeline): fill the
        // ribbon outline instead of stroking a centerline. This arm must sit
        // before the generic point-based arm below, which keeps legacy
        // uniform strokes (empty widths) on the original rough path.
        ElementKind::Freedraw { .. } if !el.ink_widths().is_empty() => {
            let points = el.absolute_points();
            if points.len() < 2 {
                return dot_geometry(el, &points);
            }
            // Widths travel as ratios of the base stroke width; convert to
            // world units here so the geometry stays style-independent.
            let widths: Vec<f64> = el
                .ink_widths()
                .iter()
                .map(|ratio| style.stroke_width * ratio)
                .collect();
            WorldGeom::Outline(crate::ink::ribbon_outline(&points, &widths))
        }
        ElementKind::Line { .. } | ElementKind::Arrow { .. } | ElementKind::Freedraw { .. } => {
            let points = el.absolute_points();
            if points.len() == 1 {
                // A pen tap without movement used to be discarded; it now
                // produces a round ink dot (Excalidraw behavior).
                return dot_geometry(el, &points);
            }
            if points.len() < 2 {
                return WorldGeom::Empty;
            }
            let is_freedraw = matches!(el.kind, ElementKind::Freedraw { .. });
            // Lines/arrows render as straight polylines by default; the
            // "curved" line type fits a smooth curve through the points
            // (Excalidraw). Freedraw strokes are always smoothed.
            let curved = !is_freedraw && style.line_type == LineType::Curved;

            if is_freedraw {
                // Freedraw bypasses roughr entirely: sample a deterministic
                // Catmull-Rom spline through the pointer points and stroke it
                // directly. A committed stroke never moves across re-renders.
                //
                // Roughness presets, all fully deterministic (sin/cos of
                // point position + seed, so the same seed always gives the
                // same offset):
                //   r0 → single smooth stroke, no offset (architect).
                //   r1 → single stroke with a smooth wave offset (delicate).
                //   r2 → double stroke: two slightly-offset wavy strokes,
                //        mimicking rough.js's two-pass hand-drawn look.
                let roughness = style.roughness.clamp(0.0, 3.0) as f64;
                let seed_f = el.seed as f64;
                let samples = curve_samples(&points, 12);
                let amp = roughness * 3.0;
                let n_passes = if roughness >= 2.0 { 2 } else { 1 };
                let mut passes = Vec::with_capacity(n_passes);
                for pass in 0..n_passes {
                    let pass_f = pass as f64;
                    let offset_fn = |p: &WPoint| {
                        if roughness < 0.01 {
                            *p
                        } else {
                            // Each pass uses a different phase + a constant
                            // shift so the two strokes diverge instead of
                            // overlapping into a single blob.
                            let phase = p.x * 0.05 + seed_f * 0.1 + pass_f * 1.7;
                            let shift = pass_f * roughness * 2.0;
                            let dx = amp * phase.sin() + shift * 0.5;
                            let dy = amp * (phase * 1.3 + 1.5).cos() + shift * 0.5;
                            WPoint::new(p.x + dx, p.y + dy)
                        }
                    };
                    passes.push(samples.iter().map(offset_fn).collect());
                }
                WorldGeom::Sampled(passes)
            } else {
                let gen = KurboGenerator::new(options_for(style, el.seed, false));
                let euclid_points: Vec<_> = points.iter().map(|p| to_euclid(*p)).collect();
                let mut sets = if curved {
                    sets_of(&gen.curve(&euclid_points))
                } else {
                    sets_of(&gen.linear_path(&euclid_points, false))
                };
                if let ElementKind::Arrow {
                    end_arrowhead,
                    start_arrowhead,
                    ..
                } = &el.kind
                {
                    let head_len = (16.0 + style.stroke_width * 3.0).max(20.0);
                    if *end_arrowhead && points.len() >= 2 {
                        push_arrowhead(&gen, &points, points.len() - 1, head_len, &mut sets);
                    }
                    if *start_arrowhead && points.len() >= 2 {
                        push_arrowhead(&gen, &points, 0, head_len, &mut sets);
                    }
                }
                WorldGeom::Rough(sets)
            }
        }
        ElementKind::Text { .. } => {
            // Text is shaped (via the text-cache) and painted separately.
            WorldGeom::Empty
        }
    }
}

/// Geometry for a single-point stroke: a round ink dot whose diameter is the
/// (pressure-scaled) stroke width. Used for pen taps.
fn dot_geometry(el: &Element, points: &[WPoint]) -> WorldGeom {
    let ratio = el.ink_widths().first().copied().unwrap_or(1.0);
    let diameter = (el.style.stroke_width * ratio).max(0.5);
    WorldGeom::Outline(crate::ink::dot_outline(points[0], diameter))
}

/// Transform cached world geometry into paintable screen-space paths for the
/// current camera. This is the only per-frame work: flatten (zoom-dependent
/// density), world_to_screen, and lyon tessellation.
pub fn paint_world_geom(
    geom: &WorldGeom,
    el: &Element,
    camera: &Camera,
    canvas_origin: Point<Pixels>,
) -> Vec<ReadyPath> {
    let style = &el.style;
    let mut out: Vec<ReadyPath> = Vec::new();

    let stroke_color = color_u32(style.stroke, style.opacity);
    let fill_color = style
        .background
        .map(|c| color_u32(c, style.opacity))
        .unwrap_or(stroke_color);

    let stroke_width_px = camera.scale(style.stroke_width).max(px(0.5));
    let fill_weight_px = camera.scale(style.stroke_width * 0.5).max(px(0.5));
    let dashed = style.stroke_style == StrokeStyle::Dashed;

    match geom {
        WorldGeom::Empty => {}
        WorldGeom::Rough(sets) => {
            for set in sets {
                match set.op_set_type {
                    OpSetType::FillPath => {
                        let mut builder = PathBuilder::fill();
                        append_bez_path(&mut builder, &set.ops, camera, canvas_origin);
                        match builder.build() {
                            Ok(path) => out.push(ReadyPath {
                                path,
                                color: fill_color,
                            }),
                            Err(e) => eprintln!(
                                "[render] FillPath tessellation failed ({}x{}): {e}",
                                el.id.to_string().get(..8).unwrap_or("?"),
                                el.bounds.w
                            ),
                        }
                    }
                    OpSetType::FillSketch => {
                        let mut width_px = fill_weight_px;
                        let mut color = fill_color;
                        if let Some(p) = &set.paint {
                            if let Some(w) = p.width {
                                width_px = camera.scale(w).max(px(0.5));
                            }
                            if let Some(c) = p.color {
                                color = c;
                            }
                        }
                        let mut builder = PathBuilder::stroke(width_px);
                        append_bez_path(&mut builder, &set.ops, camera, canvas_origin);
                        if let Ok(path) = builder.build() {
                            out.push(ReadyPath { path, color });
                        }
                    }
                    OpSetType::Path => {
                        let mut width_px = stroke_width_px;
                        let mut color = stroke_color;
                        if let Some(p) = &set.paint {
                            if let Some(w) = p.width {
                                width_px = camera.scale(w).max(px(0.5));
                            }
                            if let Some(c) = p.color {
                                color = c;
                            }
                        }
                        let mut builder = PathBuilder::stroke(width_px);
                        if dashed {
                            let dash = camera.scale(12.0);
                            let gap = camera.scale(8.0);
                            builder = builder.dash_array(&[dash, gap]);
                        }
                        append_bez_path(&mut builder, &set.ops, camera, canvas_origin);
                        if let Ok(path) = builder.build() {
                            out.push(ReadyPath { path, color });
                        }
                    }
                }
            }
        }
        WorldGeom::Sampled(passes) => {
            for pass in passes {
                let Some(first) = pass.first() else {
                    continue;
                };
                let mut builder = PathBuilder::stroke(stroke_width_px);
                builder.move_to(camera.world_to_screen(*first, canvas_origin));
                for p in pass.iter().skip(1) {
                    builder.line_to(camera.world_to_screen(*p, canvas_origin));
                }
                if let Ok(path) = builder.build() {
                    out.push(ReadyPath {
                        path,
                        color: stroke_color,
                    });
                }
            }
        }
        WorldGeom::Outline(outline) => {
            if outline.len() < 2 {
                return out;
            }
            // NonZero fill: the ribbon can self-intersect at sharp turns and
            // an EvenOdd rule would punch holes there.
            let mut builder =
                PathBuilder::fill().with_style(PathStyle::Fill(FillOptions::non_zero()));
            for (i, wp) in outline.iter().enumerate() {
                let sp = camera.world_to_screen(*wp, canvas_origin);
                if i == 0 {
                    builder.move_to(sp);
                } else {
                    builder.line_to(sp);
                }
            }
            builder.close();
            if let Ok(path) = builder.build() {
                out.push(ReadyPath {
                    path,
                    color: stroke_color,
                });
            }
        }
    }

    out
}

fn push_arrowhead(
    gen: &KurboGenerator,
    points: &[WPoint],
    tip_index: usize,
    head_len: f64,
    out: &mut Vec<RoughOpSet>,
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
        out.extend(sets_of(&gen.line(tip.x, tip.y, end.x, end.y)));
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
            Element::new(
                ElementKind::Rectangle,
                WBounds::new(0.0, 0.0, 100.0, 80.0),
                style.clone(),
            ),
            Element::new(
                ElementKind::Ellipse,
                WBounds::new(0.0, 0.0, 100.0, 80.0),
                style.clone(),
            ),
            Element::new(
                ElementKind::Diamond,
                WBounds::new(0.0, 0.0, 100.0, 80.0),
                style.clone(),
            ),
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
                |points| ElementKind::Freedraw {
                    points,
                    widths: Vec::new(),
                },
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

    fn horizontal_ink_stroke(widths: Vec<f64>) -> Element {
        let mut style = ElementStyle::default();
        style.stroke_width = 4.0;
        let mut el = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![WPoint::new(10.0, 0.0), WPoint::new(100.0, 0.0)],
            style,
        );
        if let ElementKind::Freedraw { widths: w, .. } = &mut el.kind {
            *w = widths;
        }
        el
    }

    /// Screen-space outline of an element's cached geometry — the test-side
    /// equivalent of the old ink_outline_screen helper (gpui's tessellated
    /// `Path` exposes no public geometry, so tests assert on this instead).
    fn screen_outline(
        el: &Element,
        camera: &Camera,
        canvas_origin: Point<Pixels>,
    ) -> Vec<Point<Pixels>> {
        match world_geometry(el) {
            WorldGeom::Outline(pts) => pts
                .iter()
                .map(|wp| camera.world_to_screen(*wp, canvas_origin))
                .collect(),
            other => panic!("expected Outline geometry, got {other:?}"),
        }
    }

    #[test]
    fn ink_stroke_fills_the_ribbon_outline() {
        // A tapered horizontal stroke (widths 4 → 2 world units) must render
        // as exactly one fill path whose outline covers the ribbon extents:
        // half the max width above/below the centerline, half the end widths
        // past the tips.
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let el = horizontal_ink_stroke(vec![1.0, 0.5]);
        let paths = paths_for_element(&el, &camera, origin);
        assert_eq!(paths.len(), 1, "ink stroke renders as one fill path");

        let outline = screen_outline(&el, &camera, origin);
        let (mut x0, mut y0, mut x1, mut y1) = (
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        );
        for p in &outline {
            x0 = x0.min(f32::from(p.x));
            y0 = y0.min(f32::from(p.y));
            x1 = x1.max(f32::from(p.x));
            y1 = y1.max(f32::from(p.y));
        }
        assert!((x0 - 8.0).abs() < 1e-3, "cap back {x0}");
        assert!((x1 - 101.0).abs() < 1e-3, "cap tip {x1}");
        assert!((y0 + 2.0).abs() < 1e-3, "top {y0}");
        assert!((y1 - 2.0).abs() < 1e-3, "bottom {y1}");
    }

    #[test]
    fn ink_stroke_rendering_is_deterministic() {
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let el = horizontal_ink_stroke(vec![0.6, 1.0, 0.4]);
        let a = screen_outline(&el, &camera, origin);
        let b = screen_outline(&el, &camera, origin);
        assert_eq!(a, b);
        assert_eq!(paths_for_element(&el, &camera, origin).len(), 1);
    }

    #[test]
    fn legacy_uniform_freedraw_keeps_the_rough_path() {
        // Empty widths = legacy stroke: still renders (rough stroked line),
        // never through the ink branch.
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let el = horizontal_ink_stroke(Vec::new());
        assert!(!paths_for_element(&el, &camera, origin).is_empty());
    }

    #[test]
    fn single_point_freedraw_renders_a_pressure_scaled_dot() {
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let mut el = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![WPoint::new(10.0, 0.0)],
            {
                let mut s = ElementStyle::default();
                s.stroke_width = 4.0;
                s
            },
        );
        if let ElementKind::Freedraw { widths, .. } = &mut el.kind {
            *widths = vec![0.5]; // ratio 0.5 × base 4 → Ø2 dot
        }
        let paths = paths_for_element(&el, &camera, origin);
        assert_eq!(paths.len(), 1, "a pen tap renders one dot fill");

        let outline = screen_outline(&el, &camera, origin);
        let (x0, y0, x1, y1) = (
            outline
                .iter()
                .map(|p| f32::from(p.x))
                .fold(f32::INFINITY, f32::min),
            outline
                .iter()
                .map(|p| f32::from(p.y))
                .fold(f32::INFINITY, f32::min),
            outline
                .iter()
                .map(|p| f32::from(p.x))
                .fold(f32::NEG_INFINITY, f32::max),
            outline
                .iter()
                .map(|p| f32::from(p.y))
                .fold(f32::NEG_INFINITY, f32::max),
        );
        // Center (10, 0), radius 1.
        assert!((x0 - 9.0).abs() < 1e-6 && (x1 - 11.0).abs() < 1e-6);
        assert!((y0 + 1.0).abs() < 1e-6 && (y1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn single_point_legacy_freedraw_renders_a_base_width_dot() {
        // No widths (taper off): the dot uses the base stroke width.
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let el = horizontal_ink_stroke(Vec::new());
        let mut one = Element::from_absolute_points(
            |points| ElementKind::Freedraw {
                points,
                widths: Vec::new(),
            },
            vec![WPoint::new(50.0, 40.0)],
            {
                let mut s = ElementStyle::default();
                s.stroke_width = 2.0;
                s
            },
        );
        one.seed = el.seed;
        match world_geometry(&one) {
            WorldGeom::Outline(pts) => {
                let max_r = pts
                    .iter()
                    .map(|p| (p.x - 50.0).hypot(p.y - 40.0))
                    .fold(0.0f64, f64::max);
                assert!(
                    (max_r - 1.0).abs() < 1e-9,
                    "radius {max_r} ≠ base width / 2"
                );
            }
            other => panic!("expected Outline, got {other:?}"),
        }
        assert_eq!(paths_for_element(&one, &camera, origin).len(), 1);
    }

    #[test]
    fn paths_for_element_composes_geometry_and_paint() {
        // The render cache relies on paths_for_element being exactly
        // paint_world_geom(world_geometry(el)) — keep the two call paths
        // from diverging.
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let mut style = ElementStyle::default();
        style.background = Some(0xa5d8ff);
        let mut dashed_line = ElementStyle::default();
        dashed_line.stroke_style = StrokeStyle::Dashed;
        let els = vec![
            Element::new(
                ElementKind::Rectangle,
                WBounds::new(0.0, 0.0, 100.0, 80.0),
                style.clone(),
            ),
            Element::new(
                ElementKind::Ellipse,
                WBounds::new(0.0, 0.0, 100.0, 80.0),
                style,
            ),
            Element::from_absolute_points(
                |points| ElementKind::Line { points },
                vec![WPoint::new(0.0, 0.0), WPoint::new(100.0, 0.0)],
                dashed_line,
            ),
            horizontal_ink_stroke(vec![0.6, 1.0, 0.4]),
        ];
        for el in &els {
            let direct = paths_for_element(el, &camera, origin);
            let staged = paint_world_geom(&world_geometry(el), el, &camera, origin);
            assert_eq!(direct.len(), staged.len());
        }
    }
}

#[cfg(test)]
mod solid_fill_tests {
    use super::*;
    use crate::scene::WBounds;

    #[test]
    fn solid_fill_ellipse_emits_fillpath() {
        let mut style = ElementStyle::default();
        style.background = Some(0x4A5560);
        style.fill_style = crate::scene::FillStyle::Solid;
        style.opacity = 0.5;
        let el = Element::new(
            ElementKind::Ellipse,
            WBounds::new(0.0, 0.0, 700.0, 240.0),
            style,
        );
        let geom = world_geometry(&el);
        match &geom {
            WorldGeom::Rough(sets) => {
                // gpui 0.2.2 Windows 上 FillPath（PathBuilder::fill）不可见，
                // 因此 Solid 统一走 FillSketch（密排线→视觉实心），见 options_for。
                // 近实心 = 基础密排 + 交叉第二遍，至少两道 FillSketch。
                let fill_sets: Vec<_> = sets
                    .iter()
                    .filter(|s| s.op_set_type == OpSetType::FillSketch)
                    .collect();
                assert!(
                    fill_sets.len() >= 2,
                    "solid fill must emit base + cross hachure passes, got {}",
                    fill_sets.len()
                );
                for set in &fill_sets {
                    assert!(!set.ops.elements().is_empty(), "FillSketch opset is EMPTY");
                }
            }
            other => panic!("expected Rough, got {other:?}"),
        }
        let camera = Camera::default();
        let origin = gpui::point(px(0.0), px(0.0));
        let paths = paths_for_element(&el, &camera, origin);
        assert!(
            paths.len() >= 3,
            "solid ellipse must paint base fill + cross fill + stroke, got {}",
            paths.len()
        );
    }
}
