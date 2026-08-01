//! rig `Tool` implementations that let the AI agent draw on the canvas.
//!
//! Each tool runs on the tokio runtime (rig executes tool calls there) but the
//! canvas lives on the GPUI main thread. So a tool never touches the board
//! directly: it sends a [`CanvasOp`] through a futures channel, and the AI panel
//! drains that channel on the main thread to mutate the scene. `call()` returns
//! a short confirmation string that the model sees as the tool result.
//!
//! The `Tool` trait shape (verified against rig-core 0.38.2):
//! `const NAME`, `type Args` (de+JsonSchema), `type Output` (Serialize),
//! `definition(&self, prompt) -> ToolDefinition`, `call(&self, args) -> Output`.

use futures::channel::mpsc::UnboundedSender;
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use schemars::JsonSchema;
use serde::Deserialize;

use super::canvas_ops::{CanvasOp, CanvasStyle, OpPoint, OpTextAlign};

/// Shared error type for all drawing tools. Tools are infallible in practice
/// (sending to an unbounded channel only fails if the receiver was dropped,
/// i.e. the request was cancelled), so this is a thin wrapper. Implemented by
/// hand rather than via `thiserror` (not a dependency of this crate).
#[derive(Debug)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ToolError {}

// ---------------------------------------------------------------------------
// A generic helper: each tool holds a sender and forwards a constructed op.
// We implement `Tool` per-kind because rig dispatches by type, and `Args`
// differs per shape (rect needs w/h, line needs points, text needs text…).
// ---------------------------------------------------------------------------

/// Arguments shared by the four box shapes (rectangle / ellipse / diamond).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoxArgs {
    /// Top-left X in world coordinates.
    pub x: f64,
    /// Top-left Y in world coordinates.
    pub y: f64,
    /// Width in world units.
    pub w: f64,
    /// Height in world units.
    pub h: f64,
    /// Optional visual style. Omitted fields inherit the board's current style.
    #[serde(default)]
    pub style: CanvasStyle,
}

/// Arguments for line / arrow tools.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PointsArgs {
    /// Two or more points the line/arrow passes through, in world coordinates,
    /// in order from start to end.
    pub points: Vec<OpPoint>,
    /// Draw an arrowhead at the first point.
    #[serde(default)]
    pub start_arrowhead: bool,
    /// Draw an arrowhead at the last point. Defaults to true (arrow tools only).
    #[serde(default = "default_true")]
    pub end_arrowhead: bool,
    /// Optional visual style.
    #[serde(default)]
    pub style: CanvasStyle,
}

/// Arguments for the text tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TextArgs {
    /// Top-left X in world coordinates.
    pub x: f64,
    /// Top-left Y in world coordinates.
    pub y: f64,
    /// The text content. May contain `\n` for multiple lines.
    pub text: String,
    /// Font size in world units (typical 16..48). Omit for default.
    #[serde(default)]
    pub font_size: Option<f64>,
    /// Horizontal alignment. Omit for left.
    #[serde(default)]
    pub align: Option<OpTextAlign>,
    /// Optional visual style (opacity etc.).
    #[serde(default)]
    pub style: CanvasStyle,
}

fn default_true() -> bool {
    true
}

fn send_op(sender: &UnboundedSender<CanvasOp>, op: CanvasOp) -> Result<String, ToolError> {
    sender
        .unbounded_send(op)
        .map_err(|_| ToolError("画布通道已关闭（请求已取消）".into()))?;
    Ok("已添加到画布".to_string())
}

/// Build a `ToolDefinition` with the given name/description and a schema
/// generated from the tool's `Args` type.
fn tool_def<T: JsonSchema>(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::to_value(schemars::schema_for!(T))
            .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
    }
}

// --- Rectangle -------------------------------------------------------------

pub struct RectangleTool {
    pub sender: UnboundedSender<CanvasOp>,
}

impl Tool for RectangleTool {
    const NAME: &'static str = "draw_rectangle";
    type Error = ToolError;
    type Args = BoxArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<BoxArgs>(
            Self::NAME,
            "在画布上画一个矩形。x/y 是左上角，w/h 是宽高（世界坐标）。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let res = send_op(
            &self.sender,
            CanvasOp::Rectangle {
                x: args.x,
                y: args.y,
                w: args.w,
                h: args.h,
                style: args.style,
            },
        );
        async move { res }
    }
}

// --- Ellipse ---------------------------------------------------------------

pub struct EllipseTool {
    pub sender: UnboundedSender<CanvasOp>,
}

impl Tool for EllipseTool {
    const NAME: &'static str = "draw_ellipse";
    type Error = ToolError;
    type Args = BoxArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<BoxArgs>(
            Self::NAME,
            "在画布上画一个椭圆，内接于 x/y/w/h 定义的矩形框。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let res = send_op(
            &self.sender,
            CanvasOp::Ellipse {
                x: args.x,
                y: args.y,
                w: args.w,
                h: args.h,
                style: args.style,
            },
        );
        async move { res }
    }
}

// --- Diamond ---------------------------------------------------------------

pub struct DiamondTool {
    pub sender: UnboundedSender<CanvasOp>,
}

impl Tool for DiamondTool {
    const NAME: &'static str = "draw_diamond";
    type Error = ToolError;
    type Args = BoxArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<BoxArgs>(
            Self::NAME,
            "在画布上画一个菱形，内接于 x/y/w/h 定义的矩形框。常用于流程图判断节点。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let res = send_op(
            &self.sender,
            CanvasOp::Diamond {
                x: args.x,
                y: args.y,
                w: args.w,
                h: args.h,
                style: args.style,
            },
        );
        async move { res }
    }
}

// --- Line ------------------------------------------------------------------

pub struct LineTool {
    pub sender: UnboundedSender<CanvasOp>,
}

impl Tool for LineTool {
    const NAME: &'static str = "draw_line";
    type Error = ToolError;
    type Args = PointsArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<PointsArgs>(
            Self::NAME,
            "在画布上画一条折线（无箭头），经过给定的若干点（世界坐标，至少两点）。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let res = send_op(
            &self.sender,
            CanvasOp::Line {
                points: args.points,
                style: args.style,
            },
        );
        async move { res }
    }
}

// --- Arrow -----------------------------------------------------------------

pub struct ArrowTool {
    pub sender: UnboundedSender<CanvasOp>,
}

impl Tool for ArrowTool {
    const NAME: &'static str = "draw_arrow";
    type Error = ToolError;
    type Args = PointsArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<PointsArgs>(
            Self::NAME,
            "在画布上画一个带箭头的折线，连接两点或多点。默认在末端画箭头（end_arrowhead=true）。常用于流程图/示意图中表示方向。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let res = send_op(
            &self.sender,
            CanvasOp::Arrow {
                points: args.points,
                start_arrowhead: args.start_arrowhead,
                end_arrowhead: args.end_arrowhead,
                style: args.style,
            },
        );
        async move { res }
    }
}

// --- Text ------------------------------------------------------------------

pub struct TextTool {
    pub sender: UnboundedSender<CanvasOp>,
}

impl Tool for TextTool {
    const NAME: &'static str = "draw_text";
    type Error = ToolError;
    type Args = TextArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<TextArgs>(
            Self::NAME,
            "在画布上添加文本。x/y 是左上角，text 可含换行。用于标签、标题、节点说明等。颜色、流程图箭头方向等说明性内容也用文字表示。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let res = send_op(
            &self.sender,
            CanvasOp::Text {
                x: args.x,
                y: args.y,
                text: args.text,
                font_size: args.font_size,
                align: args.align,
                style: args.style,
            },
        );
        async move { res }
    }
}

/// Build all drawing tools sharing one canvas-op channel. Registered onto the
/// rig agent's toolset; each tool forwards its op through `sender`.
pub fn all_tools(sender: UnboundedSender<CanvasOp>) -> Vec<Box<dyn rig_core::tool::ToolDyn>> {
    // Each tool is coerced to `Box<dyn ToolDyn>` (the return type names the
    // trait, so no import is needed here). `AgentBuilder::tools` accepts this.
    vec![
        Box::new(RectangleTool {
            sender: sender.clone(),
        }),
        Box::new(EllipseTool {
            sender: sender.clone(),
        }),
        Box::new(DiamondTool {
            sender: sender.clone(),
        }),
        Box::new(LineTool {
            sender: sender.clone(),
        }),
        Box::new(ArrowTool {
            sender: sender.clone(),
        }),
        Box::new(TextTool { sender }),
    ]
}
