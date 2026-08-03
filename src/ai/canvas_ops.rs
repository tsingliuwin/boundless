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
/// Coordinates are in world space (see [`OpPoint`]).
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
    /// Standalone text. `text` may contain newlines for multi-line.
    Text {
        x: f64,
        y: f64,
        text: String,
        /// Font size in world units (e.g. 16..48). Omit = default.
        #[serde(skip_serializing_if = "Option::is_none")]
        font_size: Option<f64>,
        /// Horizontal alignment within the text box. Omit = left.
        #[serde(skip_serializing_if = "Option::is_none")]
        align: Option<OpTextAlign>,
        #[serde(default)]
        style: CanvasStyle,
    },
}

fn default_true() -> bool {
    true
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
            CanvasOp::Text { .. } => "文本",
        }
    }
}

impl CanvasStyle {
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
        out
    }
}

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
    fn text_op_minimal() {
        let json = r#"{"shape":"text","x":5.0,"y":5.0,"text":"你好","style":{}}"#;
        let op: CanvasOp = serde_json::from_str(json).unwrap();
        assert_eq!(op.status_label(), "文本");
    }
}
