//! Canvas operations emitted by AI drawing tools.
//!
//! These describe *what* to draw in a serialization-friendly way (so the rig
//! tool argument types can derive a JSON schema for the model). They are
//! translated into real scene elements on the GPUI main thread by
//! `AiPanel::apply_canvas_op` — the tools themselves never touch the board,
//! they only send an op through a channel.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A 2D point in world coordinates (same space as the whiteboard: origin at
/// top-left, x grows right, y grows down).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpPoint {
    pub x: f64,
    pub y: f64,
}

impl OpPoint {
    #[allow(dead_code)]
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

impl From<OpPoint> for crate::scene::WPoint {
    fn from(p: OpPoint) -> Self {
        crate::scene::WPoint::new(p.x, p.y)
    }
}

/// How a line is dashed. Mirrors `scene::StrokeStyle` but with serde-friendly
/// naming for the model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpStrokeStyle {
    #[default]
    Solid,
    Dashed,
}

/// How shape backgrounds are filled: hachure sketch lines (default) or a
/// solid flat block — the "chalk paste" panels of a blackboard poster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpFillStyle {
    #[default]
    Hachure,
    Solid,
}

/// Optional visual style. Every field is optional: when omitted the element
/// inherits the board's current style (the "last used wins" style bar state),
/// matching how a user drawing by hand would get styled.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CanvasStyle {
    /// Stroke color as `0xRRGGBB` integer (e.g. `0x1e1e1e`). Omit = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke: Option<u32>,
    /// Fill color as `0xRRGGBB` integer. Omit / null = no fill (transparent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<u32>,
    /// Stroke width in world units. Omit = default (~2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Roughness 0.0 (clean) .. 2.0 (very hand-drawn). Omit = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roughness: Option<f32>,
    /// Dashed vs solid line. Omit = solid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_style: Option<OpStrokeStyle>,
    /// Opacity 0.0 .. 1.0. Omit = fully opaque.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Fill pattern for shape backgrounds: `hachure` (sketch lines, default)
    /// or `solid` (flat chalk-paste block). Omit = hachure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_style: Option<OpFillStyle>,
}

/// Horizontal alignment of a text element's content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpTextAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl From<OpTextAlign> for crate::scene::TextAlign {
    fn from(a: OpTextAlign) -> Self {
        match a {
            OpTextAlign::Left => crate::scene::TextAlign::Left,
            OpTextAlign::Center => crate::scene::TextAlign::Center,
            OpTextAlign::Right => crate::scene::TextAlign::Right,
        }
    }
}

/// A single drawing operation. Variants mirror the whiteboard's element kinds.
/// Coordinates are in world space (see [`OpPoint]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum CanvasOp {
    /// Axis-aligned rectangle.
    Rectangle {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        #[serde(default)]
        style: CanvasStyle,
        /// Optional text label drawn inside the shape (bound label). Empty = no label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Ellipse inscribed in the given box.
    Ellipse {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        #[serde(default)]
        style: CanvasStyle,
        /// Optional text label drawn inside the shape (bound label). Empty = no label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Diamond inscribed in the given box.
    Diamond {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        #[serde(default)]
        style: CanvasStyle,
        /// Optional text label drawn inside the shape (bound label). Empty = no label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Polyline through the given absolute points.
    Line {
        points: Vec<OpPoint>,
        #[serde(default)]
        style: CanvasStyle,
        /// Optional text label on the line (e.g. "是"/"否" on a flow arrow).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Polyline with optional arrowheads.
    Arrow {
        points: Vec<OpPoint>,
        /// Draw an arrowhead at the start (first point).
        #[serde(default)]
        start_arrowhead: bool,
        /// Draw an arrowhead at the end (last point). Defaults to true.
        #[serde(default = "default_true")]
        end_arrowhead: bool,
        #[serde(default)]
        style: CanvasStyle,
        /// Optional text label on the arrow (e.g. "是"/"否" on a flow arrow).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Modify an existing element: move it to new coordinates, change its text,
    /// restyle it, or resize text. `id` is the element's UUID (returned by the
    /// draw tool that created it, or discovered via `list_elements`). All fields
    /// except `id` are optional; omitted style fields keep the element's current
    /// style.
    UpdateElement {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// Optional visual style override. Omitted fields keep current style.
        #[serde(default)]
        style: CanvasStyle,
        /// New font size (text elements only). Omit to keep current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        font_size: Option<f64>,
    },
    /// Delete an element by id (also removes its bound label, if any).
    DeleteElement { id: String },
    /// Delete every element on the canvas (clear it for a fresh start).
    Clear,
    /// Closed polygon through the given absolute points (≥3) — irregular
    /// shapes for the ink-wash style (mountains, land masses). The last
    /// point connects back to the first.
    Polygon {
        points: Vec<OpPoint>,
        #[serde(default)]
        style: CanvasStyle,
    },
    /// Standalone text. `text` may contain newlines for multi-line.
    Text {
        /// Top-left X — or, with `anchor = "center"`, the text's horizontal
        /// CENTER line (e.g. a page's center line for a centered title).
        x: f64,
        y: f64,
        text: String,
        /// Font size in world units (e.g. 16..48). Omit = default.
        #[serde(skip_serializing_if = "Option::is_none")]
        font_size: Option<f64>,
        /// Horizontal alignment within the text box. Omit = left.
        #[serde(skip_serializing_if = "Option::is_none")]
        align: Option<OpTextAlign>,
        /// Font family alias: `handwritten` (default), `kai` (楷体, brush
        /// headings), `hei` (黑体, heavy poster headings), `song` (宋体),
        /// `system`. Omit = handwritten.
        #[serde(skip_serializing_if = "Option::is_none")]
        font_family: Option<String>,
        /// Wrap width in world units: lines longer than this wrap onto the
        /// next line. Strongly recommended for body-text blocks so paragraphs
        /// stay inside their panel. Omit = natural width (no wrapping).
        #[serde(skip_serializing_if = "Option::is_none")]
        wrap_width: Option<f64>,
        /// Positioning anchor: "center" = X is the text's horizontal center
        /// line. Use it for page-centered titles instead of computing
        /// left-edge offsets by character count.
        #[serde(skip_serializing_if = "Option::is_none")]
        anchor: Option<String>,
        #[serde(default)]
        style: CanvasStyle,
    },
    /// Set the canvas surface color (the "board"). `None` restores the
    /// default white board. First move for a blackboard poster.
    SetBackground {
        /// `0xRRGGBB` integer, e.g. `0x2a5240` (chalkboard green).
        /// `null` = white.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<u32>,
    },
    /// A whole mind map from one nested tree. The model supplies only the
    /// texts; every coordinate comes from the deterministic layout in
    /// [`crate::scene::mindmap`] (balanced two-sided tidy tree), so nodes
    /// never overlap and links never cross — the model cannot get the
    /// geometry wrong because it never places anything.
    Mindmap {
        /// The root node of the tree (its children recursively).
        root: OpMindmapNode,
        /// Root center X. Omit = canvas center (800).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cx: Option<f64>,
        /// Root center Y. Omit = canvas center (500).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cy: Option<f64>,
    },
    /// Open a new slide page: a titled world-space rectangle laid out after
    /// the existing pages. The model then draws this page's content inside
    /// the returned rect. Flipping/presenting is a viewer concern; pages are
    /// just regions.
    AddPage {
        /// Page title shown on the page frame and the page bar, e.g. "封面".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Aspect preset: "16:9" (default), "4:3", "9:16", "3:4", "1:1".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ratio: Option<String>,
    },
}

/// One node of a mind map tree (recursive). Leaves omit `children`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpMindmapNode {
    /// Node label: a short keyword phrase (≤ 20 chars, single line).
    pub text: String,
    /// Child branches/leaves. Omit for a leaf node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<OpMindmapNode>,
}

impl From<&OpMindmapNode> for crate::scene::mindmap::MindmapNodeInput {
    fn from(n: &OpMindmapNode) -> Self {
        crate::scene::mindmap::MindmapNodeInput {
            text: n.text.clone(),
            children: n.children.iter().map(Into::into).collect(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Gaudy fill: the magenta-card failure mode from the 9:16 rubric round —
/// the model filled every content card with saturated magenta while
/// *narrating* the mandated pastel blue. Catches the magenta/pink-purple
/// family, pure red, and any ultra-bright ultra-saturated fill. Restrained
/// palettes all pass: chalk pinks (0xFFC0CB), seal red (0xB33A2B), accent
/// blue (0x1a5fd7), pastel card tints (0xE7F0FF). Lives here because
/// `CanvasStyle::validate` enforces it at the tool boundary and the slides
/// rubric re-checks it as a replay sentinel.
pub fn is_gaudy_fill(c: u32) -> bool {
    let r = ((c >> 16) & 0xff) as i32;
    let g = ((c >> 8) & 0xff) as i32;
    let b = (c & 0xff) as i32;
    let magenta_like = r >= 180 && b >= 150 && g <= 140;
    let pure_red = r >= 200 && g <= 90 && b <= 90;
    let spread = r.max(g.max(b)) - r.min(g.min(b));
    let neon = (r + g + b) >= 600 && spread >= 180;
    magenta_like || pure_red || neon
}

impl CanvasOp {
    /// Short human-readable label describing the op, used for the live
    /// "drawing…" status bubble while the agent works.
    #[allow(dead_code)]
    pub fn status_label(&self) -> &'static str {
        match self {
            CanvasOp::Rectangle { .. } => "矩形",
            CanvasOp::Ellipse { .. } => "椭圆",
            CanvasOp::Diamond { .. } => "菱形",
            CanvasOp::Line { .. } => "直线",
            CanvasOp::Arrow { .. } => "箭头",
            CanvasOp::Polygon { .. } => "多边形",
            CanvasOp::Text { .. } => "文本",
            CanvasOp::UpdateElement { .. } => "修改",
            CanvasOp::DeleteElement { .. } => "删除",
            CanvasOp::Clear => "清空",
            CanvasOp::SetBackground { .. } => "底色",
            CanvasOp::Mindmap { .. } => "思维导图",
            CanvasOp::AddPage { .. } => "页面",
        }
    }
}

/// Normalize text from model output: models occasionally double-escape
/// newlines, so the canvas sees the literal two-character sequence
/// backslash + `n` instead of a real line break — which then renders as a
/// visible `\n`. Collapse those into real newlines. Idempotent.
pub fn normalize_text(text: impl Into<String>) -> String {
    const LITERAL_CRLF: &str = "\\r\\n";
    const LITERAL_LF: &str = "\\n";
    text.into()
        .replace(LITERAL_CRLF, "\n")
        .replace(LITERAL_LF, "\n")
}

impl CanvasStyle {
    /// Reject out-of-range color values — models occasionally emit 7-digit
    /// hex, which would render as a garbage color. `None` fields pass.
    pub fn validate(&self) -> Result<(), String> {
        for (name, c) in [("stroke", self.stroke), ("fill", self.fill)] {
            if let Some(c) = c {
                if c > 0xFF_FF_FF {
                    return Err(format!(
                        "{name} 颜色 0x{c:x} 超出 0xRRGGBB 范围（最大 0xFFFFFF）"
                    ));
                }
                // Hard boundary, not just a skill guideline: the model once
                // filled every content card with magenta while *narrating*
                // that it used the mandated pastel blue — prompt-level rules
                // don't hold for color values, rejection with the correct
                // hex does (the draw_mindmap lesson: enforce, don't plead).
                if name == "fill" && is_gaudy_fill(c) {
                    return Err(format!(
                        "fill #{c:06x} 是高饱和刺眼色，已拒绝。大面积填充请用主色的浅色调（蓝 0xE7F0FF / 绿 0xE6F4EA / 紫 0xEDE6FD），或白底 + 主色描边；洋红、纯红、荧光色禁止用作填充"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Overlay this optional style onto a base `ElementStyle`, returning a new
    /// style. Fields left `None` inherit the base — so an AI op that omits the
    /// style draws with the board's current ("last used wins") style.
    pub fn merge_into(self, base: crate::scene::ElementStyle) -> crate::scene::ElementStyle {
        let mut out = base;
        if let Some(stroke) = self.stroke {
            out.stroke = stroke;
        }
        if let Some(fill) = self.fill {
            out.background = Some(fill);
        }
        if let Some(width) = self.stroke_width {
            out.stroke_width = width;
        }
        if let Some(rough) = self.roughness {
            out.roughness = rough;
        }
        if let Some(style) = self.stroke_style {
            out.stroke_style = match style {
                OpStrokeStyle::Solid => crate::scene::StrokeStyle::Solid,
                OpStrokeStyle::Dashed => crate::scene::StrokeStyle::Dashed,
            };
        }
        if let Some(opacity) = self.opacity {
            out.opacity = opacity;
        }
        if let Some(fill_style) = self.fill_style {
            out.fill_style = match fill_style {
                OpFillStyle::Hachure => crate::scene::FillStyle::Hachure,
                OpFillStyle::Solid => crate::scene::FillStyle::Solid,
            };
        }
        out
    }
}

/// Machine-readable failure category for a canvas operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasOpErrorCode {
    /// Arguments failed validation (bad coordinates, non-positive size, etc.).
    InvalidArgs,
    /// A referenced element id does not exist.
    NotFound,
    /// Any other failure (board gone, internal error).
    Internal,
}

/// A failed canvas operation, relayed from the main thread back to the tool.
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasOpError {
    pub code: CanvasOpErrorCode,
    pub message: String,
}

impl CanvasOpError {
    pub fn invalid_args(msg: impl Into<String>) -> Self {
        Self {
            code: CanvasOpErrorCode::InvalidArgs,
            message: msg.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            code: CanvasOpErrorCode::NotFound,
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: CanvasOpErrorCode::Internal,
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for CanvasOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CanvasOpError {}

/// The outcome of applying one canvas op on the main thread: `Ok(message)` is
/// a success description (e.g. "已添加矩形 id=a1b2c3d4"), `Err` is a failure
/// carrying a category code and a human-readable reason.
pub type CanvasOpOutcome = Result<String, CanvasOpError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_op_roundtrips() {
        let op = CanvasOp::Rectangle {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
            style: CanvasStyle {
                fill: Some(0xa5d8ff),
                ..Default::default()
            },
            text: Some("测试".to_string()),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: CanvasOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn arrow_defaults_end_arrowhead_true() {
        // Omitting end_arrowhead keeps the default (true) on deserialize.
        let json = r#"{"shape":"arrow","points":[{"x":0.0,"y":0.0},{"x":50.0,"y":0.0}]}"#;
        let op: CanvasOp = serde_json::from_str(json).unwrap();
        match op {
            CanvasOp::Arrow {
                end_arrowhead,
                start_arrowhead,
                ..
            } => {
                assert!(end_arrowhead);
                assert!(!start_arrowhead);
            }
            _ => panic!("expected arrow"),
        }
    }

    #[test]
    fn normalize_text_unwraps_double_escaped_newlines() {
        // The model wrote \\n inside its JSON string: after JSON parsing the
        // canvas text holds the literal two characters backslash + n.
        // normalize_text collapses those into real newlines.
        let raw = "一块黑板\\n三尺讲台\\n\\n春晖遍四方";
        assert_eq!(raw, "一块黑板\\n三尺讲台\\n\\n春晖遍四方"); // raw is literal
        let out = normalize_text(raw);
        assert_eq!(out, "一块黑板\n三尺讲台\n\n春晖遍四方");
        // Real newlines pass through unchanged (idempotent).
        assert_eq!(normalize_text("a\nb"), "a\nb");
    }

    #[test]
    fn text_op_minimal() {
        let json = r#"{"shape":"text","x":5.0,"y":5.0,"text":"你好","style":{}}"#;
        let op: CanvasOp = serde_json::from_str(json).unwrap();
        assert_eq!(op.status_label(), "文本");
    }

    #[test]
    fn mindmap_op_roundtrips() {
        let op = CanvasOp::Mindmap {
            root: OpMindmapNode {
                text: "根".into(),
                children: vec![OpMindmapNode {
                    text: "叶".into(),
                    children: vec![],
                }],
            },
            cx: None,
            cy: Some(500.0),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: CanvasOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
        assert_eq!(op.status_label(), "思维导图");
        // Leaf nodes omit `children` on the wire.
        let minimal =
            r#"{"shape":"mindmap","root":{"text":"根","children":[{"text":"叶"}]},"cy":500.0}"#;
        let parsed: CanvasOp = serde_json::from_str(minimal).unwrap();
        assert_eq!(parsed, op);
    }

    #[test]
    fn mindmap_op_converts_to_layout_input() {
        let op = CanvasOp::Mindmap {
            root: OpMindmapNode {
                text: "根".into(),
                children: vec![OpMindmapNode {
                    text: "叶".into(),
                    children: vec![],
                }],
            },
            cx: None,
            cy: None,
        };
        let CanvasOp::Mindmap { root, .. } = &op else {
            panic!()
        };
        let input = crate::scene::mindmap::MindmapNodeInput::from(root);
        assert_eq!(crate::scene::mindmap::count_nodes(&input), 2);
        assert_eq!(input.text, "根");
    }
}
