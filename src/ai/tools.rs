//! rig `Tool` implementations that let the AI agent draw on the canvas.
//!
//! **Design (mirrors lakemind):** each tool owns its full lifecycle. Inside
//! `call()` it emits a three-phase event sequence through the shared event
//! channel — `ToolCall` (open a pending step) → `CanvasOp` (apply the drawing)
//! → `ToolResult` (mark the step done). rig's own tool-call / tool-result
//! stream items are ignored on the agent side; the tool is the single source
//! of truth for its lifecycle. The `call()` return value is a compact string
//! fed back to the model ("已添加到画布"); the rich UI data (coordinates,
//! colors…) flows via the `ToolCall` event's `args`.
//!
//! Tools run on the tokio runtime (rig executes them there); the canvas lives
//! on the GPUI main thread. `AgentEvent::CanvasOp` is drained on the main
//! thread to mutate the scene.

use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::agent::{next_tool_id, AgentEvent};
use super::canvas_ops::{CanvasOp, CanvasStyle, OpPoint, OpTextAlign};
use super::client::ChatMessage;

/// Generate a new element UUID (used by draw tools so the id can be reported
/// back to the model before the element is created on the main thread).
fn new_element_id() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

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
// A generic helper: each tool holds an event sender and emits a three-phase
// lifecycle (call → canvas-op → result) before returning the model-facing
// string. This makes every tool call a clean, self-contained execution unit.
// ---------------------------------------------------------------------------

/// The compact string returned to the model for a successful draw, carrying
/// the element's short id so the model can reference it in update/delete calls.
fn tool_ok_result(element_id: uuid::Uuid) -> String {
    format!("已添加到画布，id={}", &element_id.to_string()[..8])
}

/// Emit the full tool-call lifecycle: open a pending step, apply the canvas
/// op, then mark the step done. All three events share the same `id` so the
/// UI can pair them. `args_json` is the re-serialized arguments the model
/// supplied (kept for the UI's expandable detail view). `element_id` is the
/// pre-generated UUID for the new element — reported back to the model in the
/// result string so it can update/delete the element later.
fn emit_tool_step(
    events: &UnboundedSender<AgentEvent>,
    id: String,
    name: &str,
    args_json: Value,
    op: CanvasOp,
    element_id: uuid::Uuid,
) {
    let _ = events.unbounded_send(AgentEvent::ToolCall {
        id: id.clone(),
        name: name.to_string(),
        args: args_json,
    });
    let _ = events.unbounded_send(AgentEvent::CanvasOp {
        op,
        pre_assigned_id: Some(element_id),
    });
    let _ = events.unbounded_send(AgentEvent::ToolResult {
        id,
        result: tool_ok_result(element_id),
    });
}

/// Emit a tool-call lifecycle for non-create ops (update/delete) that don't
/// create a new element. The result string confirms the operation.
fn emit_tool_action(
    events: &UnboundedSender<AgentEvent>,
    id: String,
    name: &str,
    args_json: Value,
    op: CanvasOp,
    result: &str,
) {
    let _ = events.unbounded_send(AgentEvent::ToolCall {
        id: id.clone(),
        name: name.to_string(),
        args: args_json,
    });
    let _ = events.unbounded_send(AgentEvent::CanvasOp {
        op,
        pre_assigned_id: None,
    });
    let _ = events.unbounded_send(AgentEvent::ToolResult {
        id,
        result: result.to_string(),
    });
}

/// Arguments shared by the four box shapes (rectangle / ellipse / diamond).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
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
    /// Optional text to draw inside the shape (e.g. "登录" inside an ellipse,
    /// "是否为空?" inside a diamond). The text is centered and follows the
    /// shape when moved. Omit for a shape without a label.
    #[serde(default)]
    pub text: Option<String>,
}

/// Arguments for line / arrow tools.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
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
    /// Optional text label on the line/arrow (e.g. "是"/"否" on a flow arrow).
    /// The label is centered on the line and follows it when moved.
    #[serde(default)]
    pub text: Option<String>,
}

/// Arguments for the text tool.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
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
    pub events: UnboundedSender<AgentEvent>,
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
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::Rectangle {
            x: args.x,
            y: args.y,
            w: args.w,
            h: args.h,
            style: args.style,
            text: args.text,
        };
        let element_id = new_element_id();
        let result = tool_ok_result(element_id);
        emit_tool_step(&self.events, id, Self::NAME, args_json, op, element_id);
        async move { Ok(result) }
    }
}

// --- Ellipse ---------------------------------------------------------------

pub struct EllipseTool {
    pub events: UnboundedSender<AgentEvent>,
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
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::Ellipse {
            x: args.x,
            y: args.y,
            w: args.w,
            h: args.h,
            style: args.style,
            text: args.text,
        };
        let element_id = new_element_id();
        let result = tool_ok_result(element_id);
        emit_tool_step(&self.events, id, Self::NAME, args_json, op, element_id);
        async move { Ok(result) }
    }
}

// --- Diamond ---------------------------------------------------------------

pub struct DiamondTool {
    pub events: UnboundedSender<AgentEvent>,
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
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::Diamond {
            x: args.x,
            y: args.y,
            w: args.w,
            h: args.h,
            style: args.style,
            text: args.text,
        };
        let element_id = new_element_id();
        let result = tool_ok_result(element_id);
        emit_tool_step(&self.events, id, Self::NAME, args_json, op, element_id);
        async move { Ok(result) }
    }
}

// --- Line ------------------------------------------------------------------

pub struct LineTool {
    pub events: UnboundedSender<AgentEvent>,
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
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::Line {
            points: args.points,
            style: args.style,
            text: args.text,
        };
        let element_id = new_element_id();
        let result = tool_ok_result(element_id);
        emit_tool_step(&self.events, id, Self::NAME, args_json, op, element_id);
        async move { Ok(result) }
    }
}

// --- Arrow -----------------------------------------------------------------

pub struct ArrowTool {
    pub events: UnboundedSender<AgentEvent>,
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
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::Arrow {
            points: args.points,
            start_arrowhead: args.start_arrowhead,
            end_arrowhead: args.end_arrowhead,
            style: args.style,
            text: args.text,
        };
        let element_id = new_element_id();
        let result = tool_ok_result(element_id);
        emit_tool_step(&self.events, id, Self::NAME, args_json, op, element_id);
        async move { Ok(result) }
    }
}

// --- Text ------------------------------------------------------------------

pub struct TextTool {
    pub events: UnboundedSender<AgentEvent>,
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
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::Text {
            x: args.x,
            y: args.y,
            text: args.text,
            font_size: args.font_size,
            align: args.align,
            style: args.style,
        };
        let element_id = new_element_id();
        let result = tool_ok_result(element_id);
        emit_tool_step(&self.events, id, Self::NAME, args_json, op, element_id);
        async move { Ok(result) }
    }
}

// --- Update Element --------------------------------------------------------

/// Arguments for updating an existing element.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct UpdateElementArgs {
    /// The element's id (returned by the draw tool, 8-char prefix).
    pub id: String,
    /// New top-left X. Omit to keep current position.
    #[serde(default)]
    pub x: Option<f64>,
    /// New top-left Y. Omit to keep current position.
    #[serde(default)]
    pub y: Option<f64>,
    /// New text content (for shapes/lines/arrows with labels, or standalone
    /// text). Omit to keep current text.
    #[serde(default)]
    pub text: Option<String>,
}

pub struct UpdateElementTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for UpdateElementTool {
    const NAME: &'static str = "update_element";
    type Error = ToolError;
    type Args = UpdateElementArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<UpdateElementArgs>(
            Self::NAME,
            "修改已有元素的位置或文字。id 是创建时返回的元素 ID。可单独修改 x/y（移动）或 text（改文字）。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::UpdateElement {
            id: args.id,
            x: args.x,
            y: args.y,
            text: args.text,
        };
        emit_tool_action(&self.events, id, Self::NAME, args_json, op, "已更新");
        async move { Ok("已更新".to_string()) }
    }
}

// --- Delete Element --------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct DeleteElementArgs {
    /// The element's id (returned by the draw tool, 8-char prefix).
    pub id: String,
}

pub struct DeleteElementTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for DeleteElementTool {
    const NAME: &'static str = "delete_element";
    type Error = ToolError;
    type Args = DeleteElementArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<DeleteElementArgs>(
            Self::NAME,
            "删除画布上的一个元素（及其绑定的文字标签）。id 是创建时返回的元素 ID。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let id = next_tool_id(Self::NAME);
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let op = CanvasOp::DeleteElement { id: args.id };
        emit_tool_action(&self.events, id, Self::NAME, args_json, op, "已删除");
        async move { Ok("已删除".to_string()) }
    }
}

// --- List Elements ---------------------------------------------------------

/// A lightweight summary of one canvas element, for the `list_elements` tool.
#[derive(Clone, Debug)]
pub struct ElementSnapshot {
    pub id: String,
    pub kind: String,
    pub text: Option<String>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl ElementSnapshot {
    /// One-line summary for the model, e.g. "ellipse id=a1b2c3d4 text=开始 (350,40) 120×50".
    fn summary(&self) -> String {
        let text_part = self
            .text
            .as_ref()
            .map(|t| format!(" text={t}"))
            .unwrap_or_default();
        format!(
            "{} id={}{} ({:.0},{:.0}) {:.0}x{:.0}",
            self.kind, self.id, text_part, self.x, self.y, self.w, self.h
        )
    }
}

pub struct ListElementsTool {
    pub snapshot: Arc<Vec<ElementSnapshot>>,
}

impl Tool for ListElementsTool {
    const NAME: &'static str = "list_elements";
    type Error = ToolError;
    type Args = NoArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<NoArgs>(
            Self::NAME,
            "列出画布上当前所有元素的 ID、类型、文字和位置。用于查询已有元素以便修改或删除。",
        );
        async move { def }
    }

    fn call(
        &self,
        _args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let result = if self.snapshot.is_empty() {
            "画布为空".to_string()
        } else {
            let lines: Vec<String> = self.snapshot.iter().enumerate().map(|(i, e)| {
                format!("{}. {}", i + 1, e.summary())
            }).collect();
            lines.join("\n")
        };
        async move { Ok(result) }
    }
}

/// Empty args for tools that take no parameters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct NoArgs {}

/// Build all tools sharing one event channel + a canvas snapshot for
/// `list_elements`. Each draw tool emits its own `ToolCall`/`CanvasOp`/
/// `ToolResult` lifecycle through `events`.
pub fn all_tools(
    events: UnboundedSender<AgentEvent>,
    snapshot: Arc<Vec<ElementSnapshot>>,
) -> Vec<Box<dyn rig_core::tool::ToolDyn>> {
    vec![
        Box::new(RectangleTool {
            events: events.clone(),
        }),
        Box::new(EllipseTool {
            events: events.clone(),
        }),
        Box::new(DiamondTool {
            events: events.clone(),
        }),
        Box::new(LineTool {
            events: events.clone(),
        }),
        Box::new(ArrowTool {
            events: events.clone(),
        }),
        Box::new(TextTool {
            events: events.clone(),
        }),
        Box::new(UpdateElementTool {
            events: events.clone(),
        }),
        Box::new(DeleteElementTool {
            events: events.clone(),
        }),
        Box::new(ListElementsTool { snapshot }),
    ]
}

// Keep the ChatMessage import referenced — it's part of the module's public
// surface (re-exported via the agent) even if not directly used here.
#[allow(dead_code)]
fn _chat_message_referenced(_: &ChatMessage) {}
