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
}

/// The virtual canvas state after replaying a run's ops.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VirtualCanvas {
    pub background: Option<u32>,
    pub elements: Vec<VirtualElement>,
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
            "=== 黑板报评测报告：{} ===\n",
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
            push_shape(
                c,
                assigned_id,
                "rectangle",
                *x,
                *y,
                *w,
                *h,
                style,
                text.as_deref(),
            );
        }
        CanvasOp::Ellipse {
            x,
            y,
            w,
            h,
            style,
            text,
        } => {
            push_shape(
                c,
                assigned_id,
                "ellipse",
                *x,
                *y,
                *w,
                *h,
                style,
                text.as_deref(),
            );
        }
        CanvasOp::Diamond {
            x,
            y,
            w,
            h,
            style,
            text,
        } => {
            push_shape(
                c,
                assigned_id,
                "diamond",
                *x,
                *y,
                *w,
                *h,
                style,
                text.as_deref(),
            );
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
            });
            c.ops_applied += 1;
            msg = format!("已添加文本 id={id8}");
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
    });
    if let Some(t) = label {
        if !t.is_empty() {
            // Bound label: centered inside the shape (matches add_bound_label).
            let fs = 20.0;
            let (_, _, tw, th) = text_bbox(x, y, t, fs, Some(w - 20.0));
            c.elements.push(VirtualElement {
                id: format!("{label_owner_id}-label"),
                kind: "label",
                x: x + 10.0,
                y: y + h / 2.0 - th / 2.0,
                w: tw.max(1.0),
                h: th,
                text: Some(t.to_string()),
                font_size: fs,
                stroke: style.stroke.unwrap_or(0x1e1e1e),
                fill: None,
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

/// Score a replayed canvas against the blackboard-poster acceptance criteria.
/// `drew_anything` is the agent's own "did it draw at all" flag.
pub fn evaluate(canvas: &VirtualCanvas, drew_anything: bool) -> EvalReport {
    let mut checks = Vec::new();
    let texts: Vec<&VirtualElement> = canvas.texts().collect();

    // -- 风格 --
    let bg_ok = canvas
        .background
        .map(luminance)
        .map(|l| l <= 0.45)
        .unwrap_or(false);
    checks.push(check(
        "背景为深色板面",
        bg_ok,
        match canvas.background {
            Some(c) => format!("背景 #{c:06x}，亮度 {:.2}", luminance(c)),
            None => "未设置画布背景".into(),
        },
    ));

    let chalk_fail: Vec<String> = texts
        .iter()
        .filter(|t| luminance(t.stroke) < 0.6)
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
        .collect();
    checks.push(check(
        "文字使用粉笔色系（深底高亮）",
        chalk_fail.is_empty(),
        if chalk_fail.is_empty() {
            "全部文字为高亮粉笔色".into()
        } else {
            format!("非粉笔色: {}", chalk_fail.join("、"))
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
    let boards_ok = frames >= 1 || (boards.len() >= 3 && coverage >= 0.5 * CANVAS_W * CANVAS_H);
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
    let mut overlaps: Vec<String> = Vec::new();
    for i in 0..texts.len() {
        for j in i + 1..texts.len() {
            let (a, b) = (texts[i], texts[j]);
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
    let total = canvas.ops_applied + canvas.ops_failed;
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
            (CanvasOp::Text { x: 500.0, y: 50.0, text: "教师节快乐".into(), font_size: Some(64.0), align: None, font_family: Some("kai".into()), wrap_width: None, style: style(None, white) }, Some("aaa00002".into())),
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
        let report = evaluate(&canvas, true);
        assert!(report.passed, "report:\n{}", report.to_text());
    }

    #[test]
    fn white_background_fails() {
        let mut ops = poster_ops();
        ops[0] = (CanvasOp::SetBackground { color: None }, None);
        let report = evaluate(&replay(&ops), true);
        let bg = report
            .checks
            .iter()
            .find(|c| c.name == "背景为深色板面")
            .unwrap();
        assert!(!bg.passed);
        assert!(!report.passed);
    }

    #[test]
    fn black_text_on_board_fails_chalk_check() {
        let mut ops = poster_ops();
        for (op, _) in ops.iter_mut() {
            if let CanvasOp::Text { style, .. } = op {
                style.stroke = Some(0x1e1e1e);
            }
        }
        let report = evaluate(&replay(&ops), true);
        let chalk = report
            .checks
            .iter()
            .find(|c| c.name.contains("粉笔"))
            .unwrap();
        assert!(!chalk.passed);
    }

    #[test]
    fn missing_title_fails() {
        let mut ops = poster_ops();
        ops.retain(
            |(op, _)| !matches!(op, CanvasOp::Text { font_size: Some(fs), .. } if *fs >= 36.0),
        );
        let report = evaluate(&replay(&ops), true);
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
        let report = evaluate(&replay(&ops), true);
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
        let report = evaluate(&replay(&ops), true);
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
        let report = evaluate(&canvas, true);
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
