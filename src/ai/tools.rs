//! rig `Tool` implementations that let the AI agent draw on the canvas.
//!
//! **Design (mirrors the harness):** each tool owns its full lifecycle AND
//! returns the authoritative outcome. `call()` validates its arguments first
//! (fail loud at the boundary), opens a pending step, sends one `CanvasOp` to
//! the main thread and awaits the apply result — so the tool never reports
//! success for a no-op or a failure. The string fed back to the model is the
//! actual outcome, not a guess.
//!
//! Tools run on the tokio runtime (rig executes them there); the canvas lives
//! on the GPUI main thread. `AgentEvent::CanvasOp` carries a oneshot reply
//! channel the main thread uses to return the apply outcome.

use std::sync::{Arc, Mutex};

use futures::channel::mpsc::UnboundedSender;
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::agent::{next_tool_id, AgentEvent};
use super::canvas_ops::{
    CanvasOp, CanvasOpError, CanvasOpErrorCode, CanvasOpOutcome, CanvasStyle, OpMindmapNode,
    OpPoint, OpTextAlign,
};
use super::client::ChatMessage;

/// Generate a new element UUID (used by draw tools so the id can be reported
/// back to the model before the element is created on the main thread).
fn new_element_id() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

/// Machine-facing error type for a tool call. Carries a category code so the
/// model (and any future UI) can distinguish "bad arguments" from "not found"
/// from "internal failure"; `Display` is the human-readable message the model
/// sees. Implemented by hand rather than via `thiserror` (not a dependency).
#[derive(Debug, Clone)]
pub struct ToolError {
    /// Failure category for structured handling; the model-facing message is
    /// [`Self::message`]. Not yet consumed at runtime.
    #[allow(dead_code)]
    pub code: CanvasOpErrorCode,
    pub message: String,
}

impl ToolError {
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
    fn from_op(e: CanvasOpError) -> Self {
        Self {
            code: e.code,
            message: e.message,
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

// ---------------------------------------------------------------------------
// Argument validation (fail loud at the boundary). The apply path re-checks
// these defensively, but validating here gives the model fast, precise feedback
// instead of a silent no-op.
// ---------------------------------------------------------------------------

fn validate_box(args: &BoxArgs) -> Result<(), ToolError> {
    if !args.x.is_finite() || !args.y.is_finite() || !args.w.is_finite() || !args.h.is_finite() {
        return Err(ToolError::invalid_args("坐标和宽高必须是有限数值"));
    }
    if args.w <= 0.0 || args.h <= 0.0 {
        return Err(ToolError::invalid_args("宽高必须为正数"));
    }
    args.style.validate().map_err(ToolError::invalid_args)?;
    Ok(())
}

fn validate_points(points: &[OpPoint]) -> Result<(), ToolError> {
    if points.len() < 2 {
        return Err(ToolError::invalid_args("至少需要两个坐标点"));
    }
    if points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return Err(ToolError::invalid_args("坐标点必须是有限数值"));
    }
    // Style is validated at the call sites that carry one (draw_line/draw_arrow
    // share PointsArgs with the style field on the tool args).
    Ok(())
}

fn validate_text(args: &TextArgs) -> Result<(), ToolError> {
    if args.text.trim().is_empty() {
        return Err(ToolError::invalid_args("文本内容不能为空"));
    }
    if !args.x.is_finite() || !args.y.is_finite() {
        return Err(ToolError::invalid_args("坐标必须是有限数值"));
    }
    if let Some(fs) = args.font_size {
        if !fs.is_finite() || fs <= 0.0 {
            return Err(ToolError::invalid_args("字号必须为正数"));
        }
    }
    if let Some(w) = args.wrap_width {
        if !w.is_finite() || w <= 0.0 {
            return Err(ToolError::invalid_args("wrap_width 必须为正数"));
        }
    }
    args.style.validate().map_err(ToolError::invalid_args)?;
    Ok(())
}

fn validate_update(args: &UpdateElementArgs) -> Result<(), ToolError> {
    if args.id.trim().is_empty() {
        return Err(ToolError::invalid_args("id 不能为空"));
    }
    args.style.validate().map_err(ToolError::invalid_args)?;
    let has_style = args.style.stroke.is_some()
        || args.style.fill.is_some()
        || args.style.stroke_width.is_some()
        || args.style.roughness.is_some()
        || args.style.stroke_style.is_some()
        || args.style.fill_style.is_some()
        || args.style.opacity.is_some();
    if args.x.is_none()
        && args.y.is_none()
        && args.text.is_none()
        && !has_style
        && args.font_size.is_none()
    {
        return Err(ToolError::invalid_args(
            "至少提供 x/y/text/style/font_size 之一",
        ));
    }
    if args.x.is_some_and(|v| !v.is_finite()) || args.y.is_some_and(|v| !v.is_finite()) {
        return Err(ToolError::invalid_args("坐标必须是有限数值"));
    }
    if let Some(fs) = args.font_size {
        if !fs.is_finite() || fs <= 0.0 {
            return Err(ToolError::invalid_args("字号必须为正数"));
        }
    }
    args.style.validate().map_err(ToolError::invalid_args)?;
    Ok(())
}

fn validate_delete(args: &DeleteElementArgs) -> Result<(), ToolError> {
    if args.id.trim().is_empty() {
        return Err(ToolError::invalid_args("id 不能为空"));
    }
    Ok(())
}

/// True if `id` (an 8-char prefix or a full UUID) matches any live snapshot
/// entry. Matches the scene's `find_by_id_prefix` semantics.
fn snapshot_has_id(snapshot: &[ElementSnapshot], id: &str) -> bool {
    snapshot
        .iter()
        .any(|e| e.id.starts_with(id) || id.starts_with(&e.id))
}

// ---------------------------------------------------------------------------
// Lifecycle helpers: open a pending step, apply the op on the main thread
// (awaiting the reply), then close the step with the authoritative outcome.
// ---------------------------------------------------------------------------

/// Open a pending step, send the op, await the main thread's apply result, and
/// close the step with that result. The returned string is what rig feeds back
/// to the model — always the real outcome, never a guess.
async fn run_canvas_op(
    events: &UnboundedSender<AgentEvent>,
    id: String,
    name: &str,
    args_json: Value,
    op: CanvasOp,
    pre_assigned_id: Option<uuid::Uuid>,
) -> Result<String, ToolError> {
    let _ = events.unbounded_send(AgentEvent::ToolCall {
        id: id.clone(),
        name: name.to_string(),
        args: args_json,
    });
    let (tx, rx) = futures::channel::oneshot::channel();
    let _ = events.unbounded_send(AgentEvent::CanvasOp {
        op,
        pre_assigned_id,
        reply: tx,
    });
    let outcome: CanvasOpOutcome = rx
        .await
        .unwrap_or_else(|_| Err(CanvasOpError::internal("画布操作被取消（应用已关闭）")));
    let (is_error, message) = match &outcome {
        Ok(msg) => (false, msg.clone()),
        Err(e) => (true, e.message.clone()),
    };
    let _ = events.unbounded_send(AgentEvent::ToolResult {
        id,
        result: message,
        is_error,
    });
    outcome.map_err(ToolError::from_op)
}

/// Fail a tool call at the boundary: open + close the step with an error, and
/// return the error so rig feeds it back to the model for correction.
async fn fail_tool(
    events: &UnboundedSender<AgentEvent>,
    id: String,
    name: &str,
    args_json: Value,
    err: ToolError,
) -> Result<String, ToolError> {
    let _ = events.unbounded_send(AgentEvent::ToolCall {
        id: id.clone(),
        name: name.to_string(),
        args: args_json,
    });
    let _ = events.unbounded_send(AgentEvent::ToolResult {
        id,
        result: err.message.clone(),
        is_error: true,
    });
    Err(err)
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
    /// Font family alias: `handwritten` (default), `kai` (楷体), `hei` (黑体),
    /// `song` (宋体), `system`. Omit = handwritten.
    #[serde(default)]
    pub font_family: Option<String>,
    /// Wrap width in world units: lines longer than this wrap. Strongly
    /// recommended for body-text blocks. Omit = natural width.
    #[serde(default)]
    pub wrap_width: Option<f64>,
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

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_box(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            let op = CanvasOp::Rectangle {
                x: args.x,
                y: args.y,
                w: args.w,
                h: args.h,
                style: args.style,
                text: args.text,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
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

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_box(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            let op = CanvasOp::Ellipse {
                x: args.x,
                y: args.y,
                w: args.w,
                h: args.h,
                style: args.style,
                text: args.text,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
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

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_box(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            let op = CanvasOp::Diamond {
                x: args.x,
                y: args.y,
                w: args.w,
                h: args.h,
                style: args.style,
                text: args.text,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
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

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_points(&args.points) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            if let Err(msg) = args.style.validate() {
                return fail_tool(&events, id, name, args_json, ToolError::invalid_args(msg)).await;
            }
            let op = CanvasOp::Line {
                points: args.points,
                style: args.style,
                text: args.text,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
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

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_points(&args.points) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            if let Err(msg) = args.style.validate() {
                return fail_tool(&events, id, name, args_json, ToolError::invalid_args(msg)).await;
            }
            let op = CanvasOp::Arrow {
                points: args.points,
                start_arrowhead: args.start_arrowhead,
                end_arrowhead: args.end_arrowhead,
                style: args.style,
                text: args.text,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
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

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_text(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            let op = CanvasOp::Text {
                x: args.x,
                y: args.y,
                text: args.text,
                font_size: args.font_size,
                align: args.align,
                font_family: args.font_family,
                wrap_width: args.wrap_width,
                style: args.style,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
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
    /// Optional visual style override (stroke/fill/width/roughness/opacity).
    /// Omitted fields keep the element's current style.
    #[serde(default)]
    pub style: CanvasStyle,
    /// New font size (text elements only). Omit to keep current.
    #[serde(default)]
    pub font_size: Option<f64>,
}

pub struct UpdateElementTool {
    pub events: UnboundedSender<AgentEvent>,
    pub snapshot: Arc<Mutex<Vec<ElementSnapshot>>>,
}

impl Tool for UpdateElementTool {
    const NAME: &'static str = "update_element";
    type Error = ToolError;
    type Args = UpdateElementArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<UpdateElementArgs>(
            Self::NAME,
            "修改已有元素：移动（x/y）、改文字（text）、改样式（style：描边/填充/线宽/粗糙度/透明度）或改字号（font_size，仅文本）。id 是创建时返回的元素 ID。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let snapshot = self.snapshot.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_update(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            // Early id-existence check against the live snapshot so a bad id
            // fails fast (the apply path re-checks authoritatively).
            let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if !snapshot_has_id(&snap, &args.id) {
                return fail_tool(
                    &events,
                    id,
                    name,
                    args_json,
                    ToolError::not_found(format!("找不到元素 id={}", args.id)),
                )
                .await;
            }
            let op = CanvasOp::UpdateElement {
                id: args.id,
                x: args.x,
                y: args.y,
                text: args.text,
                style: args.style,
                font_size: args.font_size,
            };
            run_canvas_op(&events, id, name, args_json, op, None).await
        }
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
    pub snapshot: Arc<Mutex<Vec<ElementSnapshot>>>,
}

impl Tool for DeleteElementTool {
    const NAME: &'static str = "delete_element";
    type Error = ToolError;
    type Args = DeleteElementArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let events = self.events.clone();
        let snapshot = self.snapshot.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_delete(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if !snapshot_has_id(&snap, &args.id) {
                return fail_tool(
                    &events,
                    id,
                    name,
                    args_json,
                    ToolError::not_found(format!("找不到元素 id={}", args.id)),
                )
                .await;
            }
            let op = CanvasOp::DeleteElement { id: args.id };
            run_canvas_op(&events, id, name, args_json, op, None).await
        }
    }
}

// --- Clear Canvas ----------------------------------------------------------

pub struct ClearCanvasTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for ClearCanvasTool {
    const NAME: &'static str = "clear_canvas";
    type Error = ToolError;
    type Args = NoArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<NoArgs>(
            Self::NAME,
            "清空画布上的所有元素。用于用户要求重新开始或全部重画时。",
        );
        async move { def }
    }

    fn call(
        &self,
        _args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            run_canvas_op(&events, id, name, Value::Null, CanvasOp::Clear, None).await
        }
    }
}

// --- Set Canvas Background --------------------------------------------------

/// Arguments for `set_canvas_background`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SetBackgroundArgs {
    /// Preset surface: `greenboard` (墨绿粉笔板，黑板报首选), `blackboard`
    /// (黑板黑), or `white` (恢复白板). Either preset or color is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Explicit surface color as `0xRRGGBB`. Overrides `preset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
}

pub struct SetBackgroundTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for SetBackgroundTool {
    const NAME: &'static str = "set_canvas_background";
    type Error = ToolError;
    type Args = SetBackgroundArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<SetBackgroundArgs>(
            Self::NAME,
            "设置画布底色（板面）。黑板报/海报类作品的第一步：先用 preset=\"greenboard\" 把画布设为墨绿粉笔板，之后所有元素用粉笔色（白/米黄/粉/浅蓝）绘制。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Some(c) = args.color {
                if c > 0xFF_FF_FF {
                    return fail_tool(
                        &events,
                        id,
                        name,
                        args_json,
                        ToolError::invalid_args(format!(
                            "颜色 0x{c:x} 超出 0xRRGGBB 范围（最大 0xFFFFFF）"
                        )),
                    )
                    .await;
                }
            }
            let color = if let Some(c) = args.color {
                Some(c)
            } else {
                match args
                    .preset
                    .as_deref()
                    .map(|s| s.trim().to_ascii_lowercase())
                {
                    Some(p) => match p.as_str() {
                        "greenboard" | "墨绿" => Some(0x2A5240),
                        "blackboard" | "黑板" | "black" => Some(0x1F1F1F),
                        "white" | "白板" | "whiteboard" => None,
                        other => {
                            return fail_tool(
                                &events,
                                id,
                                name,
                                args_json,
                                ToolError::invalid_args(format!(
                                    "未知 preset: {other}（可用 greenboard/blackboard/white）"
                                )),
                            )
                            .await;
                        }
                    },
                    None => {
                        return fail_tool(
                            &events,
                            id,
                            name,
                            args_json,
                            ToolError::invalid_args("需要 preset 或 color 之一"),
                        )
                        .await;
                    }
                }
            };
            run_canvas_op(
                &events,
                id,
                name,
                args_json,
                CanvasOp::SetBackground { color },
                None,
            )
            .await
        }
    }
}

// --- Polygon -----------------------------------------------------------------

/// Arguments for the polygon tool.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PolygonArgs {
    /// Closed polygon vertices (≥3) in world coordinates, in drawing order.
    /// The last point connects back to the first. For mountains: 6~10 points
    /// with an irregular ridgeline reads best.
    pub points: Vec<OpPoint>,
    /// Optional visual style. Ink-wash guidance: fill + fill_style="solid"
    /// with opacity 0.35~0.5 for 远山，0.6~0.7 for 近岸.
    #[serde(default)]
    pub style: CanvasStyle,
}

pub struct PolygonTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for PolygonTool {
    const NAME: &'static str = "draw_polygon";
    type Error = ToolError;
    type Args = PolygonArgs;
    type Output = String;

    fn definition(&self, _prompt: String) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<PolygonArgs>(
            Self::NAME,
            "画一个封闭多边形（≥3 个顶点，末点自动连回首点）。不规则形状的首选：水墨的远山、近岸、坡地都用它（6~10 个顶点勾出起伏轮廓，fill+fill_style=solid 半透明填充）。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if args.points.len() < 3 {
                return fail_tool(
                    &events,
                    id,
                    name,
                    args_json,
                    ToolError::invalid_args("多边形至少需要三个顶点"),
                )
                .await;
            }
            if args
                .points
                .iter()
                .any(|p| !p.x.is_finite() || !p.y.is_finite())
            {
                return fail_tool(
                    &events,
                    id,
                    name,
                    args_json,
                    ToolError::invalid_args("坐标必须是有限数值"),
                )
                .await;
            }
            if let Err(msg) = args.style.validate() {
                return fail_tool(&events, id, name, args_json, ToolError::invalid_args(msg))
                    .await;
            }
            let op = CanvasOp::Polygon {
                points: args.points,
                style: args.style,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
    }
}

// --- Mind Map --------------------------------------------------------------

/// Arguments for `draw_mindmap`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MindmapArgs {
    /// The root of the mind map tree, e.g.
    /// `{"text":"中心主题","children":[{"text":"分支","children":[{"text":"要点"}]}]}`.
    /// Layout (positions, colors, links) is computed automatically — supply
    /// only the texts. Keep node text a ≤ 20-char single-line keyword;
    /// whole tree ≤ 40 nodes and ≤ 5 levels.
    pub root: OpMindmapNode,
    /// Root center X in world coordinates. Omit = canvas center (800).
    #[serde(default)]
    pub cx: Option<f64>,
    /// Root center Y in world coordinates. Omit = canvas center (500).
    #[serde(default)]
    pub cy: Option<f64>,
}

/// Tree limits mirroring the system prompt's guidance; keeps the auto-fit
/// layout inside readable font sizes.
const MINDMAP_MAX_NODES: usize = 40;
const MINDMAP_MAX_DEPTH: usize = 5;
const MINDMAP_MAX_TEXT: usize = 20;

fn validate_mindmap_text(node: &OpMindmapNode) -> Result<(), ToolError> {
    let t = node.text.trim();
    if t.is_empty() {
        return Err(ToolError::invalid_args("节点文字不能为空"));
    }
    if t.contains('\n') {
        return Err(ToolError::invalid_args(
            "节点文字必须是单行（不能含换行）",
        ));
    }
    if t.chars().count() > MINDMAP_MAX_TEXT {
        return Err(ToolError::invalid_args(format!(
            "节点文字「{}…」超过 {MINDMAP_MAX_TEXT} 字上限，请精炼成关键词短语",
            t.chars().take(12).collect::<String>()
        )));
    }
    for c in &node.children {
        validate_mindmap_text(c)?;
    }
    Ok(())
}

fn validate_mindmap(args: &MindmapArgs) -> Result<(), ToolError> {
    if let Some(cx) = args.cx {
        if !cx.is_finite() {
            return Err(ToolError::invalid_args("cx 必须是有限数值"));
        }
    }
    if let Some(cy) = args.cy {
        if !cy.is_finite() {
            return Err(ToolError::invalid_args("cy 必须是有限数值"));
        }
    }
    validate_mindmap_text(&args.root)?;
    let input = crate::scene::mindmap::MindmapNodeInput::from(&args.root);
    let n = crate::scene::mindmap::count_nodes(&input);
    if n > MINDMAP_MAX_NODES {
        return Err(ToolError::invalid_args(format!(
            "思维导图共 {n} 个节点，超过 {MINDMAP_MAX_NODES} 上限——请删减要点或拆成两张图"
        )));
    }
    let d = crate::scene::mindmap::max_depth(&input);
    if d > MINDMAP_MAX_DEPTH {
        return Err(ToolError::invalid_args(format!(
            "思维导图深度 {d} 层，超过 {MINDMAP_MAX_DEPTH} 层上限"
        )));
    }
    Ok(())
}

pub struct MindmapTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for MindmapTool {
    const NAME: &'static str = "draw_mindmap";
    type Error = ToolError;
    type Args = MindmapArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<MindmapArgs>(
            Self::NAME,
            "画一张完整的思维导图。root 是嵌套树（text + children），只需给出文字内容：布局（左右均衡、节点位置、曲线连线、分支配色）全部自动计算，禁止自己用矩形+连线拼导图。中心主题 1 个，一级分支 3~6 个，每个分支 2~5 个要点；节点文字为 ≤20 字的单行关键词。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if let Err(e) = validate_mindmap(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            let op = CanvasOp::Mindmap {
                root: args.root,
                cx: args.cx,
                cy: args.cy,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
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
    /// Public so the board's runtime-context snapshot reuses the exact wording
    /// the `list_elements` tool reports back (the model sees consistent ids).
    pub fn summary(&self) -> String {
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
    pub snapshot: Arc<Mutex<Vec<ElementSnapshot>>>,
}

impl Tool for ListElementsTool {
    const NAME: &'static str = "list_elements";
    type Error = ToolError;
    type Args = NoArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
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
        let snapshot = self.snapshot.clone();
        async move {
            // Read the LIVE snapshot (refreshed by the main thread after each
            // apply), so elements drawn earlier in this same request are visible.
            let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if snap.is_empty() {
                Ok("画布为空".to_string())
            } else {
                let lines: Vec<String> = snap
                    .iter()
                    .enumerate()
                    .map(|(i, e)| format!("{}. {}", i + 1, e.summary()))
                    .collect();
                Ok(lines.join("\n"))
            }
        }
    }
}

/// Empty args for tools that take no parameters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct NoArgs {}

/// Build all tools sharing one event channel + a live canvas snapshot (for
/// `list_elements` and update/delete id validation). The snapshot is shared and
/// refreshed by the main thread after each applied op.
pub fn all_tools(
    events: UnboundedSender<AgentEvent>,
    snapshot: Arc<Mutex<Vec<ElementSnapshot>>>,
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
            snapshot: snapshot.clone(),
        }),
        Box::new(DeleteElementTool {
            events: events.clone(),
            snapshot: snapshot.clone(),
        }),
        Box::new(ClearCanvasTool {
            events: events.clone(),
        }),
        Box::new(SetBackgroundTool {
            events: events.clone(),
        }),
        Box::new(PolygonTool {
            events: events.clone(),
        }),
        Box::new(MindmapTool {
            events: events.clone(),
        }),
        Box::new(ListElementsTool { snapshot }),
    ]
}

// Keep the ChatMessage import referenced — it's part of the module's public
// surface (re-exported via the agent) even if not directly used here.
#[allow(dead_code)]
fn _chat_message_referenced(_: &ChatMessage) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_args(w: f64, h: f64) -> BoxArgs {
        BoxArgs {
            x: 0.0,
            y: 0.0,
            w,
            h,
            style: CanvasStyle::default(),
            text: None,
        }
    }

    fn snap(id: &str) -> ElementSnapshot {
        ElementSnapshot {
            id: id.to_string(),
            kind: "rectangle".to_string(),
            text: None,
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        }
    }

    #[test]
    fn validate_box_rejects_bad_sizes() {
        assert!(validate_box(&box_args(100.0, 50.0)).is_ok());
        assert!(validate_box(&box_args(0.0, 50.0)).is_err());
        assert!(validate_box(&box_args(100.0, -5.0)).is_err());
        assert!(validate_box(&box_args(f64::NAN, 50.0)).is_err());
    }

    #[test]
    fn validate_points_requires_two_finite_points() {
        assert!(validate_points(&[OpPoint { x: 0.0, y: 0.0 }]).is_err());
        assert!(
            validate_points(&[OpPoint { x: 0.0, y: 0.0 }, OpPoint { x: 1.0, y: 1.0 },]).is_ok()
        );
        assert!(validate_points(&[
            OpPoint { x: 0.0, y: 0.0 },
            OpPoint {
                x: f64::INFINITY,
                y: 1.0
            },
        ])
        .is_err());
    }

    #[test]
    fn snapshot_has_id_matches_prefix_and_full() {
        let s = vec![snap("a1b2c3d4")];
        assert!(snapshot_has_id(&s, "a1b2c3d4"));
        assert!(snapshot_has_id(&s, "a1b2"));
        assert!(!snapshot_has_id(&s, "deadbeef"));
    }

    fn mindmap_args(root: OpMindmapNode) -> MindmapArgs {
        MindmapArgs {
            root,
            cx: None,
            cy: None,
        }
    }

    #[test]
    fn validate_mindmap_accepts_reasonable_tree() {
        let tree = OpMindmapNode {
            text: "高效学习方法".into(),
            children: vec![OpMindmapNode {
                text: "主动回忆".into(),
                children: vec![
                    OpMindmapNode {
                        text: "自测".into(),
                        children: vec![],
                    },
                    OpMindmapNode {
                        text: "闪卡".into(),
                        children: vec![],
                    },
                ],
            }],
        };
        assert!(validate_mindmap(&mindmap_args(tree)).is_ok());
    }

    #[test]
    fn validate_mindmap_rejects_bad_text() {
        let mk = |t: &str| OpMindmapNode {
            text: t.into(),
            children: vec![],
        };
        for bad in ["", "  ", "一\n二"] {
            assert!(validate_mindmap(&mindmap_args(mk(bad))).is_err(), "{bad}");
        }
        let long = "这个词组远远超过了二十个字的节点上限确实太长了";
        assert_eq!(long.chars().count(), 23);
        assert!(validate_mindmap(&mindmap_args(mk(long))).is_err());
        // A deep node's bad text is caught too.
        let tree = OpMindmapNode {
            text: "根".into(),
            children: vec![OpMindmapNode {
                text: "枝".into(),
                children: vec![mk("一\n二")],
            }],
        };
        assert!(validate_mindmap(&mindmap_args(tree)).is_err());
    }

    #[test]
    fn validate_mindmap_rejects_oversize_tree() {
        // 41 nodes > 40 cap.
        let mut root = OpMindmapNode {
            text: "根".into(),
            children: vec![],
        };
        for i in 0..40 {
            root.children.push(OpMindmapNode {
                text: format!("叶{i}"),
                children: vec![],
            });
        }
        assert_eq!(crate::scene::mindmap::count_nodes(
            &crate::scene::mindmap::MindmapNodeInput::from(&root)
        ), 41);
        assert!(validate_mindmap(&mindmap_args(root)).is_err());
    }

    #[test]
    fn validate_mindmap_rejects_too_deep() {
        let mut n = OpMindmapNode {
            text: "第六层".into(),
            children: vec![],
        };
        for t in ["第五层", "第四层", "第三层", "第二层", "根"] {
            n = OpMindmapNode {
                text: t.into(),
                children: vec![n],
            };
        }
        assert!(validate_mindmap(&mindmap_args(n)).is_err());
    }

    #[test]
    fn validate_mindmap_rejects_non_finite_center() {
        let tree = OpMindmapNode {
            text: "根".into(),
            children: vec![],
        };
        let args = MindmapArgs {
            root: tree,
            cx: Some(f64::NAN),
            cy: None,
        };
        assert!(validate_mindmap(&args).is_err());
    }

    #[test]
    fn tool_error_display_is_the_message() {
        let e = ToolError::not_found("找不到元素 id=abc");
        assert_eq!(e.to_string(), "找不到元素 id=abc");
        assert_eq!(e.code, CanvasOpErrorCode::NotFound);
    }

    /// End-to-end tool-chain test without GPUI: the tool emits
    /// ToolCall → CanvasOp → (caller applies + replies) → ToolResult. A
    /// successful apply MUST yield `is_error == false` on the ToolResult —
    /// this is the exact chain the headless eval harness depends on.
    #[test]
    fn tool_result_not_error_after_successful_reply() {
        use futures::task::noop_waker_ref;
        use std::task::Context;

        let (tx, mut rx) = futures::channel::mpsc::unbounded::<AgentEvent>();
        let tool = TextTool { events: tx };
        let args: TextArgs =
            serde_json::from_str(r#"{"x":10.0,"y":10.0,"text":"你好","font_size":20.0}"#).unwrap();
        let mut fut = Box::pin(tool.call(args));
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);

        // First poll: emits ToolCall + CanvasOp, then pends on the reply.
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(_) => panic!("tool completed before reply"),
        }

        // Drain events until the CanvasOp, then reply exactly like the
        // eval harness / panel does.
        let mut replied = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::CanvasOp { reply, .. } => {
                    reply
                        .send(Ok("已添加文本 id=test1234".to_string()))
                        .expect("reply send");
                    replied = true;
                }
                AgentEvent::ToolResult {
                    is_error, result, ..
                } => {
                    panic!("ToolResult before reply: is_error={is_error} {result}");
                }
                _ => {}
            }
        }
        assert!(replied, "no CanvasOp event to reply to");

        // Completion poll: emits the ToolResult.
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Ready(Ok(msg)) => assert!(msg.contains("test1234")),
            _ => panic!("tool did not complete after reply"),
        }

        // The ToolResult must carry is_error=false.
        let mut saw_result = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::ToolResult {
                is_error, result, ..
            } = event
            {
                saw_result = true;
                assert!(!is_error, "successful reply logged as error: {result}");
            }
        }
        assert!(saw_result, "no ToolResult event");
    }
}
