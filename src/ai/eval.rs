//! Blackboard-poster evaluation: replays [`CanvasOp`]s into a lightweight
//! virtual canvas, then scores the result against the blackboard-poster
//! acceptance criteria (structure / layout / style / process).
//!
//! The replay mirrors `BoardView::apply_canvas_op` for the fields evaluation
//! needs (ids, kinds, bboxes, text, colors, font sizes). If you change how ops
//! are applied on the board, update [`replay`] to match. Pure functions, no
//! GPUI — unit tests cover every rule both ways.
//!
//! Id correlation: draw ops don't carry element ids — the board assigns them
//! and reports back in the tool result ("已添加… id=a1b2c3d4"). The harness
//! therefore pairs each op with the id extracted from its result
//! (`replay` takes `&[(CanvasOp, Option<String>)]`); updates/deletes match by
//! the same id-prefix rule the board uses.

use crate::ai::canvas_ops::CanvasOp;
use crate::ai::canvas_ops::CanvasStyle;
use serde::Serialize;

/// Visible canvas area the model is told about (world units).
const CANVAS_W: f64 = 1600.0;
const CANVAS_H: f64 = 1000.0;
/// Out-of-bounds tolerance: elements may poke slightly outside.
const BOUNDS_TOL: f64 = 20.0;
/// Tool-call budget per run.
const MAX_TOOL_CALLS: usize = 60;

/// One element on the virtual canvas after replaying ops.
#[derive(Clone, Debug, Serialize)]
pub struct VirtualElement {
    pub id: String,
    /// "rectangle" / "ellipse" / "diamond" / "line" / "arrow" / "text" /
    /// "label" (bound label text).
    pub kind: &'static str,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub text: Option<String>,
    pub font_size: f64,
    pub stroke: u32,
    pub fill: Option<u32>,
    /// Element opacity (0..1) — the ink-wash rubric grades ink density.
    pub opacity: f64,
    /// Real polyline geometry for point-based elements (lines/arrows and
    /// mind-map links), `None` for boxes. The mind-map rubric checks link
    /// crossings and node intrusions against actual segments, not bboxes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<[f64; 2]>>,
}

/// The virtual canvas state after replaying a run's ops.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VirtualCanvas {
    pub background: Option<u32>,
    pub elements: Vec<VirtualElement>,
    /// Slide pages created via `AddPage` (scene/pages.rs rects).
    pub pages: Vec<crate::scene::pages::Page>,
    pub ops_applied: usize,
    pub ops_failed: usize,
}

impl VirtualCanvas {
    pub fn texts(&self) -> impl Iterator<Item = &VirtualElement> {
        self.elements
            .iter()
            .filter(|e| e.kind == "text" || e.kind == "label")
    }

    /// Mirror of `BoardView::element_snapshot`: the agent-visible element list
    /// that feeds `list_elements` / update-delete id checks. The harness must
    /// refresh the shared snapshot with this after every applied op, otherwise
    /// the model is told the canvas is empty and redraws duplicates.
    pub fn snapshot(&self) -> Vec<crate::ai::tools::ElementSnapshot> {
        use crate::ai::tools::ElementSnapshot;
        self.elements
            .iter()
            .map(|e| ElementSnapshot {
                id: e.id.clone(),
                kind: if e.kind == "label" {
                    "text".to_string()
                } else {
                    e.kind.to_string()
                },
                text: e.text.clone(),
                x: e.x,
                y: e.y,
                w: e.w,
                h: e.h,
            })
            .collect()
    }
}

/// One acceptance check with a human-readable verdict.
#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// The full review: `passed` is true only when every check passes.
#[derive(Clone, Debug, Serialize)]
pub struct EvalReport {
    pub passed: bool,
    pub checks: Vec<Check>,
}

impl EvalReport {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "=== 评测报告：{} ===\n",
            if self.passed {
                "达标 ✓"
            } else {
                "未达标 ✗"
            }
        ));
        for c in &self.checks {
            out.push_str(&format!(
                "[{}] {} — {}\n",
                if c.passed { "PASS" } else { "FAIL" },
                c.name,
                c.detail
            ));
        }
        out
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// Rough text width: CJK/fullwidth chars ≈ 1.0 × font size, others ≈ 0.55.
fn line_width(line: &str, font_size: f64) -> f64 {
    line.chars()
        .map(|c| {
            if is_cjk(c) {
                font_size
            } else {
                font_size * 0.55
            }
        })
        .sum()
}

/// Estimated (x, y, w, h) for a text block, honoring wrap width.
fn text_bbox(
    x: f64,
    y: f64,
    text: &str,
    font_size: f64,
    wrap: Option<f64>,
) -> (f64, f64, f64, f64) {
    let natural: f64 = text
        .lines()
        .map(|l| line_width(l, font_size))
        .fold(0.0, f64::max);
    let w = match wrap {
        Some(wrap) if wrap > 0.0 && wrap < natural => wrap,
        _ => natural,
    };
    let total_lines: f64 = text
        .lines()
        .map(|l| {
            if wrap.is_some() && w < natural {
                (line_width(l, font_size) / w).ceil().max(1.0)
            } else {
                1.0
            }
        })
        .sum();
    let h = total_lines.max(1.0) * font_size * crate::scene::LINE_HEIGHT;
    (x, y, w.max(1.0), h.max(font_size))
}

/// Replay a run's ops into a virtual canvas. `ops` pairs each op with the
/// element id the board reported back for it (draw ops only; `None` for
/// background/clear). Ops that would fail on the real board (unknown id)
/// count into `ops_failed` and change nothing.
pub fn replay(ops: &[(CanvasOp, Option<String>)]) -> VirtualCanvas {
    let mut c = VirtualCanvas::default();
    for (op, assigned_id) in ops {
        let _ = apply(&mut c, op, assigned_id.as_deref());
    }
    c
}

fn id_matches(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix)
}

/// Apply one op to the virtual canvas, mirroring the board's semantics and
/// result messages. `Err` mirrors the board's rejection (counted in
/// `ops_failed`) so the harness can relay it back to the model.
pub fn apply(
    c: &mut VirtualCanvas,
    op: &CanvasOp,
    assigned_id: Option<&str>,
) -> Result<String, String> {
    let mut msg = String::new();
    match op {
        CanvasOp::SetBackground { color } => {
            c.background = *color;
            c.ops_applied += 1;
            msg = match color {
                Some(c) => format!("画布底色已设为 #{c:06x}"),
                None => "画布底色已恢复白色".to_string(),
            };
        }
        CanvasOp::Clear => {
            c.elements.clear();
            c.ops_applied += 1;
        }
        CanvasOp::DeleteElement { id } => {
            let Some(pos) = c.elements.iter().position(|e| id_matches(&e.id, id)) else {
                c.ops_failed += 1;
                return Err(format!("找不到元素 id={id}"));
            };
            let removed = c.elements.remove(pos);
            // The board removes a bound label with its container.
            let label_id = format!("{}-label", removed.id);
            c.elements.retain(|e| e.id != label_id);
            c.ops_applied += 1;
            msg = format!("已删除元素 id={}", &id[..id.len().min(8)]);
        }
        CanvasOp::UpdateElement {
            id,
            x,
            y,
            text,
            style,
            font_size,
        } => {
            let Some(e) = c.elements.iter_mut().find(|e| id_matches(&e.id, id)) else {
                c.ops_failed += 1;
                return Err(format!("找不到元素 id={id}"));
            };
            if let Some(nx) = x {
                e.x = *nx;
            }
            if let Some(ny) = y {
                e.y = *ny;
            }
            if let Some(s) = &style.stroke {
                e.stroke = *s;
            }
            if let Some(f) = &style.fill {
                e.fill = Some(*f);
            }
            let is_text = matches!(e.kind, "text" | "label");
            if is_text && text.is_some() {
                e.text = Some(text.clone().unwrap());
            }
            if is_text {
                if let Some(fs) = font_size {
                    e.font_size = *fs;
                }
                if text.is_some() || font_size.is_some() {
                    let (_, _, nw, nh) =
                        text_bbox(e.x, e.y, e.text.as_deref().unwrap_or(""), e.font_size, None);
                    e.w = nw;
                    e.h = nh;
                }
            }
            c.ops_applied += 1;
        }
        CanvasOp::Rectangle {
            x,
            y,
            w,
            h,
            style,
            text,
        } => {
            msg = push_shape(
                c,
                assigned_id,
                "rectangle",
                *x,
                *y,
                *w,
                *h,
                style,
                text.as_deref(),
            )?;
        }
        CanvasOp::Ellipse {
            x,
            y,
            w,
            h,
            style,
            text,
        } => {
            msg = push_shape(
                c,
                assigned_id,
                "ellipse",
                *x,
                *y,
                *w,
                *h,
                style,
                text.as_deref(),
            )?;
        }
        CanvasOp::Polygon { points, style } => {
            if points.len() < 3 || points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
                c.ops_failed += 1;
                return Err("多边形至少需要三个有限坐标点".to_string());
            }
            let id = assigned_id
                .map(str::to_string)
                .unwrap_or_else(|| format!("p{}", c.elements.len()));
            let min_x = points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
            let min_y = points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
            let max_x = points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);
            let max_y = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
            let id8 = id[..id.len().min(8)].to_string();
            c.elements.push(VirtualElement {
                id,
                kind: "polygon",
                x: min_x,
                y: min_y,
                w: (max_x - min_x).max(1.0),
                h: (max_y - min_y).max(1.0),
                text: None,
                font_size: 0.0,
                stroke: style.stroke.unwrap_or(0x1e1e1e),
                fill: style.fill,
                opacity: f64::from(style.opacity.unwrap_or(1.0)),
                points: Some(
                    points
                        .iter()
                        .map(|p| [p.x, p.y])
                        .collect(),
                ),
            });
            c.ops_applied += 1;
            msg = format!("已添加多边形 id={id8}");
        }
        CanvasOp::Diamond {
            x,
            y,
            w,
            h,
            style,
            text,
        } => {
            msg = push_shape(
                c,
                assigned_id,
                "diamond",
                *x,
                *y,
                *w,
                *h,
                style,
                text.as_deref(),
            )?;
        }
        CanvasOp::Line {
            points,
            style,
            text,
        }
        | CanvasOp::Arrow {
            points,
            style,
            text,
            ..
        } => {
            if points.len() < 2 {
                c.ops_failed += 1;
                return Err("连线至少需要两个点".to_string());
            }
            let kind = if matches!(op, CanvasOp::Line { .. }) {
                "line"
            } else {
                "arrow"
            };
            let min_x = points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
            let min_y = points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
            let max_x = points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);
            let max_y = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
            let id = assigned_id
                .map(str::to_string)
                .unwrap_or_else(|| format!("l{}", c.elements.len()));
            let id8 = id[..id.len().min(8)].to_string();
            c.elements.push(VirtualElement {
                id,
                kind,
                x: min_x,
                y: min_y,
                w: (max_x - min_x).max(1.0),
                h: (max_y - min_y).max(1.0),
                text: None,
                font_size: 0.0,
                stroke: style.stroke.unwrap_or(0x1e1e1e),
                fill: style.fill,
                opacity: f64::from(style.opacity.unwrap_or(1.0)),
                points: Some(points.iter().map(|p| [p.x, p.y]).collect()),
            });
            if let Some(t) = text {
                if !t.is_empty() {
                    let (_, _, tw, th) = text_bbox(min_x, min_y, t, 16.0, None);
                    c.elements.push(VirtualElement {
                        id: format!("lbl{}", c.elements.len()),
                        kind: "label",
                        x: min_x + (max_x - min_x) / 2.0 - tw / 2.0,
                        y: min_y + (max_y - min_y) / 2.0 - th / 2.0,
                        w: tw.max(1.0),
                        h: th,
                        text: Some(t.clone()),
                        font_size: 16.0,
                        stroke: style.stroke.unwrap_or(0x1e1e1e),
                        fill: None,
                        opacity: f64::from(style.opacity.unwrap_or(1.0)),
                        points: None,
                    });
                }
            }
            c.ops_applied += 1;
            msg = format!(
                "已添加{} id={}",
                if kind == "line" { "直线" } else { "箭头" },
                id8
            );
        }
        CanvasOp::Text {
            x,
            y,
            text,
            font_size,
            wrap_width,
            style,
            ..
        } => {
            if text.trim().is_empty() {
                c.ops_failed += 1;
                return Err("文本内容不能为空".to_string());
            }
            let id = assigned_id
                .map(str::to_string)
                .unwrap_or_else(|| format!("t{}", c.elements.len()));
            let id8 = id[..id.len().min(8)].to_string();
            let (ex, ey, ew, eh) = text_bbox(*x, *y, text, font_size.unwrap_or(20.0), *wrap_width);
            c.elements.push(VirtualElement {
                id,
                kind: "text",
                x: ex,
                y: ey,
                w: ew,
                h: eh,
                text: Some(text.clone()),
                font_size: font_size.unwrap_or(20.0),
                stroke: style.stroke.unwrap_or(0x1e1e1e),
                fill: style.fill,
                opacity: f64::from(style.opacity.unwrap_or(1.0)),
                points: None,
            });
            c.ops_applied += 1;
            msg = format!("已添加文本 id={id8}");
        }
        CanvasOp::Mindmap { root, cx, cy } => {
            let input = crate::scene::mindmap::MindmapNodeInput::from(root);
            let center =
                crate::scene::WPoint::new(cx.unwrap_or(800.0), cy.unwrap_or(500.0));
            let layout = crate::scene::mindmap::layout(&input, center);
            let root_id = assigned_id
                .map(str::to_string)
                .unwrap_or_else(|| format!("m{}", c.elements.len()));
            let root8 = root_id[..root_id.len().min(8)].to_string();
            // Links first (z-order mirrors the board: nodes paint on top).
            for link in &layout.links {
                let min_x = link.points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
                let min_y = link.points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
                let max_x = link.points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);
                let max_y = link.points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
                c.elements.push(VirtualElement {
                    id: format!("l{}", c.elements.len()),
                    kind: "line",
                    x: min_x,
                    y: min_y,
                    w: (max_x - min_x).max(1.0),
                    h: (max_y - min_y).max(1.0),
                    text: None,
                    font_size: 0.0,
                    stroke: link.stroke,
                    fill: None,
                    opacity: 1.0,
                    points: Some(link.points.iter().map(|p| [p.x, p.y]).collect()),
                });
            }
            for (i, spec) in layout.nodes.iter().enumerate() {
                let id = if i == 0 {
                    root_id.clone()
                } else {
                    format!("s{}", c.elements.len())
                };
                c.elements.push(VirtualElement {
                    id: id.clone(),
                    kind: "rectangle",
                    x: spec.bounds.x,
                    y: spec.bounds.y,
                    w: spec.bounds.w,
                    h: spec.bounds.h,
                    text: None,
                    font_size: 0.0,
                    stroke: spec.stroke,
                    fill: Some(spec.fill),
                    opacity: 1.0,
                    points: None,
                });
                // Bound label centered in the node; the box is sized to the
                // text, so the fold gate sits past the box (emergency brake
                // only — matches the board's mind map labels).
                let (_, _, tw, th) = text_bbox(
                    spec.bounds.x,
                    spec.bounds.y,
                    &spec.text,
                    spec.font_size,
                    Some(spec.bounds.w + 40.0),
                );
                c.elements.push(VirtualElement {
                    id: format!("{id}-label"),
                    kind: "label",
                    x: spec.bounds.x + (spec.bounds.w - tw.max(1.0)) / 2.0,
                    y: spec.bounds.y + spec.bounds.h / 2.0 - th / 2.0,
                    w: tw.max(1.0),
                    h: th,
                    text: Some(spec.text.clone()),
                    font_size: spec.font_size,
                    stroke: 0x1e1e1e,
                    fill: None,
                    opacity: 1.0,
                    points: None,
                });
            }
            c.ops_applied += 1;
            msg = format!(
                "已添加思维导图（{} 个节点）id={root8}",
                layout.nodes.len()
            );
        }
        CanvasOp::AddPage { title, ratio } => {
            let r = match ratio.as_deref().map(crate::scene::pages::PageRatio::parse) {
                Some(Some(r)) => r,
                Some(None) => {
                    c.ops_failed += 1;
                    return Err("ratio 只支持 16:9 / 4:3 / 9:16 / 3:4 / 1:1".to_string());
                }
                None => crate::scene::pages::PageRatio::default(),
            };
            let (page, number) = crate::scene::pages::push_page(&mut c.pages, title.clone(), r);
            c.ops_applied += 1;
            msg = format!(
                "已创建第 {number} 页「{}」（{}），区域 ({},{}) 尺寸 {}×{}。本页所有内容必须画在该矩形内（四周留 ≥30 边距），页面之间的空隙不要放任何元素。",
                page.title,
                r.label(),
                page.x as i32,
                page.y as i32,
                page.w as i32,
                page.h as i32
            );
        }
    }
    Ok(msg)
}

#[allow(clippy::too_many_arguments)]
fn push_shape(
    c: &mut VirtualCanvas,
    assigned_id: Option<&str>,
    kind: &'static str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    style: &CanvasStyle,
    label: Option<&str>,
) -> Result<String, String> {
    if !(w > 0.0 && h > 0.0) {
        c.ops_failed += 1;
        return Err("形状宽高必须为正数".to_string());
    }
    let id = assigned_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("s{}", c.elements.len()));
    let label_owner_id = id.clone();
    c.elements.push(VirtualElement {
        id,
        kind,
        x,
        y,
        w,
        h,
        text: None,
        font_size: 0.0,
        stroke: style.stroke.unwrap_or(0x1e1e1e),
        fill: style.fill,
        opacity: f64::from(style.opacity.unwrap_or(1.0)),
        points: None,
    });
    if let Some(t) = label {
        if !t.is_empty() {
            // Bound label: centered inside the shape, wrapping 6px inside
            // the border (matches add_bound_label).
            let fs = 20.0;
            let (_, _, tw, th) = text_bbox(x, y, t, fs, Some((w - 12.0).max(10.0)));
            c.elements.push(VirtualElement {
                id: format!("{label_owner_id}-label"),
                kind: "label",
                x: x + (w - tw.max(1.0)) / 2.0,
                y: y + h / 2.0 - th / 2.0,
                w: tw.max(1.0),
                h: th,
                text: Some(t.to_string()),
                font_size: fs,
                stroke: style.stroke.unwrap_or(0x1e1e1e),
                fill: None,
                opacity: f64::from(style.opacity.unwrap_or(1.0)),
                points: None,
            });
        }
    }
    Ok(format!(
        "已添加{} id={}",
        kind_label(kind),
        &label_owner_id[..label_owner_id.len().min(8)]
    ))
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "rectangle" => "矩形",
        "ellipse" => "椭圆",
        "diamond" => "菱形",
        "line" => "直线",
        "arrow" => "箭头",
        _ => "形状",
    }
}

// ---------------------------------------------------------------------------
// Acceptance checks
// ---------------------------------------------------------------------------

fn luminance(rgb: u32) -> f64 {
    let r = ((rgb >> 16) & 0xff) as f64 / 255.0;
    let g = ((rgb >> 8) & 0xff) as f64 / 255.0;
    let b = (rgb & 0xff) as f64 / 255.0;
    0.299 * r + 0.587 * g + 0.114 * b
}

fn rects_overlap(a: &VirtualElement, b: &VirtualElement, shrink: f64) -> bool {
    let (ax0, ay0, ax1, ay1) = (
        a.x + shrink,
        a.y + shrink,
        a.x + a.w - shrink,
        a.y + a.h - shrink,
    );
    let (bx0, by0, bx1, by1) = (
        b.x + shrink,
        b.y + shrink,
        b.x + b.w - shrink,
        b.y + b.h - shrink,
    );
    ax0 < bx1 && bx0 < ax1 && ay0 < by1 && by0 < ay1
}

fn is_base_panel(e: &VirtualElement) -> bool {
    matches!(e.kind, "rectangle" | "ellipse" | "diamond") && is_board_sized(e)
}

/// Board-sized: a full-width frame, or a section panel (黑板报的分区底板).
fn is_board_sized(e: &VirtualElement) -> bool {
    (e.w >= 1000.0 && e.h >= 600.0) || (e.w >= 280.0 && e.h >= 350.0)
}

fn check(name: impl Into<String>, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

/// The "effective" surface color behind the text: the canvas background, or —
/// if a huge filled shape covers most of the canvas (the model sometimes
/// paints a full-canvas panel) — that shape's fill, since it visually
/// replaces the board surface.
fn effective_surface(canvas: &VirtualCanvas) -> Option<u32> {
    const CANVAS_AREA: f64 = 1600.0 * 1000.0;
    let mut covered: Option<u32> = None;
    for e in &canvas.elements {
        if matches!(e.kind, "rectangle" | "ellipse") && e.w * e.h >= 0.8 * CANVAS_AREA {
            if let Some(f) = e.fill {
                // Only dark full-canvas fills count as "the new surface";
                // a light one is the model burying the blackboard.
                if is_dark_surface(f) {
                    covered = Some(f);
                }
            }
        }
    }
    covered.or(canvas.background)
}

fn is_dark_surface(c: u32) -> bool {
    luminance(c) <= 0.45
}

/// Score a replayed canvas against the blackboard-poster acceptance criteria.
/// `drew_anything` is the agent's own "did it draw at all" flag.
pub fn evaluate(
    canvas: &VirtualCanvas,
    drew_anything: bool,
    total_tool_calls: usize,
) -> EvalReport {
    let mut checks = Vec::new();
    let texts: Vec<&VirtualElement> = canvas.texts().collect();

    // -- 风格 --
    let surface = effective_surface(canvas);
    let surface_dark = surface.map(is_dark_surface).unwrap_or(false);
    checks.push(check(
        "板面为深色（黑板）",
        surface_dark,
        match (canvas.background, surface) {
            (bg, Some(s)) if Some(s) != bg => format!(
                "画布背景 #{:06x}，但被大面积填充形状覆盖为 #{s:06x}（亮度 {:.2}）——黑板底色被盖住了",
                canvas.background.unwrap_or(0xFFFFFF),
                luminance(s)
            ),
            (Some(c), _) => format!("背景 #{c:06x}，亮度 {:.2}", luminance(c)),
            (None, _) => "未设置画布背景".to_string(),
        },
    ));

    // 全屏覆盖矩形禁令：底色只能用 set_canvas_background。画一个盖住整个
    // 画布的填充矩形会埋掉板面，让粉笔字失去对比（第一轮实测教训）。
    let mut full_cover: Vec<String> = Vec::new();
    for e in &canvas.elements {
        if matches!(e.kind, "rectangle" | "ellipse") && e.fill.is_some() {
            if e.w >= CANVAS_W * 0.9 && e.h >= CANVAS_H * 0.9 {
                // A dark full-canvas panel is harmless (equals the board
                // surface); a LIGHT one buries the blackboard and kills
                // chalk contrast (实测第一轮事故)。
                let light = e.fill.map(|f| !is_dark_surface(f)).unwrap_or(false);
                if light {
                    full_cover.push(format!(
                        "{}({:.0}x{:.0}) fill #{:06x}",
                        e.kind,
                        e.w,
                        e.h,
                        e.fill.unwrap_or(0)
                    ));
                }
            }
        }
    }
    checks.push(check(
        "无浅色全画布覆盖矩形（底色用 set_canvas_background）",
        full_cover.is_empty(),
        if full_cover.is_empty() {
            "无".to_string()
        } else {
            format!("浅色覆盖形状: {}", full_cover.join("、"))
        },
    ));

    // 字面换行残留：模型双重转义的 "\\n" 会原样渲染出来（实测事故）。
    // 板端 normalize_text 会清洗，这里兜底检测其他路径。
    const LITERAL_LF: &str = "\\n";
    let literal_n: Vec<String> = texts
        .iter()
        .filter(|t| {
            t.text
                .as_deref()
                .map(|s| s.contains(LITERAL_LF))
                .unwrap_or(false)
        })
        .map(|t| {
            format!(
                "「{}…」",
                t.text
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(8)
                    .collect::<String>()
            )
        })
        .collect();
    checks.push(check(
        "无字面换行符残留",
        literal_n.is_empty(),
        if literal_n.is_empty() {
            "无".to_string()
        } else {
            format!("含字面 \\n 的文本: {}", literal_n.join("、"))
        },
    ));

    // 文字与有效板面的对比：深底要浅字，浅底要深字。
    let contrast_fail: Vec<String> = match surface {
        Some(s) => {
            let dark = is_dark_surface(s);
            texts
                .iter()
                .filter(|t| {
                    let l = luminance(t.stroke);
                    if dark {
                        l < 0.6
                    } else {
                        l > 0.4
                    }
                })
                .map(|t| {
                    format!(
                        "「{}」#{:06x}",
                        t.text
                            .as_deref()
                            .unwrap_or("")
                            .chars()
                            .take(6)
                            .collect::<String>(),
                        t.stroke
                    )
                })
                .collect()
        }
        None => vec![],
    };
    checks.push(check(
        "文字与板面对比充足",
        contrast_fail.is_empty(),
        if contrast_fail.is_empty() {
            format!(
                "全部文字与板面对比清晰（{}）",
                match surface {
                    Some(s) => format!("板面 #{s:06x}"),
                    None => "默认白板".to_string(),
                }
            )
        } else {
            format!("对比不足: {}", contrast_fail.join("、"))
        },
    ));

    // -- 结构 --
    let titles: Vec<&VirtualElement> = texts
        .iter()
        .copied()
        .filter(|t| t.font_size >= 36.0)
        .collect();
    checks.push(check(
        "存在标题级文本（字号 ≥ 36）",
        !titles.is_empty(),
        if titles.is_empty() {
            "没有大字号标题".into()
        } else {
            format!(
                "最大标题字号 {:.0}",
                titles.iter().map(|t| t.font_size).fold(0.0, f64::max)
            )
        },
    ));

    let bodies = texts
        .iter()
        .filter(|t| {
            t.font_size > 0.0
                && t.font_size <= 26.0
                && t.text.as_deref().map(|s| s.chars().count()).unwrap_or(0) >= 6
        })
        .count();
    checks.push(check(
        "正文/板块文本块 ≥ 3",
        bodies >= 3,
        format!("共 {bodies} 个"),
    ));

    // 底板 = 整幅边框，或 ≥ 3 块分区底板且合计面积覆盖过半（合法的板报
    // 设计是三栏分区，不要求单块覆盖大半张画布）。
    let boards: Vec<&VirtualElement> = canvas
        .elements
        .iter()
        .filter(|e| is_base_panel(e))
        .collect();
    let frames = boards
        .iter()
        .filter(|e| e.w >= 1000.0 && e.h >= 600.0)
        .count();
    let coverage: f64 = boards.iter().map(|e| e.w * e.h).sum();
    let boards_ok = frames >= 1 || (boards.len() >= 3 && coverage >= 0.45 * CANVAS_W * CANVAS_H);
    checks.push(check(
        "存在底板/边框（整幅框或 ≥3 块分区板且覆盖过半）",
        boards_ok,
        format!(
            "整幅框 {frames} 个，分区底板 {} 块，合计覆盖 {:.0}%",
            boards.len(),
            coverage / (CANVAS_W * CANVAS_H) * 100.0
        ),
    ));

    let dividers = canvas
        .elements
        .iter()
        .filter(|e| matches!(e.kind, "line" | "arrow"))
        .count();
    checks.push(check(
        "分隔/装饰线 ≥ 1",
        dividers >= 1,
        format!("共 {dividers} 条"),
    ));

    let decorations = canvas
        .elements
        .iter()
        .filter(|e| matches!(e.kind, "rectangle" | "ellipse" | "diamond") && !is_base_panel(e))
        .count();
    checks.push(check(
        "插图/装饰形状 ≥ 2",
        decorations >= 2,
        format!("共 {decorations} 个"),
    ));
    // 分隔与装饰互补：花边可以是线条，也可以是成组的小形状（花瓣/星星）。
    // 门槛：至少一条分隔线 + 装饰足够丰富（两者合计 ≥ 6）。
    let deco_ok = dividers >= 1 && decorations >= 2 && (dividers + decorations) >= 6;
    checks.push(check(
        "分隔花边与装饰充足",
        deco_ok,
        format!("分隔线 {dividers} 条，装饰形状 {decorations} 个"),
    ));

    // -- 层级 --
    let title_max = titles.iter().map(|t| t.font_size).fold(0.0, f64::max);
    let body_max = texts
        .iter()
        .filter(|t| t.font_size > 0.0 && t.font_size < 36.0)
        .map(|t| t.font_size)
        .fold(0.0, f64::max);
    let ratio_ok = title_max > 0.0 && body_max > 0.0 && title_max / body_max >= 1.6;
    checks.push(check(
        "标题/正文字号层级差 ≥ 1.6×",
        ratio_ok,
        format!("标题 {title_max:.0} vs 正文 {body_max:.0}"),
    ));

    // -- 布局 --
    let is_deco_glyph = |t: &VirtualElement| {
        // 短装饰字符（如标题两侧的「★」）压在标题 bbox 内是有意的排版。
        t.text
            .as_deref()
            .map(|s| {
                s.chars()
                    .filter(|c| !matches!(c, ' ' | '★' | '☆' | '●' | '◆' | '·'))
                    .count()
                    <= 2
            })
            .unwrap_or(false)
    };
    let mut overlaps: Vec<String> = Vec::new();
    for i in 0..texts.len() {
        for j in i + 1..texts.len() {
            let (a, b) = (texts[i], texts[j]);
            if is_deco_glyph(a) || is_deco_glyph(b) {
                continue;
            }
            if rects_overlap(a, b, 2.0) {
                overlaps.push(format!(
                    "「{}」×「{}」",
                    a.text
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(5)
                        .collect::<String>(),
                    b.text
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(5)
                        .collect::<String>(),
                ));
            }
        }
    }
    checks.push(check(
        "文字块两两不重叠",
        overlaps.is_empty(),
        if overlaps.is_empty() {
            "无重叠".into()
        } else {
            overlaps.join("；")
        },
    ));

    // -- 构图 --
    // 报头横向居中：最大字号标题的中心偏离画布中线 ≤ 120。
    if let Some(t) = titles
        .iter()
        .copied()
        .max_by(|a, b| a.font_size.total_cmp(&b.font_size))
    {
        let center = t.x + t.w / 2.0;
        let off = (center - CANVAS_W / 2.0).abs();
        checks.push(check(
            "报头横向居中",
            off <= 120.0,
            format!("标题中心 x={center:.0}，偏离中线 {off:.0}"),
        ));
    }

    // 板块利用率：每块分区底板内要有文字，且正文距板顶 ≤ 35% 板高。
    // 整幅边框不参与。
    let section_boards: Vec<&VirtualElement> = canvas
        .elements
        .iter()
        .filter(|e| is_base_panel(e) && !(e.w >= 1000.0 && e.h >= 600.0))
        .collect();
    if !section_boards.is_empty() {
        let mut empty_panels: Vec<String> = Vec::new();
        let mut gappy_panels: Vec<String> = Vec::new();
        for b in &section_boards {
            let inside: Vec<&VirtualElement> = texts
                .iter()
                .filter(|t| {
                    let cx = t.x + t.w / 2.0;
                    let cy = t.y + t.h / 2.0;
                    cx >= b.x && cx <= b.x + b.w && cy >= b.y && cy <= b.y + b.h
                })
                .copied()
                .collect();
            if inside.is_empty() {
                empty_panels.push(format!("({:.0},{:.0})", b.x, b.y));
                continue;
            }
            let top = inside.iter().map(|t| t.y).fold(f64::INFINITY, f64::min);
            if top - b.y > 0.35 * b.h {
                gappy_panels.push(format!("({:.0},{:.0})", b.x, b.y));
            }
        }
        checks.push(check(
            "每块板块都有文字",
            empty_panels.is_empty(),
            if empty_panels.is_empty() {
                "全部板块有内容".to_string()
            } else {
                format!("空板块: {}", empty_panels.join("、"))
            },
        ));
        checks.push(check(
            "正文紧随板块顶部（留白 ≤ 35%）",
            gappy_panels.is_empty(),
            if gappy_panels.is_empty() {
                "无大段顶部留白".to_string()
            } else {
                format!("顶部留白过大: {}", gappy_panels.join("、"))
            },
        ));
    }

    // 装饰对称：画布左右两半各 ≥ 2 处装饰（线/箭头/小形状）。
    let deco_left = canvas
        .elements
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                "rectangle" | "ellipse" | "diamond" | "line" | "arrow"
            ) && !is_base_panel(e)
                && e.x + e.w / 2.0 < CANVAS_W / 2.0
        })
        .count();
    let deco_right = canvas
        .elements
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                "rectangle" | "ellipse" | "diamond" | "line" | "arrow"
            ) && !is_base_panel(e)
                && e.x + e.w / 2.0 >= CANVAS_W / 2.0
        })
        .count();
    checks.push(check(
        "装饰左右对称（两侧各 ≥ 2）",
        deco_left >= 2 && deco_right >= 2,
        format!("左 {deco_left} 处，右 {deco_right} 处"),
    ));

    let out_of_bounds: Vec<String> = canvas
        .elements
        .iter()
        .filter(|e| {
            e.x < -BOUNDS_TOL
                || e.y < -BOUNDS_TOL
                || e.x + e.w > CANVAS_W + BOUNDS_TOL
                || e.y + e.h > CANVAS_H + BOUNDS_TOL
        })
        .map(|e| format!("{}({:.0},{:.0})", e.kind, e.x, e.y))
        .collect();
    checks.push(check(
        "全部元素在可见范围内",
        out_of_bounds.is_empty(),
        if out_of_bounds.is_empty() {
            "无越界".into()
        } else {
            format!("越界: {}", out_of_bounds.join("、"))
        },
    ));

    // -- 过程 --
    checks.push(check(
        "无失败工具调用",
        canvas.ops_failed == 0,
        format!("失败 {} 次", canvas.ops_failed),
    ));
    // 预算按全部工具调用计（含 list_elements 自检），否则自检刷屏会绕过成本上限。
    let total = total_tool_calls.max(canvas.ops_applied + canvas.ops_failed);
    checks.push(check(
        format!("工具调用 ≤ {MAX_TOOL_CALLS}"),
        total <= MAX_TOOL_CALLS,
        format!("共 {total} 次"),
    ));
    checks.push(check(
        "确实画了东西",
        drew_anything,
        if drew_anything {
            "有绘制".to_string()
        } else {
            "未调用绘图工具".to_string()
        },
    ));

    let passed = checks.iter().all(|c| c.passed);
    EvalReport { passed, checks }
}

// ---------------------------------------------------------------------------
// 水墨山水 rubric
// ---------------------------------------------------------------------------

/// Grade an ink-wash landscape replay. The rubric is deliberately different
/// from the blackboard one: the soul of 水墨 is ink-density layering (淡墨远山
/// → 浓墨近岸), generous empty space, and the literati finishing touches
/// (竖排题跋 + 朱印).
pub fn evaluate_ink(
    canvas: &VirtualCanvas,
    drew_anything: bool,
    total_tool_calls: usize,
) -> EvalReport {
    let mut checks = Vec::new();

    // -- 宣纸底 --
    let paper_ok = canvas
        .background
        .map(|c| luminance(c) >= 0.75)
        .unwrap_or(false);
    checks.push(check(
        "宣纸底（浅色）",
        paper_ok,
        match canvas.background {
            Some(c) => format!("背景 #{c:06x}，亮度 {:.2}", luminance(c)),
            None => "未设置画布背景".to_string(),
        },
    ));

    // -- 远山层：宽扁椭圆、带填充、墨色可见 --
    let is_mountain = |e: &VirtualElement| {
        let shaped = matches!(e.kind, "ellipse" | "polygon");
        shaped && e.fill.is_some() && (e.w >= 250.0 && e.h >= 80.0 || e.w * e.h >= 120_000.0)
    };
    let mountains: Vec<&VirtualElement> =
        canvas.elements.iter().filter(|e| is_mountain(e)).collect();
    // 有效墨色：fill 按透明度与宣纸混合后的亮度，须与纸面拉开 ≥ 0.24，
    // 否则该山在宣纸上不可见（opacity 过低的实测教训）。
    let paper_lum = canvas.background.map(luminance).unwrap_or(0.94);
    let visible_ink = |e: &VirtualElement| {
        let fill_lum = luminance(e.fill.unwrap_or(0xFFFFFF));
        let eff = fill_lum * e.opacity + paper_lum * (1.0 - e.opacity);
        (paper_lum - eff).abs() >= 0.24
    };
    let visible_mountains = mountains.iter().filter(|m| visible_ink(m)).count();
    checks.push(check(
        "远山层 ≥ 3（宽扁椭圆，墨色可见）",
        visible_mountains >= 3,
        format!(
            "宽扁椭圆 {} 只，其中墨色可见 {} 只（宣纸亮度 {:.2}）",
            mountains.len(),
            visible_mountains,
            paper_lum
        ),
    ));

    // 不可见的山单独提示：透明度过低 = 墨太淡
    let invisible: Vec<String> = mountains
        .iter()
        .filter(|m| !visible_ink(m))
        .map(|m| format!("opacity {:.2}", m.opacity))
        .collect();
    if !invisible.is_empty() {
        checks.push(check(
            "远山透明度不可低于可见下限",
            false,
            format!("过淡的山: {}", invisible.join("、")),
        ));
    }

    // -- 墨色递进：远山透明度或墨色有梯度（不能全画布一个浓淡）--
    let opacities: Vec<f64> = mountains.iter().map(|m| m.opacity).collect();
    let opacity_spread = opacities.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
        - opacities.iter().cloned().fold(f64::INFINITY, f64::min);
    let fill_variety = mountains
        .iter()
        .map(|m| m.fill)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let gradation = mountains.len() >= 2 && (opacity_spread >= 0.12 || fill_variety >= 2);
    checks.push(check(
        "墨色浓淡有递进",
        gradation,
        format!(
            "透明度跨度 {:.2}，墨色 {} 种",
            opacity_spread.max(0.0),
            fill_variety
        ),
    ));

    // -- 浓墨近景 ≥ 1 --
    let dark_near = canvas
        .elements
        .iter()
        .filter(|e| {
            matches!(e.kind, "rectangle" | "ellipse")
                && e.fill.is_some()
                && (e.opacity >= 0.55 || luminance(e.fill.unwrap_or(0xFFFFFF)) <= 0.35)
                && visible_ink(e)
        })
        .count();
    checks.push(check(
        "浓墨近景 ≥ 1（近实远虚）",
        dark_near >= 1,
        format!("共 {dark_near} 处"),
    ));

    // -- 竖排题跋：一列单字（≥3 字、横向聚拢）或窄高文本块 --
    let single_chars: Vec<&VirtualElement> = texts_of(canvas)
        .into_iter()
        .filter(|t| {
            t.text
                .as_deref()
                .map(|s| {
                    s.chars().count() == 1
                        && s.chars().next().map(|c| !c.is_ascii()).unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .collect();
    let has_column = if single_chars.len() >= 3 {
        let xs: Vec<f64> = single_chars.iter().map(|t| t.x).collect();
        let (min_x, max_x) = (
            xs.iter().cloned().fold(f64::INFINITY, f64::min),
            xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        );
        max_x - min_x <= 80.0
    } else {
        false
    };
    let narrow_tall = texts_of(canvas).iter().any(|t| {
        let chars = t.text.as_deref().map(|s| s.chars().count()).unwrap_or(0);
        (4..=16).contains(&chars) && t.w <= 90.0 && t.h >= t.w * 1.8
    });
    checks.push(check(
        "竖排题跋",
        has_column || narrow_tall,
        format!(
            "单字 {} 个{}，窄高文本 {}",
            single_chars.len(),
            if has_column {
                "（成列）"
            } else {
                "（未成列）"
            },
            if narrow_tall { "有" } else { "无" }
        ),
    ));

    // -- 朱印：小型红 dominant 方块 --
    let seal = canvas.elements.iter().any(|e| {
        matches!(e.kind, "rectangle" | "ellipse")
            && e.w <= 50.0
            && e.h <= 50.0
            && e.fill
                .map(|f| {
                    let r = ((f >> 16) & 0xff) as f64;
                    let g = ((f >> 8) & 0xff) as f64;
                    let b = (f & 0xff) as f64;
                    r - g.max(b) >= 30.0 && luminance(f) <= 0.55
                })
                .unwrap_or(false)
    });
    checks.push(check(
        "朱红印章",
        seal,
        if seal {
            "有".to_string()
        } else {
            "缺".to_string()
        },
    ));

    // -- 点景（舟/雁/树）：小型线形或小形状 --
    let small_lines = canvas
        .elements
        .iter()
        .filter(|e| matches!(e.kind, "line" | "arrow") && e.w <= 220.0 && e.h <= 120.0)
        .count();
    let small_shapes = canvas
        .elements
        .iter()
        .filter(|e| matches!(e.kind, "ellipse" | "diamond") && !is_mountain(e) && e.w <= 120.0)
        .count();
    checks.push(check(
        "点景（舟/雁/树）≥ 1",
        small_lines + small_shapes >= 1,
        format!("小型线形 {small_lines}，小形状 {small_shapes}"),
    ));

    // 浓墨压场克制：渲染后接近黑色的大块（合成亮度 ≤ 0.3）合计 ≤ 画布
    // 20%——过重会压满画面下部，挤掉留白与呼吸空间（实测教训）。中景的
    // 深灰层次（合成亮度 > 0.3）不算压场。
    let near_black_count = canvas
        .elements
        .iter()
        .filter(|e| {
            matches!(e.kind, "rectangle" | "ellipse" | "polygon")
                && e.fill.is_some()
                && e.w * e.h >= 40_000.0
        })
        .map(|e| {
            let fill_lum = luminance(e.fill.unwrap_or(0xFFFFFF));
            fill_lum * e.opacity + 0.94 * (1.0 - e.opacity)
        })
        .filter(|eff| *eff <= 0.30)
        .count();
    checks.push(check(
        "近黑压场克制（无大面积近黑色块）",
        near_black_count == 0,
        format!("近黑大块 {near_black_count} 块"),
    ));

    // -- 无越界 --
    let out_of_bounds: Vec<String> = canvas
        .elements
        .iter()
        .filter(|e| {
            e.x < -BOUNDS_TOL
                || e.y < -BOUNDS_TOL
                || e.x + e.w > CANVAS_W + BOUNDS_TOL
                || e.y + e.h > CANVAS_H + BOUNDS_TOL
        })
        .map(|e| format!("{}({:.0},{:.0})", e.kind, e.x, e.y))
        .collect();
    checks.push(check(
        "全部元素在可见范围内",
        out_of_bounds.is_empty(),
        if out_of_bounds.is_empty() {
            "无越界".to_string()
        } else {
            format!("越界: {}", out_of_bounds.join("、"))
        },
    ));

    // -- 纪律 --
    checks.push(check(
        "无失败工具调用",
        canvas.ops_failed == 0,
        format!("失败 {} 次", canvas.ops_failed),
    ));
    let total = total_tool_calls.max(canvas.ops_applied + canvas.ops_failed);
    checks.push(check(
        format!("工具调用 ≤ {MAX_TOOL_CALLS}"),
        total <= MAX_TOOL_CALLS,
        format!("共 {total} 次"),
    ));
    checks.push(check(
        "确实画了东西",
        drew_anything,
        if drew_anything {
            "有绘制".to_string()
        } else {
            "未调用绘图工具".to_string()
        },
    ));

    let passed = checks.iter().all(|c| c.passed);
    EvalReport { passed, checks }
}

fn texts_of(canvas: &VirtualCanvas) -> Vec<&VirtualElement> {
    canvas
        .elements
        .iter()
        .filter(|e| e.kind == "text" || e.kind == "label")
        .collect()
}

// ---------------------------------------------------------------------------
// 思维导图 rubric
// ---------------------------------------------------------------------------

/// Tool-call budget for a mind map run: the whole point of `draw_mindmap` is
/// that ONE call lays out the entire tree, so a run beyond a handful of calls
/// means the model fell back to hand-placing rectangles (which cannot hit the
/// no-overlap/no-crossing bar) — the budget steers it back to the tool.
const MINDMAP_MAX_TOOL_CALLS: usize = 20;
/// Minimum node count the exam demands (根 + ≥4 一级分支 + 每分支 ≥2 要点 = 13).
const MINDMAP_MIN_NODES: usize = 13;
/// Node label length cap, matching the draw_mindmap tool's validation.
const MINDMAP_MAX_TEXT: usize = 24;

/// Labeled node shapes: container shapes paired with their bound label
/// element (`<shape id>-label`, the same convention the replay uses).
fn mindmap_nodes(canvas: &VirtualCanvas) -> Vec<(&VirtualElement, &VirtualElement)> {
    canvas
        .elements
        .iter()
        .filter(|e| matches!(e.kind, "rectangle" | "ellipse" | "diamond"))
        .filter_map(|shape| {
            let label_id = format!("{}-label", shape.id);
            canvas
                .elements
                .iter()
                .find(|l| l.id == label_id)
                .map(|label| (shape, label))
        })
        .collect()
}

/// Points strictly inside a rectangle shrunk by `shrink` (edge contact is
/// allowed — links legitimately end ON node edges).
fn strictly_inside(px: f64, py: f64, e: &VirtualElement, shrink: f64) -> bool {
    px > e.x + shrink && px < e.x + e.w - shrink && py > e.y + shrink && py < e.y + e.h - shrink
}

/// Strict interior×interior segment crossing: both segments must straddle
/// each other (endpoint touches, T-junctions and collinear overlaps give a
/// zero side product → not a crossing). Same-parent mind-map links
/// legitimately T-join into a bundled trunk; a visual crossing is two lines
/// passing through each other's interiors.
fn segments_cross(p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), p4: (f64, f64)) -> bool {
    fn ccw(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
        (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
    }
    let d1 = ccw(p3, p4, p1);
    let d2 = ccw(p3, p4, p2);
    let d3 = ccw(p1, p2, p3);
    let d4 = ccw(p1, p2, p4);
    (d1 * d2 < 0.0) && (d3 * d4 < 0.0)
}

/// Grade a replayed mind map run. Structure (enough labeled nodes), layout
/// (no overlaps / link intrusions / link crossings), text discipline
/// (keyword-length single-line labels), bounds and the tool budget.
pub fn evaluate_mindmap(
    canvas: &VirtualCanvas,
    drew_anything: bool,
    total_tool_calls: usize,
) -> EvalReport {
    let mut checks = Vec::new();
    let nodes = mindmap_nodes(canvas);
    let links: Vec<&VirtualElement> = canvas
        .elements
        .iter()
        .filter(|e| matches!(e.kind, "line" | "arrow") && e.points.is_some())
        .collect();
    let all_texts = texts_of(canvas);

    // -- 结构 --
    checks.push(check(
        "确实画了东西",
        drew_anything,
        if drew_anything {
            "有绘制".to_string()
        } else {
            "未调用绘图工具".to_string()
        },
    ));
    checks.push(check(
        format!("节点（带文字的形状）≥ {MINDMAP_MIN_NODES}"),
        nodes.len() >= MINDMAP_MIN_NODES,
        format!("共 {} 个节点", nodes.len()),
    ));

    // -- 根节点居中 + 分支左右分布 --
    // 根 = 面积最大的节点（draw_mindmap 的根字号最大；手绘路径通常也是如此）。
    let root = nodes
        .iter()
        .max_by(|a, b| (a.0.w * a.0.h).total_cmp(&(b.0.w * b.0.h)))
        .map(|(s, _)| *s);
    if let Some(root) = root {
        let rcx = root.x + root.w / 2.0;
        let rcy = root.y + root.h / 2.0;
        let centered = (500.0..=1100.0).contains(&rcx) && (300.0..=700.0).contains(&rcy);
        checks.push(check(
            "中心主题靠近画布中部",
            centered,
            format!("中心主题位于 ({rcx:.0}, {rcy:.0})"),
        ));
        let left = nodes
            .iter()
            .filter(|(s, _)| s.x + s.w / 2.0 < rcx - 1.0)
            .count();
        let right = nodes
            .iter()
            .filter(|(s, _)| s.x + s.w / 2.0 > rcx + 1.0)
            .count();
        checks.push(check(
            "分支左右两侧都有分布（各 ≥ 2）",
            left >= 2 && right >= 2,
            format!("左 {left} 个，右 {right} 个"),
        ));
    } else {
        checks.push(check("中心主题靠近画布中部", false, "没有节点".to_string()));
        checks.push(check(
            "分支左右两侧都有分布（各 ≥ 2）",
            false,
            "没有节点".to_string(),
        ));
    }

    // -- 布局：节点不重叠 --
    let mut overlaps: Vec<String> = Vec::new();
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            if rects_overlap(nodes[i].0, nodes[j].0, 4.0) {
                overlaps.push(format!(
                    "「{}」×「{}」",
                    nodes[i].1.text.as_deref().unwrap_or("?"),
                    nodes[j].1.text.as_deref().unwrap_or("?")
                ));
            }
        }
    }
    checks.push(check(
        "节点两两不重叠",
        overlaps.is_empty(),
        if overlaps.is_empty() {
            "无重叠".to_string()
        } else {
            format!("重叠: {}", overlaps.join("、"))
        },
    ));

    // -- 布局：连线不穿节点内部 --
    // 采样每段折线；节点矩形向内收缩 3px（连线贴边属正常）。
    let mut intrusions: Vec<String> = Vec::new();
    for link in &links {
        let pts = link.points.as_deref().unwrap_or(&[]);
        let mut offender: Option<usize> = None;
        'outer: for (ni, (shape, _)) in nodes.iter().enumerate() {
            for w in pts.windows(2) {
                let (ax, ay) = (w[0][0], w[0][1]);
                let (bx, by) = (w[1][0], w[1][1]);
                for k in 0..=24 {
                    let t = k as f64 / 24.0;
                    let px = ax + (bx - ax) * t;
                    let py = ay + (by - ay) * t;
                    if strictly_inside(px, py, shape, 3.0) {
                        offender = Some(ni);
                        break 'outer;
                    }
                }
            }
        }
        if let Some(ni) = offender {
            intrusions.push(format!(
                "连线进入节点{ni}「{}」",
                nodes[ni].1.text.as_deref().unwrap_or("?")
            ));
        }
    }
    checks.push(check(
        "连线不穿过节点内部",
        intrusions.is_empty(),
        if intrusions.is_empty() {
            "无穿越".to_string()
        } else {
            format!("{} 处", intrusions.len())
        },
    ));

    // -- 布局：连线互不交叉（T 形汇入不算，见 segments_cross）--
    let mut crossings = 0usize;
    for i in 0..links.len() {
        for j in i + 1..links.len() {
            let a = links[i].points.as_deref().unwrap_or(&[]);
            let b = links[j].points.as_deref().unwrap_or(&[]);
            for wa in a.windows(2) {
                for wb in b.windows(2) {
                    let p1 = (wa[0][0], wa[0][1]);
                    let p2 = (wa[1][0], wa[1][1]);
                    let p3 = (wb[0][0], wb[0][1]);
                    let p4 = (wb[1][0], wb[1][1]);
                    if segments_cross(p1, p2, p3, p4) {
                        crossings += 1;
                    }
                }
            }
        }
    }
    checks.push(check(
        "连线互不交叉",
        crossings == 0,
        if crossings == 0 {
            "无交叉".to_string()
        } else {
            format!("{crossings} 处交叉")
        },
    ));

    // -- 文字纪律 --
    let too_long: Vec<String> = all_texts
        .iter()
        .filter(|t| {
            t.text
                .as_deref()
                .map(|s| s.chars().count() > MINDMAP_MAX_TEXT)
                .unwrap_or(false)
        })
        .map(|t| {
            format!(
                "「{}…」{}字",
                t.text
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(10)
                    .collect::<String>(),
                t.text.as_deref().unwrap_or("").chars().count()
            )
        })
        .collect();
    checks.push(check(
        format!("节点文字精炼（≤ {MINDMAP_MAX_TEXT} 字）"),
        too_long.is_empty(),
        if too_long.is_empty() {
            "全部精炼".to_string()
        } else {
            format!("过长: {}", too_long.join("、"))
        },
    ));

    const LITERAL_LF: &str = "\\n";
    let literal_n: Vec<String> = all_texts
        .iter()
        .filter(|t| {
            t.text
                .as_deref()
                .map(|s| s.contains(LITERAL_LF))
                .unwrap_or(false)
        })
        .map(|t| {
            format!(
                "「{}…」",
                t.text
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(8)
                    .collect::<String>()
            )
        })
        .collect();
    checks.push(check(
        "无字面换行符残留",
        literal_n.is_empty(),
        if literal_n.is_empty() {
            "无".to_string()
        } else {
            format!("含字面 \\n 的文本: {}", literal_n.join("、"))
        },
    ));

    // -- 纪律 --
    let out_of_bounds: Vec<String> = canvas
        .elements
        .iter()
        .filter(|e| {
            e.x < -BOUNDS_TOL
                || e.y < -BOUNDS_TOL
                || e.x + e.w > CANVAS_W + BOUNDS_TOL
                || e.y + e.h > CANVAS_H + BOUNDS_TOL
        })
        .map(|e| format!("{}({:.0},{:.0})", e.kind, e.x, e.y))
        .collect();
    checks.push(check(
        "全部元素在可见范围内",
        out_of_bounds.is_empty(),
        if out_of_bounds.is_empty() {
            "无越界".to_string()
        } else {
            format!("越界: {}", out_of_bounds.join("、"))
        },
    ));
    let surface_ok = canvas
        .background
        .map(|c| luminance(c) >= 0.55)
        .unwrap_or(true);
    checks.push(check(
        "画布保持浅色底（导图配色为深字浅底设计）",
        surface_ok,
        match canvas.background {
            Some(c) => format!("背景 #{c:06x}，亮度 {:.2}", luminance(c)),
            None => "默认白板".to_string(),
        },
    ));
    checks.push(check(
        "无失败工具调用",
        canvas.ops_failed == 0,
        format!("失败 {} 次", canvas.ops_failed),
    ));
    let total = total_tool_calls.max(canvas.ops_applied + canvas.ops_failed);
    checks.push(check(
        format!("工具调用 ≤ {MINDMAP_MAX_TOOL_CALLS}"),
        total <= MINDMAP_MAX_TOOL_CALLS,
        format!("共 {total} 次"),
    ));

    let passed = checks.iter().all(|c| c.passed);
    EvalReport { passed, checks }
}

// ---------------------------------------------------------------------------
// 幻灯片（PPT）rubric
// ---------------------------------------------------------------------------

/// Tool-call budget per page for a slide-deck run. Measured across two real
/// runs: lean 16:9 decks cost ~13/page, card-rich 9:16 decks ~25/page — 26
/// covers both while a runaway loop (redrawing pages without adding new ones)
/// still fails. The ceiling scales with the actual page count.
const SLIDES_BUDGET_PER_PAGE: usize = 26;
const SLIDES_BUDGET_BASE: usize = 20;
/// Elements may poke this far past their page's rect before counting as
/// out-of-page (matches the board's BOUNDS_TOL spirit).
const PAGE_CONTENT_TOL: f64 = 40.0;

/// The page whose rect fully contains the element (with tolerance), i.e. the
/// page the element belongs to. `None` = the element lives in the gap between
/// pages or spans across two — always a defect.
fn covering_page<'a>(
    canvas: &'a VirtualCanvas,
    e: &VirtualElement,
    tol: f64,
) -> Option<&'a crate::scene::pages::Page> {
    canvas.pages.iter().find(|p| {
        e.x >= p.x - tol
            && e.y >= p.y - tol
            && e.x + e.w <= p.x + p.w + tol
            && e.y + e.h <= p.y + p.h + tol
    })
}

/// Gaudy fill: the magenta-card failure mode from the 9:16 rubric round —
/// the model filled content cards with saturated magenta, clashing with its
/// own declared blue accent. Catches the magenta/pink-purple family, pure
/// red, and any ultra-bright ultra-saturated fill. Blue/green/pastel palettes
/// (chalk pinks, seal red 0xB33A2B, accent blue 0x1a5fd7) all pass.
fn is_gaudy_fill(c: u32) -> bool {
    let r = ((c >> 16) & 0xff) as i32;
    let g = ((c >> 8) & 0xff) as i32;
    let b = (c & 0xff) as i32;
    let magenta_like = r >= 180 && b >= 150 && g <= 140;
    let pure_red = r >= 200 && g <= 90 && b <= 90;
    let spread = r.max(g.max(b)) - r.min(g.min(b));
    let neon = (r + g + b) >= 600 && spread >= 180;
    magenta_like || pure_red || neon
}

/// Grade a replayed slide-deck run. Structure (page count, expected ratio),
/// per-page content (each page has text + enough elements), placement
/// discipline (no element in the gap between pages), per-page text
/// non-overlap, and the usual hygiene checks.
pub fn evaluate_slides(
    canvas: &VirtualCanvas,
    drew_anything: bool,
    total_tool_calls: usize,
    min_pages: usize,
    expected: crate::scene::pages::PageRatio,
) -> EvalReport {
    let mut checks = Vec::new();
    let texts: Vec<&VirtualElement> = canvas.texts().collect();

    // -- 结构 --
    checks.push(check(
        "确实画了东西",
        drew_anything,
        if drew_anything {
            "有绘制".to_string()
        } else {
            "未调用绘图工具".to_string()
        },
    ));
    checks.push(check(
        format!("页数 ≥ {min_pages}"),
        canvas.pages.len() >= min_pages,
        format!("共 {} 页", canvas.pages.len()),
    ));

    // -- 比例 --
    let (ew, eh) = expected.size();
    let expected_ar = ew / eh;
    let wrong_ratio: Vec<String> = canvas
        .pages
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            let ar = p.w / p.h;
            (ar - expected_ar).abs() / expected_ar > 0.02
        })
        .map(|(i, p)| format!("第{}页 {:.2}:{}.expected {}", i + 1, p.w, p.h, expected.label()))
        .collect();
    checks.push(check(
        format!("页面比例 = {}", expected.label()),
        wrong_ratio.is_empty(),
        if wrong_ratio.is_empty() {
            format!("全部 {} 页比例正确", canvas.pages.len())
        } else {
            wrong_ratio.join("；")
        },
    ));

    // -- 每页内容 --
    let center = |e: &VirtualElement| (e.x + e.w / 2.0, e.y + e.h / 2.0);
    let mut empty_pages: Vec<String> = Vec::new();
    let mut textless_pages: Vec<String> = Vec::new();
    for (i, p) in canvas.pages.iter().enumerate() {
        let inside: Vec<&VirtualElement> = canvas
            .elements
            .iter()
            .filter(|e| {
                let (cx, cy) = center(e);
                cx >= p.x && cx <= p.x + p.w && cy >= p.y && cy <= p.y + p.h
            })
            .collect();
        if inside.len() < 2 {
            empty_pages.push(format!("第{}页({}元素)", i + 1, inside.len()));
        }
        let has_text = inside
            .iter()
            .any(|e| matches!(e.kind, "text" | "label"));
        if !has_text {
            textless_pages.push(format!("第{}页", i + 1));
        }
    }
    checks.push(check(
        "每页都有内容（≥2 个元素）",
        empty_pages.is_empty(),
        if empty_pages.is_empty() {
            "全部页面有内容".to_string()
        } else {
            empty_pages.join("；")
        },
    ));
    checks.push(check(
        "每页都有文字",
        textless_pages.is_empty(),
        if textless_pages.is_empty() {
            "全部页面有文字".to_string()
        } else {
            format!("无文字: {}", textless_pages.join("、"))
        },
    ));

    // -- 垂直构图：内容纵向铺开，不许堆在页面上部 --
    // 竖屏首秀的实测缺陷：横版公式套竖屏，内容挤在上面 40%，下半整段空白。
    // 页码（右下角小字）不计入跨度，否则它会伪装成“沉底”的内容。
    let mut shallow_pages: Vec<String> = Vec::new();
    for (i, p) in canvas.pages.iter().enumerate() {
        let mut max_bottom = p.y;
        for e in canvas.elements.iter() {
            let (cx, cy) = center(e);
            if cx < p.x || cx > p.x + p.w || cy < p.y || cy > p.y + p.h {
                continue;
            }
            let is_page_number = matches!(e.kind, "text" | "label")
                && e.font_size <= 18.0
                && e.w <= 220.0
                && cy > p.y + 0.88 * p.h;
            if is_page_number {
                continue;
            }
            max_bottom = max_bottom.max(e.y + e.h);
        }
        let span = (max_bottom - p.y) / p.h;
        if span < 0.55 {
            shallow_pages.push(format!("第{}页(纵向跨度 {:.0}%)", i + 1, span * 100.0));
        }
    }
    checks.push(check(
        "每页内容纵向铺开（跨度 ≥ 55% 页高）",
        shallow_pages.is_empty(),
        if shallow_pages.is_empty() {
            "全部页面纵向铺满".to_string()
        } else {
            format!("内容堆上部: {}", shallow_pages.join("；"))
        },
    ));

    // -- 位置纪律：元素必须归属某页，不许落在页面之间的空隙 --
    let strays: Vec<String> = canvas
        .elements
        .iter()
        .filter(|e| covering_page(canvas, e, PAGE_CONTENT_TOL).is_none())
        .map(|e| format!("{}({:.0},{:.0})", e.kind, e.x, e.y))
        .collect();
    checks.push(check(
        "全部元素都在页面矩形内（无元素落在页间空隙）",
        strays.is_empty(),
        if strays.is_empty() {
            "无越页元素".to_string()
        } else {
            format!("越页: {}", strays.join("、"))
        },
    ));

    // -- 每页文本不重叠 --
    let is_deco_glyph = |t: &VirtualElement| {
        t.text
            .as_deref()
            .map(|s| {
                s.chars()
                    .filter(|c| !matches!(c, ' ' | '★' | '☆' | '●' | '◆' | '·'))
                    .count()
                    <= 2
            })
            .unwrap_or(false)
    };
    let mut overlaps: Vec<String> = Vec::new();
    for (i, p) in canvas.pages.iter().enumerate() {
        let page_texts: Vec<&&VirtualElement> = texts
            .iter()
            .filter(|t| {
                let (cx, cy) = center(t);
                cx >= p.x && cx <= p.x + p.w && cy >= p.y && cy <= p.y + p.h
            })
            .collect();
        for a in 0..page_texts.len() {
            for b in a + 1..page_texts.len() {
                let (t1, t2) = (page_texts[a], page_texts[b]);
                if is_deco_glyph(t1) || is_deco_glyph(t2) {
                    continue;
                }
                if rects_overlap(t1, t2, 2.0) {
                    overlaps.push(format!(
                        "第{}页「{}」×「{}」",
                        i + 1,
                        t1.text.as_deref().unwrap_or("?").chars().take(5).collect::<String>(),
                        t2.text.as_deref().unwrap_or("?").chars().take(5).collect::<String>(),
                    ));
                }
            }
        }
    }
    checks.push(check(
        "页内文字两两不重叠",
        overlaps.is_empty(),
        if overlaps.is_empty() {
            "无重叠".to_string()
        } else {
            overlaps.join("；")
        },
    ));

    // -- 配色纪律：无刺眼高饱和填充 --
    let gaudy: Vec<String> = canvas
        .elements
        .iter()
        .filter(|e| e.fill.map(is_gaudy_fill).unwrap_or(false))
        .map(|e| {
            format!(
                "{} #{:06x}",
                e.kind,
                e.fill.unwrap_or(0)
            )
        })
        .collect();
    checks.push(check(
        "无刺眼高饱和填充（洋红/纯红/荧光色）",
        gaudy.is_empty(),
        if gaudy.is_empty() {
            "配色克制".to_string()
        } else {
            format!("刺眼填充: {}", gaudy.join("、"))
        },
    ));

    // -- 字面换行残留 --
    const LITERAL_LF: &str = "\\n";
    let literal_n: Vec<String> = texts
        .iter()
        .filter(|t| {
            t.text
                .as_deref()
                .map(|s| s.contains(LITERAL_LF))
                .unwrap_or(false)
        })
        .map(|t| {
            format!(
                "「{}…」",
                t.text
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(8)
                    .collect::<String>()
            )
        })
        .collect();
    checks.push(check(
        "无字面换行符残留",
        literal_n.is_empty(),
        if literal_n.is_empty() {
            "无".to_string()
        } else {
            format!("含字面 \\n 的文本: {}", literal_n.join("、"))
        },
    ));

    // -- 纪律 --
    checks.push(check(
        "无失败工具调用",
        canvas.ops_failed == 0,
        format!("失败 {} 次", canvas.ops_failed),
    ));
    let budget = SLIDES_BUDGET_BASE
        + SLIDES_BUDGET_PER_PAGE * canvas.pages.len().max(min_pages);
    let total = total_tool_calls.max(canvas.ops_applied + canvas.ops_failed);
    checks.push(check(
        format!("工具调用 ≤ {budget}（按页数缩放）"),
        total <= budget,
        format!("共 {total} 次"),
    ));

    let passed = checks.iter().all(|c| c.passed);
    EvalReport { passed, checks }
}

#[cfg(test)]
mod slides_tests {
    use super::*;
    use crate::ai::canvas_ops::CanvasStyle;
    use crate::scene::pages::PageRatio;

    fn page_op(title: &str, ratio: &str) -> (CanvasOp, Option<String>) {
        (
            CanvasOp::AddPage {
                title: Some(title.into()),
                ratio: Some(ratio.into()),
            },
            None,
        )
    }

    fn text_op(page: &crate::scene::pages::Page, dy: f64, text: &str) -> (CanvasOp, Option<String>)
    {
        (
            CanvasOp::Text {
                x: page.x + 100.0,
                y: page.y + 100.0 + dy,
                text: text.into(),
                font_size: Some(24.0),
                align: None,
                font_family: None,
                wrap_width: None,
                style: CanvasStyle::default(),
            },
            None,
        )
    }

    fn sample_deck(pages: usize, ratio: &str) -> Vec<(CanvasOp, Option<String>)> {
        let mut ops: Vec<(CanvasOp, Option<String>)> = Vec::new();
        for i in 0..pages {
            ops.push(page_op(&format!("第{}页", i + 1), ratio));
            // Derive this page's rect from the ops so far (mirrors the board
            // layout: each new page sits after the previous ones).
            let canvas = replay(&ops);
            let p = canvas.pages.last().unwrap().clone();
            // Title + body spread down the page (the vertical-composition rule).
            ops.push(text_op(&p, 0.22 * p.h, "页标题"));
            ops.push(text_op(&p, 0.60 * p.h, "正文要点一二三"));
        }
        ops
    }

    #[test]
    fn well_formed_deck_passes() {
        let ops = sample_deck(5, "16:9");
        let canvas = replay(&ops);
        let report = evaluate_slides(&canvas, true, ops.len(), 5, PageRatio::Ratio16_9);
        assert!(
            report.passed,
            "deck must pass:\n{}",
            report.to_text()
        );
    }

    #[test]
    fn too_few_pages_fail() {
        let ops = sample_deck(3, "16:9");
        let canvas = replay(&ops);
        let report = evaluate_slides(&canvas, true, ops.len(), 5, PageRatio::Ratio16_9);
        assert!(report.checks.iter().any(|c| c.name.contains("页数") && !c.passed));
    }

    #[test]
    fn wrong_ratio_fails() {
        let ops = sample_deck(5, "9:16");
        let canvas = replay(&ops);
        let report = evaluate_slides(&canvas, true, ops.len(), 5, PageRatio::Ratio16_9);
        assert!(report
            .checks
            .iter()
            .any(|c| c.name.contains("比例") && !c.passed));
    }

    #[test]
    fn content_crammed_at_top_fails() {
        // The 9:16 debut defect: everything packed into the top of the page
        // while the bottom half stays empty. The page-number exemption must
        // not mask it (these are body-size texts anyway).
        let ops = vec![page_op("封面", "16:9")];
        let canvas = replay(&ops);
        let p = canvas.pages.last().unwrap().clone();
        let mut ops = ops;
        ops.push(text_op(&p, 100.0, "标题"));
        ops.push(text_op(&p, 160.0, "正文"));
        let canvas = replay(&ops);
        let report = evaluate_slides(&canvas, true, ops.len(), 1, PageRatio::Ratio16_9);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name.contains("纵向铺开") && !c.passed),
            "crammed page must fail:\n{}",
            report.to_text()
        );
    }

    #[test]
    fn gaudy_magenta_fills_fail() {
        let ops = sample_deck(2, "16:9");
        let canvas = replay(&ops);
        // A content card filled with the failure-mode magenta.
        let mut ops = ops;
        ops.push((
            CanvasOp::Rectangle {
                x: 200.0,
                y: 400.0,
                w: 300.0,
                h: 150.0,
                style: crate::ai::canvas_ops::CanvasStyle {
                    fill: Some(0xFF00FF),
                    ..Default::default()
                },
                text: None,
            },
            None,
        ));
        let canvas = replay(&ops);
        let report = evaluate_slides(&canvas, true, ops.len(), 2, PageRatio::Ratio16_9);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name.contains("刺眼") && !c.passed),
            "magenta card must fail:\n{}",
            report.to_text()
        );
        // Accent blue and chalk pastels stay legal.
        assert!(!is_gaudy_fill(0x1a5fd7));
        assert!(!is_gaudy_fill(0xFFC0CB));
        assert!(!is_gaudy_fill(0xB33A2B));
        assert!(is_gaudy_fill(0xFF00FF));
        assert!(is_gaudy_fill(0xFF0000));
    }

    #[test]
    fn element_in_gap_fails() {
        let ops = sample_deck(2, "16:9");
        // An element stranded in the gap between page 1 and page 2.
        let gap_x = 1600.0 + crate::scene::pages::PAGE_GAP / 2.0;
        let mut ops = ops;
        ops.push((
            CanvasOp::Text {
                x: gap_x,
                y: 100.0,
                text: "漂在缝隙里".into(),
                font_size: Some(24.0),
                align: None,
                font_family: None,
                wrap_width: None,
                style: CanvasStyle::default(),
            },
            None,
        ));
        let canvas = replay(&ops);
        let report = evaluate_slides(&canvas, true, ops.len(), 2, PageRatio::Ratio16_9);
        assert!(report
            .checks
            .iter()
            .any(|c| c.name.contains("页间空隙") && !c.passed));
    }
}

#[cfg(test)]
mod ink_tests {
    use super::*;
    use crate::ai::canvas_ops::OpFillStyle;
    use crate::ai::canvas_ops::OpPoint;

    fn pt(x: f64, y: f64) -> OpPoint {
        OpPoint::new(x, y)
    }

    fn ink_ops() -> Vec<(CanvasOp, Option<String>)> {
        let mut ops = vec![(
            CanvasOp::SetBackground {
                color: Some(0xf5efdc),
            },
            None,
        )];
        let layers = [
            (80.0, 120.0, 700.0, 240.0, 0.5),
            (300.0, 180.0, 750.0, 260.0, 0.6),
            (520.0, 260.0, 700.0, 250.0, 0.72),
            (200.0, 420.0, 800.0, 280.0, 0.9),
        ];
        for (x, y, w, h, op) in layers {
            ops.push((
                CanvasOp::Ellipse {
                    x,
                    y,
                    w,
                    h,
                    style: CanvasStyle {
                        fill: Some(0x4a5560),
                        opacity: Some(op),
                        fill_style: Some(OpFillStyle::Solid),
                        ..Default::default()
                    },
                    text: None,
                },
                None,
            ));
        }
        // 孤舟：一根短横线
        ops.push((
            CanvasOp::Line {
                points: vec![pt(760.0, 700.0), pt(860.0, 700.0)],
                style: CanvasStyle {
                    stroke: Some(0x3a4148),
                    ..Default::default()
                },
                text: None,
            },
            None,
        ));
        // 竖排题跋：一列单字
        for (i, ch) in ["远", "屿", "含", "烟"].into_iter().enumerate() {
            ops.push((
                CanvasOp::Text {
                    x: 60.0,
                    y: 100.0 + i as f64 * 40.0,
                    text: ch.into(),
                    font_size: Some(18.0),
                    align: None,
                    font_family: Some("kai".into()),
                    wrap_width: None,
                    style: CanvasStyle {
                        stroke: Some(0x3a3a3a),
                        ..Default::default()
                    },
                },
                None,
            ));
        }
        // 朱印
        ops.push((
            CanvasOp::Rectangle {
                x: 60.0,
                y: 280.0,
                w: 22.0,
                h: 22.0,
                style: CanvasStyle {
                    fill: Some(0xB33A2B),
                    ..Default::default()
                },
                text: None,
            },
            None,
        ));
        ops
    }

    #[test]
    fn well_formed_ink_painting_passes() {
        let ops = ink_ops();
        let canvas = replay(&ops);
        let report = evaluate_ink(&canvas, true, ops.len());
        assert!(
            report.passed,
            "report:
{}",
            report.to_text()
        );
    }

    #[test]
    fn ink_without_seal_fails() {
        let ops: Vec<_> = ink_ops()
            .into_iter()
            .filter(|(op, _)| !matches!(op, CanvasOp::Rectangle { .. }))
            .collect();
        let canvas = replay(&ops);
        let report = evaluate_ink(&canvas, true, ops.len());
        let seal = report.checks.iter().find(|c| c.name == "朱红印章").unwrap();
        assert!(!seal.passed);
        assert!(!report.passed);
    }

    #[test]
    fn ink_without_gradation_fails() {
        let mut ops = ink_ops();
        for (op, _) in ops.iter_mut() {
            if let CanvasOp::Ellipse { style, w, h, .. } = op {
                if *w >= 250.0 && *h >= 80.0 {
                    style.opacity = Some(0.3);
                }
            }
        }
        let canvas = replay(&ops);
        let report = evaluate_ink(&canvas, true, ops.len());
        let g = report
            .checks
            .iter()
            .find(|c| c.name.contains("递进"))
            .unwrap();
        assert!(!g.passed, "detail: {}", g.detail);
    }

    #[test]
    fn ink_without_column_fails() {
        let mut ops = ink_ops();
        // 把题跋单字横向摊开，破坏竖排列
        for (i, (op, _)) in ops.iter_mut().enumerate() {
            if let CanvasOp::Text { x, .. } = op {
                if *x == 60.0 {
                    *x = 60.0 + (i % 4) as f64 * 150.0;
                }
            }
        }
        let canvas = replay(&ops);
        let report = evaluate_ink(&canvas, true, ops.len());
        let col = report.checks.iter().find(|c| c.name == "竖排题跋").unwrap();
        assert!(!col.passed, "detail: {}", col.detail);
    }
}

#[cfg(test)]
mod mindmap_tests {
    use super::*;
    use crate::ai::canvas_ops::OpMindmapNode;

    /// Root + 4 branches × 3 keyword leaves — the exam's minimum shape.
    fn sample_tree() -> OpMindmapNode {
        let leaf = |t: &str| OpMindmapNode {
            text: t.into(),
            children: vec![],
        };
        let branch = |t: &str, leaves: &[&str]| OpMindmapNode {
            text: t.into(),
            children: leaves.iter().map(|t| leaf(t)).collect(),
        };
        OpMindmapNode {
            text: "高效学习方法".into(),
            children: vec![
                branch("主动回忆", &["自测", "闪卡", "费曼技巧"]),
                branch("间隔重复", &["艾宾浩斯", "Anki", "周期复习"]),
                branch("深度加工", &["类比", "举例", "跨学科联系"]),
                branch("专注环境", &["番茄钟", "远离手机", "固定时段"]),
            ],
        }
    }

    fn mindmap_op(root: OpMindmapNode) -> (CanvasOp, Option<String>) {
        (CanvasOp::Mindmap { root, cx: None, cy: None }, Some("aaa00001".into()))
    }

    #[test]
    fn replayed_sample_mindmap_passes_rubric() {
        let ops = vec![mindmap_op(sample_tree())];
        let canvas = replay(&ops);
        assert_eq!(canvas.ops_applied, 1);
        // 17 nodes + 17 labels + 16 links.
        assert_eq!(canvas.elements.len(), 17 + 17 + 16);
        let report = evaluate_mindmap(&canvas, true, 1);
        assert!(
            report.passed,
            "sample mind map must pass:\n{}",
            report.to_text()
        );
    }

    #[test]
    fn narrating_without_drawing_fails() {
        let canvas = VirtualCanvas::default();
        let report = evaluate_mindmap(&canvas, false, 0);
        assert!(!report.passed);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("确实画了东西"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn too_few_nodes_fail() {
        let small = OpMindmapNode {
            text: "根".into(),
            children: vec![
                OpMindmapNode {
                    text: "枝一".into(),
                    children: vec![],
                },
                OpMindmapNode {
                    text: "枝二".into(),
                    children: vec![],
                },
            ],
        };
        let canvas = replay(&vec![mindmap_op(small)]);
        let report = evaluate_mindmap(&canvas, true, 1);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("节点"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn hand_drawn_overlapping_nodes_fail() {
        let mut canvas = VirtualCanvas::default();
        for (i, id) in ["aaa00001", "aaa00002"].iter().enumerate() {
            let x = 400.0 + i as f64 * 40.0; // boxes overlap by design
            canvas.elements.push(VirtualElement {
                id: id.to_string(),
                kind: "rectangle",
                x,
                y: 400.0,
                w: 120.0,
                h: 50.0,
                text: None,
                font_size: 0.0,
                stroke: 0x1e1e1e,
                fill: Some(0xFFE8D9),
                opacity: 1.0,
                points: None,
            });
            canvas.elements.push(VirtualElement {
                id: format!("{id}-label"),
                kind: "label",
                x,
                y: 415.0,
                w: 60.0,
                h: 20.0,
                text: Some(format!("节点{i}")),
                font_size: 16.0,
                stroke: 0x1e1e1e,
                fill: None,
                opacity: 1.0,
                points: None,
            });
        }
        let report = evaluate_mindmap(&canvas, true, 2);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("不重叠"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn link_through_node_interior_fails() {
        let mut canvas = VirtualCanvas::default();
        // A node at (600,300)-(760,360) and a link crossing its middle.
        canvas.elements.push(VirtualElement {
            id: "aaa00001".into(),
            kind: "rectangle",
            x: 600.0,
            y: 300.0,
            w: 160.0,
            h: 60.0,
            text: None,
            font_size: 0.0,
            stroke: 0x1e1e1e,
            fill: Some(0xFFE8D9),
            opacity: 1.0,
            points: None,
        });
        canvas.elements.push(VirtualElement {
            id: "aaa00001-label".into(),
            kind: "label",
            x: 640.0,
            y: 320.0,
            w: 80.0,
            h: 20.0,
            text: Some("挡路节点".into()),
            font_size: 16.0,
            stroke: 0x1e1e1e,
            fill: None,
            opacity: 1.0,
            points: None,
        });
        canvas.elements.push(VirtualElement {
            id: "l1".into(),
            kind: "line",
            x: 400.0,
            y: 330.0,
            w: 400.0,
            h: 0.0,
            text: None,
            font_size: 0.0,
            stroke: 0x1e1e1e,
            fill: None,
            opacity: 1.0,
            points: Some(vec![[400.0, 330.0], [800.0, 330.0]]),
        });
        let report = evaluate_mindmap(&canvas, true, 2);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("不穿过节点"))
                .unwrap()
                .passed,
            "detail: {}",
            report
                .checks
                .iter()
                .find(|c| c.name.contains("不穿过节点"))
                .unwrap()
                .detail
        );
    }

    #[test]
    fn crossing_links_fail() {
        let mut canvas = VirtualCanvas::default();
        for (id, pts) in [
            ("l1", vec![[100.0, 100.0], [300.0, 300.0]]),
            ("l2", vec![[100.0, 300.0], [300.0, 100.0]]),
        ] {
            canvas.elements.push(VirtualElement {
                id: id.into(),
                kind: "line",
                x: 100.0,
                y: 100.0,
                w: 200.0,
                h: 200.0,
                text: None,
                font_size: 0.0,
                stroke: 0x1e1e1e,
                fill: None,
                opacity: 1.0,
                points: Some(pts),
            });
        }
        let report = evaluate_mindmap(&canvas, true, 2);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("互不交叉"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn overlong_label_fails_text_discipline() {
        // Same tree but one node label is a whole sentence (> 24 chars).
        let mut tree = sample_tree();
        tree.children[0].children[0].text = "这是一个远远超过二十四个字的啰里啰嗦的要点描述它应当被判定为不合格".into();
        let canvas = replay(&vec![mindmap_op(tree)]);
        let report = evaluate_mindmap(&canvas, true, 1);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("精炼"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn exceeded_tool_budget_fails() {
        let ops = vec![mindmap_op(sample_tree())];
        let canvas = replay(&ops);
        let report = evaluate_mindmap(&canvas, true, 30);
        // Name must not collide with "无失败工具调用".
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.starts_with("工具调用"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn mindmap_on_dark_surface_fails() {
        let mut ops = vec![(
            CanvasOp::SetBackground {
                color: Some(0x2A5240),
            },
            None,
        )];
        ops.push(mindmap_op(sample_tree()));
        let canvas = replay(&ops);
        let report = evaluate_mindmap(&canvas, true, 2);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("浅色底"))
                .unwrap()
                .passed
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::canvas_ops::OpPoint;

    fn style(fill: Option<u32>, stroke: u32) -> CanvasStyle {
        CanvasStyle {
            stroke: Some(stroke),
            fill,
            ..Default::default()
        }
    }

    fn pt(x: f64, y: f64) -> OpPoint {
        OpPoint::new(x, y)
    }

    fn white_text(
        x: f64,
        y: f64,
        text: &str,
        font_size: f64,
        wrap: Option<f64>,
    ) -> (CanvasOp, Option<String>) {
        (
            CanvasOp::Text {
                x,
                y,
                text: text.into(),
                font_size: Some(font_size),
                align: None,
                font_family: Some("kai".into()),
                wrap_width: wrap,
                style: style(None, 0xFFFFFF),
            },
            None,
        )
    }

    /// A minimal-but-passing poster op sequence.
    fn poster_ops() -> Vec<(CanvasOp, Option<String>)> {
        let white = 0xFFFFFFu32;
        vec![
            (CanvasOp::SetBackground { color: Some(0x2A5240) }, None),
            (CanvasOp::Rectangle { x: 30.0, y: 30.0, w: 1540.0, h: 940.0, style: style(Some(0x2A5240), white), text: None }, Some("aaa00001".into())),
            (CanvasOp::Text { x: 640.0, y: 50.0, text: "教师节快乐".into(), font_size: Some(64.0), align: None, font_family: Some("kai".into()), wrap_width: None, style: style(None, white) }, Some("aaa00002".into())),
            (CanvasOp::Line { points: vec![pt(100.0, 190.0), pt(1500.0, 190.0)], style: style(None, white), text: None }, Some("aaa00003".into())),
            (CanvasOp::Line { points: vec![pt(100.0, 880.0), pt(1500.0, 880.0)], style: style(None, white), text: None }, Some("aaa00004".into())),
            (CanvasOp::Ellipse { x: 60.0, y: 60.0, w: 60.0, h: 60.0, style: style(Some(0xFFC0CB), white), text: None }, Some("aaa00005".into())),
            (CanvasOp::Rectangle { x: 1480.0, y: 60.0, w: 60.0, h: 60.0, style: style(Some(0xFFC0CB), white), text: None }, Some("aaa00006".into())),
            (CanvasOp::Ellipse { x: 140.0, y: 70.0, w: 40.0, h: 40.0, style: style(Some(0xFFE699), white), text: None }, Some("aaa00007".into())),
            (CanvasOp::Rectangle { x: 1400.0, y: 70.0, w: 40.0, h: 40.0, style: style(Some(0xA8D8EA), white), text: None }, Some("aaa00008".into())),
            white_text(80.0, 240.0, "老师您辛苦了。春蚕到死丝方尽，蜡炬成灰泪始干。感谢每一位老师的辛勤付出与陪伴，节日快乐。", 20.0, Some(420.0)),
            white_text(580.0, 240.0, "三尺讲台育桃李，一支粉笔写春秋。在这里我们向全体老师致以节日的问候和最崇高的敬意。", 20.0, Some(420.0)),
            white_text(1080.0, 240.0, "感恩教师节，情满中秋月。愿老师们身体健康，工作顺利，桃李满天下，春风化雨润心田。", 20.0, Some(420.0)),
        ]
    }

    #[test]
    fn well_formed_poster_passes() {
        let canvas = replay(&poster_ops());
        let report = evaluate(&canvas, true, 0);
        assert!(report.passed, "report:\n{}", report.to_text());
    }

    #[test]
    fn black_text_on_board_fails_chalk_check() {
        let mut ops = poster_ops();
        for (op, _) in ops.iter_mut() {
            if let CanvasOp::Text { style, .. } = op {
                style.stroke = Some(0x1e1e1e);
            }
        }
        let report = evaluate(&replay(&ops), true, ops.len());
        let contrast = report
            .checks
            .iter()
            .find(|c| c.name.contains("对比"))
            .unwrap();
        assert!(
            !contrast.passed,
            "black text on dark board must fail contrast: {}",
            contrast.detail
        );
    }

    #[test]
    fn missing_title_fails() {
        let mut ops = poster_ops();
        ops.retain(
            |(op, _)| !matches!(op, CanvasOp::Text { font_size: Some(fs), .. } if *fs >= 36.0),
        );
        let report = evaluate(&replay(&ops), true, ops.len());
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("标题"))
                .unwrap()
                .passed,
            "removing the title must fail the title check"
        );
    }

    #[test]
    fn overlapping_text_fails() {
        let mut ops = poster_ops();
        // Move the second body block onto the first one's area.
        if let (CanvasOp::Text { x, y, .. }, _) = &mut ops[10] {
            *x = 120.0;
            *y = 260.0;
        }
        let report = evaluate(&replay(&ops), true, ops.len());
        let overlap = report
            .checks
            .iter()
            .find(|c| c.name.contains("不重叠"))
            .unwrap();
        assert!(!overlap.passed, "detail: {}", overlap.detail);
    }

    #[test]
    fn out_of_bounds_fails() {
        let mut ops = poster_ops();
        if let (CanvasOp::Text { x, .. }, _) = &mut ops[9] {
            *x = 1700.0;
        }
        let report = evaluate(&replay(&ops), true, ops.len());
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("可见范围"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn tool_call_budget_counts_everything() {
        let mut canvas = VirtualCanvas::default();
        canvas.ops_applied = 10;
        // 55 次 list_elements 自检 + 10 次绘制 = 65 > 60：预算必须算上自检。
        let report = evaluate(&canvas, true, 65);
        let budget = report
            .checks
            .iter()
            .find(|c| c.name.contains("工具调用 ≤"))
            .unwrap();
        assert!(!budget.passed, "detail: {}", budget.detail);
        let report = evaluate(&canvas, true, 40);
        assert!(
            report
                .checks
                .iter()
                .find(|c| c.name.contains("工具调用 ≤"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn literal_newline_remnants_fail() {
        let mut canvas = VirtualCanvas::default();
        canvas.background = Some(0x2A5240);
        canvas.elements.push(VirtualElement {
            id: "t1".into(),
            kind: "text",
            x: 100.0,
            y: 100.0,
            w: 400.0,
            h: 30.0,
            text: Some("一块黑板\\n三尺讲台".into()),
            font_size: 20.0,
            stroke: 0xFFFFFF,
            fill: None,
            opacity: 1.0,
            points: None,
        });
        let report = evaluate(&canvas, true, 0);
        let check = report
            .checks
            .iter()
            .find(|c| c.name.contains("字面换行"))
            .unwrap();
        assert!(!check.passed, "detail: {}", check.detail);
    }

    #[test]
    fn update_with_unknown_id_counts_as_failed() {
        let ops = vec![
            (
                CanvasOp::SetBackground {
                    color: Some(0x2A5240),
                },
                None,
            ),
            (
                CanvasOp::UpdateElement {
                    id: "beef0000".into(),
                    x: Some(1.0),
                    y: None,
                    text: None,
                    style: CanvasStyle::default(),
                    font_size: None,
                },
                None,
            ),
        ];
        let canvas = replay(&ops);
        assert_eq!(canvas.ops_failed, 1);
        let report = evaluate(&canvas, true, 0);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name.contains("无失败"))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn update_moves_and_retexts_elements() {
        let mut ops = poster_ops();
        ops.push((
            CanvasOp::UpdateElement {
                id: "aaa00002".into(),
                x: Some(600.0),
                y: Some(60.0),
                text: Some("庆祝教师节".into()),
                style: CanvasStyle::default(),
                font_size: Some(72.0),
            },
            None,
        ));
        let canvas = replay(&ops);
        let title = canvas.elements.iter().find(|e| e.id == "aaa00002").unwrap();
        assert_eq!((title.x, title.y), (600.0, 60.0));
        assert_eq!(title.text.as_deref(), Some("庆祝教师节"));
        assert!((title.font_size - 72.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::ai::canvas_ops::CanvasStyle;

    #[test]
    fn apply_returns_ok_with_board_style_message() {
        let mut c = VirtualCanvas::default();
        let r = apply(
            &mut c,
            &CanvasOp::SetBackground {
                color: Some(0x2A5240),
            },
            None,
        );
        assert!(r.is_ok(), "SetBackground apply returned Err: {r:?}");
        let r = apply(
            &mut c,
            &CanvasOp::Text {
                x: 10.0,
                y: 10.0,
                text: "你好".into(),
                font_size: Some(20.0),
                align: None,
                font_family: None,
                wrap_width: None,
                style: CanvasStyle::default(),
            },
            Some("abcd1234"),
        );
        assert!(r.is_ok(), "Text apply returned Err: {r:?}");
        assert!(
            r.as_deref()
                .map(|s| s.contains("abcd1234"))
                .unwrap_or(false),
            "message should carry the id: {r:?}"
        );
    }
}
