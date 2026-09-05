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

/// How shape backgrounds are filled: hachure sketch lines (default), dense
/// overlapping hachure (near-solid), or a solid flat block — the "chalk
/// paste" panels of a blackboard poster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpFillStyle {
    #[default]
    Hachure,
    Dense,
    Solid,
    Watercolor,
    Gradient,
}

/// Optional visual style. Every field is optional: when omitted the element
/// inherits the board's current style (the "last used wins" style bar state),
/// matching how a user drawing by hand would get styled.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CanvasStyle {
    /// Stroke color: 0xRRGGBB as a decimal integer (e.g. `0x1e1e1e` =
    /// 1973790) or a hex string (`"0x1e1e1e"` / `"#1e1e1e"`). Omit = default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_color"
    )]
    pub stroke: Option<u32>,
    /// Fill color: 0xRRGGBB as a decimal integer (e.g. `0xE7F0FF` =
    /// 15200511) or a hex string (`"0xE7F0FF"` / `"#E7F0FF"`). Omit / null =
    /// no fill (transparent).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_color"
    )]
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
    /// Fill pattern for shape backgrounds: `hachure` (sketch lines, default),
    /// `dense` (overlapping lines), `solid` (near-flat block), `watercolor`
    /// (layered wash with edge pooling), or `gradient` (vertical fade into a
    /// darkened shade — sky panels, depth). Omit = hachure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_style: Option<OpFillStyle>,
    /// Draw a line/arrow as a smooth curve through the points (waves,
    /// rivers, smiles) instead of straight segments. Omit = straight.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smooth: Option<bool>,
    /// Fine-grained fill tuning (overrides the fill_style preset): fill line
    /// spacing in world units (2~6 reads best). Omit = preset value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hachure_gap: Option<f64>,
    /// Fill line stroke width in world units; >= 2x the gap reads nearly
    /// solid. Omit = preset value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_weight: Option<f64>,
    /// Fill line angle in degrees (default -41). Omit = preset value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hachure_angle: Option<f64>,
    /// Draw a soft hand-drawn hachure shadow under the shape (offset from
    /// the outline). `false` removes an existing shadow. Omit = keep.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow: Option<bool>,
    /// Shadow offset from the shape outline, world units. Default 10/12.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_dx: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_dy: Option<f64>,
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
        #[serde(default, deserialize_with = "de_style")]
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
        #[serde(default, deserialize_with = "de_style")]
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
        #[serde(default, deserialize_with = "de_style")]
        style: CanvasStyle,
        /// Optional text label drawn inside the shape (bound label). Empty = no label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Polyline through the given absolute points.
    Line {
        points: Vec<OpPoint>,
        #[serde(default, deserialize_with = "de_style")]
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
        #[serde(default, deserialize_with = "de_style")]
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
        #[serde(default, deserialize_with = "de_style")]
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
    /// point connects back to the first. With `smooth` the outline is a
    /// closed Catmull-Rom spline through the points (organic blobs:
    /// petals, clouds, pebbles) instead of straight edges.
    Polygon {
        points: Vec<OpPoint>,
        #[serde(default)]
        smooth: bool,
        #[serde(default, deserialize_with = "de_style")]
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
        #[serde(default, deserialize_with = "de_style")]
        style: CanvasStyle,
    },
    /// Set the paper-grain material layered over the canvas: ink on textured
    /// paper instead of vectors on glass. Pairs well with set_canvas_background.
    SetTexture {
        /// `grain` (水彩纸细纹) / `kraft` (牛皮纸纤维) / `chalkboard` (黑板
        /// 粉尘) / `none` (移除材质，恢复纯面). Required.
        #[serde(default)]
        texture: Option<Option<crate::scene::PaperTexture>>,
    },
    /// Set the canvas surface color (the "board"). `None` restores the
    /// default white board. First move for a blackboard poster.
    SetBackground {
        /// Surface color: 0xRRGGBB as a decimal integer (e.g. `0x2a5240` =
        /// 2773568, chalkboard green) or a hex string (`"0x2a5240"` /
        /// `"#2a5240"`). `null` = white.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_color"
        )]
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
        /// Transition used when this page appears (flip into it / show
        /// opener): "slide" (default, camera glides in), "fade" (through
        /// black), "none" (hard cut).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<String>,
    },
    /// Delete a slide page frame. Elements on that page stay on the canvas
    /// (the model can redraw or the user can clean up) — same semantics as
    /// the page bar's manual delete.
    DeletePage {
        /// 1-based page number to delete. Omit = the last page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
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

/// Parse a color in the string forms models actually emit: `#RRGGBB`,
/// `0xRRGGBB`, bare `RRGGBB` (case-insensitive), CSS short `#RGB`, and
/// 8-digit hex with an alpha pair — `#RRGGBBAA` (alpha last) / `0xAARRGGBB`
/// (alpha first); the alpha pair is dropped because opacity is a separate
/// style field. Everything else is a corrective error the model can retry
/// against.
pub fn parse_color_hex(s: &str) -> Result<u32, String> {
    let err = || {
        format!("无法识别的颜色 \"{s}\"：请用 0xRRGGBB 整数，或 \"#RRGGBB\" / \"0xRRGGBB\" 字符串")
    };
    let t = s.trim();
    let t = t.strip_prefix('#').unwrap_or(t);
    if let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return match rest.len() {
            6 => u32::from_str_radix(rest, 16).map_err(|_| err()),
            8 => u32::from_str_radix(&rest[2..], 16).map_err(|_| err()),
            _ => Err(err()),
        };
    }
    match t.len() {
        3 => {
            let expanded: String = t.chars().flat_map(|c| [c, c]).collect();
            u32::from_str_radix(&expanded, 16).map_err(|_| err())
        }
        6 => u32::from_str_radix(t, 16).map_err(|_| err()),
        8 => u32::from_str_radix(&t[..6], 16).map_err(|_| err()),
        _ => Err(err()),
    }
}

/// serde `deserialize_with` for model-supplied color fields. The schema says
/// "0xRRGGBB integer", but models frequently answer with the *string*
/// `"0xE7F0FF"` / `"#E7F0FF"`, which a bare `u32` rejects — failing the whole
/// tool call until the model gives up on colors entirely (observed: a whole
/// slide deck drawn with `style: {}` after exactly such retries). Accept the
/// decimal number, the hex string, and null/missing alike.
pub fn de_color<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<serde_json::Value>::deserialize(deserializer)?;
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => {
            const MAX: u64 = 0xFF_FF_FF;
            if let Some(u) = n.as_u64() {
                if u <= MAX {
                    Ok(Some(u as u32))
                } else {
                    Err(serde::de::Error::custom(format!(
                        "颜色 {u} 超出 0xRRGGBB 范围（最大 0xFFFFFF）"
                    )))
                }
            } else if let Some(f) = n.as_f64() {
                // Models occasionally emit colors as floats (1.5187711e7).
                if f.fract() == 0.0 && f >= 0.0 && f <= MAX as f64 {
                    Ok(Some(f as u32))
                } else {
                    Err(serde::de::Error::custom(format!(
                        "颜色 {f} 超出 0xRRGGBB 范围（最大 0xFFFFFF）"
                    )))
                }
            } else {
                Err(serde::de::Error::custom(
                    "颜色必须是 0xRRGGBB 整数或十六进制字符串",
                ))
            }
        }
        Some(serde_json::Value::String(s)) => parse_color_hex(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(other) => Err(serde::de::Error::custom(format!(
            "颜色必须是 0xRRGGBB 整数或十六进制字符串，收到 {other}"
        ))),
    }
}

/// serde `deserialize_with` for `style` fields. Some models (observed with
/// glm-5.3-flash) serialize the nested style object as a *string* —
/// `"style": "{\"fill\":...}"` — which a plain struct deserializer rejects,
/// failing the whole tool call invisibly (before the tool runs, so nothing
/// reaches the log or the UI) until the model concludes style "cannot be
/// passed" and draws everything unstyled. Accept the double-encoded form by
/// parsing the string as JSON first; objects and null behave as before.
pub fn de_style<'de, D>(deserializer: D) -> Result<CanvasStyle, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::String(s) => serde_json::from_str::<CanvasStyle>(&s).map_err(|e| {
            serde::de::Error::custom(format!(
                "style 应为对象（或对象的 JSON 字符串形式），解析失败：{e}"
            ))
        }),
        serde_json::Value::Null => Ok(CanvasStyle::default()),
        other => serde_json::from_value(other)
            .map_err(|e| serde::de::Error::custom(format!("style 解析失败：{e}"))),
    }
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
            CanvasOp::SetTexture { .. } => "纸纹",
            CanvasOp::Mindmap { .. } => "思维导图",
            CanvasOp::AddPage { .. } => "页面",
            CanvasOp::DeletePage { .. } => "删页",
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
                OpFillStyle::Dense => crate::scene::FillStyle::Dense,
                OpFillStyle::Solid => crate::scene::FillStyle::Solid,
                OpFillStyle::Watercolor => crate::scene::FillStyle::Watercolor,
                OpFillStyle::Gradient => crate::scene::FillStyle::Gradient,
            };
        }
        if let Some(smooth) = self.smooth {
            out.line_type = if smooth {
                crate::scene::LineType::Curved
            } else {
                crate::scene::LineType::Straight
            };
        }
        if let Some(v) = self.hachure_gap {
            out.hachure_gap = Some(v);
        }
        if let Some(v) = self.fill_weight {
            out.fill_weight = Some(v);
        }
        if let Some(v) = self.hachure_angle {
            out.hachure_angle = Some(v);
        }
        match self.shadow {
            Some(on) => {
                out.shadow = on.then_some(crate::scene::Shadow {
                    dx: self.shadow_dx.unwrap_or(10.0),
                    dy: self.shadow_dy.unwrap_or(12.0),
                });
            }
            None => {
                if let (Some(dx), Some(dy)) = (self.shadow_dx, self.shadow_dy) {
                    out.shadow = Some(crate::scene::Shadow { dx, dy });
                }
            }
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
    fn fill_accepts_number_and_hex_string_forms() {
        // The DuckDB-slide failure: models answer the "0xRRGGBB integer"
        // schema with the *string* "0xE7F0FF" — the bare u32 deserializer
        // rejected the whole tool call and the model gave up on fills.
        for form in ["15200511", "\"0xE7F0FF\"", "\"#E7F0FF\"", "\"e7f0ff\""] {
            let style: CanvasStyle = serde_json::from_str(&format!("{{\"fill\":{form}}}"))
                .unwrap_or_else(|e| panic!("form {form}: {e}"));
            assert_eq!(style.fill, Some(0xE7_F0_FF), "form {form}");
        }
    }

    #[test]
    fn fill_accepts_null_and_omission() {
        let style: CanvasStyle = serde_json::from_str("{\"fill\":null}").unwrap();
        assert_eq!(style.fill, None);
        let style: CanvasStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(style.fill, None);
    }

    #[test]
    fn fill_drops_alpha_pair() {
        // #RRGGBBAA (alpha last) and 0xAARRGGBB (alpha first): opacity is a
        // separate style field, so strip the alpha instead of rejecting.
        let style: CanvasStyle = serde_json::from_str("{\"fill\":\"#E7F0FFCC\"}").unwrap();
        assert_eq!(style.fill, Some(0xE7_F0_FF));
        let style: CanvasStyle = serde_json::from_str("{\"fill\":\"0xFFE7F0FF\"}").unwrap();
        assert_eq!(style.fill, Some(0xE7_F0_FF));
    }

    #[test]
    fn fill_expands_short_hex() {
        let style: CanvasStyle = serde_json::from_str("{\"fill\":\"#FFF\"}").unwrap();
        assert_eq!(style.fill, Some(0xFF_FF_FF));
    }

    #[test]
    fn fill_rejects_garbage_with_corrective_message() {
        for form in ["\"红色\"", "\"rgb(230,240,255)\"", "true", "-5"] {
            let err = serde_json::from_str::<CanvasStyle>(&format!("{{\"fill\":{form}}}"))
                .expect_err(form);
            assert!(err.to_string().contains("0xRRGGBB"), "{form}: {err}");
        }
    }

    #[test]
    fn fill_out_of_range_number_still_rejected() {
        let err = serde_json::from_str::<CanvasStyle>("{\"fill\":4294967295}").unwrap_err();
        assert!(err.to_string().contains("超出"), "{err}");
    }

    #[test]
    fn set_background_accepts_string_color() {
        let op: CanvasOp =
            serde_json::from_str("{\"shape\":\"set_background\",\"color\":\"#2a5240\"}").unwrap();
        match op {
            CanvasOp::SetBackground { color } => assert_eq!(color, Some(0x2A_52_40)),
            _ => panic!("expected set_background"),
        }
    }

    #[test]
    fn style_accepts_double_encoded_json_string() {
        // glm-5.3-flash serializes the nested style object as a string; the
        // whole tool call then failed invisibly (before the tool ran) and the
        // model declared style "un-passable", drawing everything unstyled.
        let op: CanvasOp = serde_json::from_str(
            r##"{"shape":"rectangle","x":0,"y":0,"w":10,"h":10,"style":"{\"fill\":\"#E7F0FF\"}"}"##,
        )
        .unwrap();
        match op {
            CanvasOp::Rectangle { style, .. } => assert_eq!(style.fill, Some(0xE7_F0_FF)),
            _ => panic!("expected rectangle"),
        }
    }

    #[test]
    fn style_null_and_garbage_string() {
        let op: CanvasOp =
            serde_json::from_str(r#"{"shape":"rectangle","x":0,"y":0,"w":10,"h":10,"style":null}"#)
                .unwrap();
        match op {
            CanvasOp::Rectangle { style, .. } => assert_eq!(style, CanvasStyle::default()),
            _ => panic!("expected rectangle"),
        }
        // A string that isn't a style object is a corrective error, not silence.
        let err = serde_json::from_str::<CanvasOp>(
            r#"{"shape":"rectangle","x":0,"y":0,"w":10,"h":10,"style":"随便什么"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("style"), "{err}");
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
