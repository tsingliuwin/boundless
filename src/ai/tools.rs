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
    de_color, de_style, CanvasOp, CanvasOpError, CanvasOpErrorCode, CanvasOpOutcome, CanvasStyle,
    OpMindmapNode, OpPoint, OpTextAlign,
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
    if let Some(a) = &args.anchor {
        if a != "center" {
            return Err(ToolError::invalid_args(
                "anchor 只支持 \"center\"（x 为文本水平中心线）；省略 = x 为左上角",
            ));
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
    #[serde(default, deserialize_with = "de_style")]
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
    #[serde(default, deserialize_with = "de_style")]
    pub style: CanvasStyle,
    /// Optional text label on the line/arrow (e.g. "是"/"否" on a flow arrow).
    /// The label is centered on the line and follows it when moved.
    #[serde(default)]
    pub text: Option<String>,
}

/// Arguments for the text tool.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct TextArgs {
    /// Top-left X in world coordinates — or, with anchor="center", the
    /// text's horizontal CENTER line (e.g. a page's center line).
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
    /// Positioning anchor: "center" = X is the text's horizontal center line.
    /// Use for page-centered titles instead of computing offsets by hand.
    #[serde(default)]
    pub anchor: Option<String>,
    /// Optional visual style (opacity etc.).
    #[serde(default, deserialize_with = "de_style")]
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
            "在画布上画一条折线或平滑曲线（无箭头），经过给定的若干点（世界坐标，至少两点）。smooth=true 时为波浪/河流/微笑等有机曲线的首选。",
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
                anchor: args.anchor,
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
    #[serde(default, deserialize_with = "de_style")]
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

// --- Pose Element（摆肢体/掰关节） ------------------------------------------

/// Arguments for `pose_element`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PoseElementArgs {
    /// 要摆姿势的元素 id（盖章返回的 8 位短 id；角色的手臂/腿是线条元素，
    /// list_elements 里 kind=line/arrow/polygon）。
    pub id: String,
    /// 新的绝对坐标点序列（世界坐标，≥2 个）。点数可与原来不同：给直线
    /// 加中间点 = 加关节（肘/膝）。例：把垂在身侧的直手臂 [(150,300),
    /// (150,380)] 改成举起的折臂 [(150,300),(160,330),(120,300)]。
    pub points: Vec<OpPoint>,
}

pub struct PoseElementTool {
    pub events: UnboundedSender<AgentEvent>,
    pub snapshot: Arc<Mutex<Vec<ElementSnapshot>>>,
}

fn validate_pose(args: &PoseElementArgs) -> Result<(), ToolError> {
    if args.id.trim().is_empty() {
        return Err(ToolError::invalid_args("id 不能为空"));
    }
    if args.points.len() < 2 {
        return Err(ToolError::invalid_args("points 至少需要两个坐标点"));
    }
    if args.points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return Err(ToolError::invalid_args("坐标点必须是有限数值"));
    }
    Ok(())
}

impl Tool for PoseElementTool {
    const NAME: &'static str = "pose_element";
    type Error = ToolError;
    type Args = PoseElementArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<PoseElementArgs>(
            Self::NAME,
            "摆肢体姿势：替换线条/箭头/多边形元素的绝对坐标点（≥2，点数可变，加中间点=加关节）。这是角色肢体语言的核心工具——盖章后的角色手臂/腿是独立线条元素，用本工具掰出举手、摊手、指点、叉腰、扶额、奔跑摆臂等动作。禁止只让角色垂手站立说话。",
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
            if let Err(e) = validate_pose(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            // Early id-existence check against the live snapshot（guard 在
            // 块内释放，fail_tool 的 await 不得持锁跨 await）。
            let id_exists = {
                let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                snapshot_has_id(&snap, args.id.trim())
            };
            if !id_exists {
                return fail_tool(
                    &events,
                    id,
                    name,
                    args_json,
                    ToolError::not_found(format!("找不到元素 id={}", args.id.trim())),
                )
                .await;
            }
            let op = CanvasOp::SetElementPoints {
                id: args.id.trim().to_string(),
                points: args.points,
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

// --- Set Paper Texture -----------------------------------------------------

/// Arguments for `set_paper_texture`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SetPaperTextureArgs {
    /// `grain` (水彩纸细纹) / `kraft` (牛皮纸纤维) / `chalkboard` (黑板粉尘)
    /// / `none` (移除材质). Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture: Option<String>,
}

pub struct SetPaperTextureTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for SetPaperTextureTool {
    const NAME: &'static str = "set_paper_texture";
    type Error = ToolError;
    type Args = SetPaperTextureArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<SetPaperTextureArgs>(
            Self::NAME,
            "给画布叠加纸纹材质（细腻质感的关键一步）：grain=水彩纸细纹、kraft=牛皮纸纤维、chalkboard=黑板粉尘、none=移除。与 set_canvas_background 搭配使用。",
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
            let texture = match args.texture.as_deref() {
                Some("grain") => Some(Some(crate::scene::PaperTexture::Grain)),
                Some("kraft") => Some(Some(crate::scene::PaperTexture::Kraft)),
                Some("chalkboard") => Some(Some(crate::scene::PaperTexture::Chalkboard)),
                Some("none") => Some(None),
                _ => {
                    return fail_tool(
                        &events,
                        id,
                        name,
                        args_json,
                        ToolError::invalid_args("texture 需要 grain/kraft/chalkboard/none 之一"),
                    )
                    .await;
                }
            };
            run_canvas_op(
                &events,
                id,
                name,
                args_json,
                CanvasOp::SetTexture { texture },
                None,
            )
            .await
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
    /// Explicit surface color: 0xRRGGBB as a decimal integer (e.g. `0x2a5240`
    /// = 2773568) or a hex string (`"0x2a5240"` / `"#2a5240"`). Overrides
    /// `preset`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_color"
    )]
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
    /// Draw as a smooth curve through the points (waves, rivers, smiles)
    /// instead of straight segments. Recommended for organic strokes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smooth: Option<bool>,
    /// Optional visual style. Ink-wash guidance: fill + fill_style="solid"
    /// with opacity 0.35~0.5 for 远山，0.6~0.7 for 近岸；fill_style="dense"
    /// 为近乎实心的密排填充；fill_style="watercolor" 为多层晕染 + 边缘
    /// 墨色堆积的水彩质感（大面积上色/主体的首选，最细腻）。
    /// 细粒度微调（优先于预设）：hachure_gap（排线间距，2~6）、
    /// fill_weight（排线线宽，>= 2×间距 近乎实心）、hachure_angle（角度）。
    #[serde(default, deserialize_with = "de_style")]
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

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<PolygonArgs>(
            Self::NAME,
            "画一个封闭多边形（≥3 个顶点，末点自动连回首点）。不规则形状的首选：水墨的远山、近岸、坡地都用它（6~10 个顶点勾出起伏轮廓，fill+fill_style=solid 半透明填充；fill_style=dense 为近乎实心的密排填充）。",
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
                return fail_tool(&events, id, name, args_json, ToolError::invalid_args(msg)).await;
            }
            let op = CanvasOp::Polygon {
                points: args.points,
                smooth: args.smooth.unwrap_or(false),
                style: args.style,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
    }
}

// --- Smooth Shape ----------------------------------------------------------

/// Arguments for the add-image tool.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct AddImageArgs {
    /// Absolute path of the image file (PNG/JPEG/GIF/WebP/BMP). The file is
    /// copied into the workspace asset store — the board keeps working even
    /// if the original is moved or deleted.
    pub path: String,
    /// Left edge in world coordinates. Omit = centered on the current view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    /// Top edge in world coordinates. Omit = centered on the current view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    /// Display width in world units (20~4000); height follows the aspect
    /// ratio. Omit = 320.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
}

/// Embed a raster image onto the canvas (插画 / 照片 / 贴图素材).
pub struct AddImageTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for AddImageTool {
    const NAME: &'static str = "add_image";
    type Error = ToolError;
    type Args = AddImageArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<AddImageArgs>(
            Self::NAME,
            "把一张图片嵌入画布（PNG/JPEG/GIF/WebP/BMP）。给出文件绝对路径；width 控制显示宽度（高度按比例自动求出）。适合插画、照片、贴图素材。",
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
            if let Some(w) = args.width {
                if !(20.0..=4000.0).contains(&w) {
                    return fail_tool(
                        &events,
                        id,
                        name,
                        args_json,
                        ToolError::invalid_args("width 需在 20~4000 之间"),
                    )
                    .await;
                }
            }
            let op = CanvasOp::AddImage {
                path: args.path,
                x: args.x,
                y: args.y,
                width: args.width,
            };
            run_canvas_op(&events, id, name, args_json, op, Some(new_element_id())).await
        }
    }
}

/// Arguments for `draw_smooth_shape`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SmoothShapeArgs {
    /// Control points (≥3) in world coordinates, in drawing order. The shape
    /// is a closed smooth spline THROUGH these points — place them on the
    /// outline's extremes (like pushing pins into the silhouette). 4~8
    /// points read best: petals, clouds, pebbles, leaves, water ripples.
    pub points: Vec<OpPoint>,
    /// Optional visual style. Guidance: fill + fill_style="watercolor" for
    /// the softest hand-painted look; hachure_gap/fill_weight fine-tune the
    /// texture; shadow=true lifts the shape off the canvas.
    #[serde(default, deserialize_with = "de_style")]
    pub style: CanvasStyle,
}

pub struct SmoothShapeTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for SmoothShapeTool {
    const NAME: &'static str = "draw_smooth_shape";
    type Error = ToolError;
    type Args = SmoothShapeArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<SmoothShapeArgs>(
            Self::NAME,
            "画一个平滑的封闭曲线形（有机形态的首选：花瓣、云朵、鹅卵石、树叶、水波）。3~8 个控制点勾出轮廓极值点，曲线自动平滑闭合，比 polygon 更柔和细腻。fill+fill_style=watercolor 搭配最佳，可加 shadow 增加立体感。",
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
                    ToolError::invalid_args("平滑曲线形至少需要三个控制点"),
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
                return fail_tool(&events, id, name, args_json, ToolError::invalid_args(msg)).await;
            }
            let op = CanvasOp::Polygon {
                points: args.points,
                smooth: true,
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
        return Err(ToolError::invalid_args("节点文字必须是单行（不能含换行）"));
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

// --- Use Skill --------------------------------------------------------------

/// Arguments for `use_skill`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct UseSkillArgs {
    /// 技能名，取自系统提示「场景技能库」清单中的加粗名称，如 "mindmap"。
    pub name: String,
}

/// Loads a scenario skill's composition spec (skills/*/SKILL.md). The system
/// prompt carries only the one-line catalog; this tool hands the model the
/// full per-scene spec on demand and records the activation so the panel can
/// keep the spec in the runtime context across turns.
pub struct UseSkillTool {
    pub events: UnboundedSender<AgentEvent>,
    pub active: super::skills::ActiveSkill,
}

impl Tool for UseSkillTool {
    const NAME: &'static str = "use_skill";
    type Error = ToolError;
    type Args = UseSkillArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<UseSkillArgs>(
            Self::NAME,
            "加载一个场景技能的完整构图规范。用户请求命中系统提示「场景技能库」中的某个技能时，第一步先调用本工具（传技能名 name），返回的规范必须严格遵循后再开始绘制。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let active = self.active.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            let _ = events.unbounded_send(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.to_string(),
                args: args_json,
            });
            match super::skills::find(args.name.trim()) {
                Some(skill) => {
                    active.set(&skill.name);
                    let title = if skill.display_name.is_empty() {
                        skill.name.clone()
                    } else {
                        format!("{}（{}）", skill.name, skill.display_name)
                    };
                    let result = format!(
                        "已加载技能「{title}」v{}的构图规范，后续绘制必须严格遵循：\n\n{}",
                        skill.version, skill.body
                    );
                    let _ = events.unbounded_send(AgentEvent::ToolResult {
                        id,
                        result: result.clone(),
                        is_error: false,
                    });
                    Ok(result)
                }
                None => {
                    let msg = format!(
                        "找不到技能 name={}。请核对系统提示「场景技能库」清单中的技能名。",
                        args.name
                    );
                    let _ = events.unbounded_send(AgentEvent::ToolResult {
                        id,
                        result: msg.clone(),
                        is_error: true,
                    });
                    Err(ToolError::not_found(msg))
                }
            }
        }
    }
}

// --- Add Page ----------------------------------------------------------------

/// Arguments for `add_page`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct AddPageArgs {
    /// 页标题（显示在页面框上方和页面栏），如 "封面"、"目录"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 页面比例预设："16:9"（默认）、"4:3"、"9:16"（竖屏）、"3:4"、"1:1"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<String>,
    /// 本页的过渡动效（放映翻到本页时、以及放映从本页开始时的出现方式）：
    /// "slide"（默认，相机横扫滑入）、"fade"（经黑淡入淡出）、"none"（硬切）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
}

/// Opens a new slide page: a titled world-space rect laid out after the
/// existing pages. The tool result reports the page rect; the model draws
/// that page's content inside it. Flip/present is a viewer concern handled
/// by the page bar (PageUp/PageDown / F5).
pub struct AddPageTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for AddPageTool {
    const NAME: &'static str = "add_page";
    type Error = ToolError;
    type Args = AddPageArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<AddPageArgs>(
            Self::NAME,
            "新建一张幻灯片页面（PPT 翻页用）。返回页面矩形；本页的所有内容必须画在该矩形内。制作 PPT/演示文稿时，每页先调用本工具（可带 title 和 ratio），再绘制页面内容。",
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
            if let Some(t) = &args.title {
                if t.trim().chars().count() > 30 {
                    return fail_tool(
                        &events,
                        next_tool_id(name),
                        name,
                        serde_json::to_value(&args).unwrap_or(Value::Null),
                        ToolError::invalid_args("页面标题不要超过 30 字"),
                    )
                    .await;
                }
            }
            if let Some(r) = &args.ratio {
                if crate::scene::pages::PageRatio::parse(r).is_none() {
                    return fail_tool(
                        &events,
                        next_tool_id(name),
                        name,
                        serde_json::to_value(&args).unwrap_or(Value::Null),
                        ToolError::invalid_args("ratio 只支持 16:9 / 4:3 / 9:16 / 3:4 / 1:1"),
                    )
                    .await;
                }
            }
            if let Some(e) = &args.effect {
                if crate::scene::pages::PageEffect::parse(e).is_none() {
                    return fail_tool(
                        &events,
                        next_tool_id(name),
                        name,
                        serde_json::to_value(&args).unwrap_or(Value::Null),
                        ToolError::invalid_args("effect 只支持 slide / fade / none"),
                    )
                    .await;
                }
            }
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            let op = CanvasOp::AddPage {
                title: args.title,
                ratio: args.ratio,
                effect: args.effect,
            };
            run_canvas_op(&events, id, name, args_json, op, None).await
        }
    }
}

// --- Delete Page -------------------------------------------------------------

/// Arguments for `delete_page`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct DeletePageArgs {
    /// 要删除的页码（1 起）。省略 = 删除最后一页。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<usize>,
}

/// Deletes a slide page frame. Elements on that page stay on the canvas —
/// the model should redraw replacements or tell the user about leftovers.
pub struct DeletePageTool {
    pub events: UnboundedSender<AgentEvent>,
}

impl Tool for DeletePageTool {
    const NAME: &'static str = "delete_page";
    type Error = ToolError;
    type Args = DeletePageArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<DeletePageArgs>(
            Self::NAME,
            "删除一张幻灯片页面（页面框；页内元素保留在画布上，如需清理可再用 delete_element 删除具体元素）。用户要求删掉某页/减少页数时使用。",
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
            let op = CanvasOp::DeletePage {
                number: args.number,
            };
            run_canvas_op(&events, id, name, args_json, op, None).await
        }
    }
}

// --- Save Template -----------------------------------------------------------

/// Arguments for `save_template`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SaveTemplateArgs {
    /// 模板名（如角色名「小明」），之后 stamp_template 用它引用。同名覆盖。
    pub name: String,
    /// 模板类别：character（角色）/ scene（场景）/ prop（道具）。
    pub kind: String,
    /// 组成模板的元素 id 列表（draw_* 返回的 8 位短 id）。
    pub ids: Vec<String>,
    /// true = 保存后从画布删除这些元素（推荐：页面外画好角色 → 收进模板库
    /// → 用 stamp_template 盖章到每一格）。默认 false。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_source: Option<bool>,
}

pub struct SaveTemplateTool {
    pub events: UnboundedSender<AgentEvent>,
    pub snapshot: Arc<Mutex<Vec<ElementSnapshot>>>,
    pub templates_dir: std::path::PathBuf,
}

impl Tool for SaveTemplateTool {
    const NAME: &'static str = "save_template";
    type Error = ToolError;
    type Args = SaveTemplateArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<SaveTemplateArgs>(
            Self::NAME,
            "把已画好的一组元素保存为可复用模板（角色/场景/道具）。漫画等多格创作里，同一角色先画一次存模板，之后每格都用 stamp_template 盖章，保证人物与场景一致。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let snapshot = self.snapshot.clone();
        let templates_dir = self.templates_dir.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            // 边界校验：名字、类别、id 列表（对快照快速失败，主线程再权威复核）。
            let clean = match crate::scene::templates::sanitize_name(&args.name) {
                Ok(n) => n,
                Err(e) => return fail_tool(&events, id, name, args_json, ToolError::invalid_args(e)).await,
            };
            let kind = match crate::scene::templates::TemplateKind::parse(&args.kind) {
                Some(k) => k,
                None => {
                    return fail_tool(
                        &events,
                        id,
                        name,
                        args_json,
                        ToolError::invalid_args("kind 需要 character / scene / prop 之一"),
                    )
                    .await
                }
            };
            if args.ids.is_empty() || args.ids.len() > 60 {
                return fail_tool(
                    &events,
                    id,
                    name,
                    args_json,
                    ToolError::invalid_args("ids 需要 1~60 个元素 id"),
                )
                .await;
            }
            // 对快照快速失败；guard 在块内释放，fail_tool 的 await 不得
            // 持锁跨 await（future 必须 Send）。
            let missing: Vec<String> = {
                let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                args.ids
                    .iter()
                    .filter(|i| !snapshot_has_id(&snap, i))
                    .cloned()
                    .collect()
            };
            if let Some(first) = missing.first() {
                return fail_tool(
                    &events,
                    id,
                    name,
                    args_json,
                    ToolError::not_found(format!("找不到元素 id={first}")),
                )
                .await;
            }
            let _ = events.unbounded_send(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.to_string(),
                args: args_json,
            });
            let (tx, rx) = futures::channel::oneshot::channel();
            let delete_source = args.delete_source.unwrap_or(false);
            let _ = events.unbounded_send(AgentEvent::ExtractElements {
                ids: args.ids.clone(),
                delete_source,
                reply: tx,
            });
            let elements = match rx
                .await
                .unwrap_or_else(|_| Err(CanvasOpError::internal("元素提取被取消（应用已关闭）")))
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = events.unbounded_send(AgentEvent::ToolResult {
                        id,
                        result: e.message.clone(),
                        is_error: true,
                    });
                    return Err(ToolError::from_op(e));
                }
            };
            let template = crate::scene::templates::ComicTemplate {
                name: clean.clone(),
                kind,
                elements,
            };
            // 汇总包围盒用于回报尺寸。
            let mut bbox: Option<crate::scene::WBounds> = None;
            for el in &template.elements {
                bbox = Some(match bbox {
                    Some(u) => u.union(&el.bounds),
                    None => el.bounds,
                });
            }
            let (w, h) = bbox.map(|b| (b.w, b.h)).unwrap_or((0.0, 0.0));
            match crate::scene::templates::save(&templates_dir, &template) {
                Ok(_) => {
                    let msg = format!(
                        "已保存模板「{clean}」（{}，{} 个元素，{:.0}×{:.0}）。之后每格都用 stamp_template(\"{clean}\", x, y) 复用它，不要重画。",
                        kind.label(),
                        template.elements.len(),
                        w,
                        h
                    );
                    let _ = events.unbounded_send(AgentEvent::ToolResult {
                        id,
                        result: msg.clone(),
                        is_error: false,
                    });
                    Ok(msg)
                }
                Err(e) => {
                    let _ = events.unbounded_send(AgentEvent::ToolResult {
                        id,
                        result: e.clone(),
                        is_error: true,
                    });
                    Err(ToolError {
                        code: CanvasOpErrorCode::Internal,
                        message: e,
                    })
                }
            }
        }
    }
}

// --- Stamp Template ----------------------------------------------------------

/// Arguments for `stamp_template`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct StampTemplateArgs {
    /// 模板名（save_template 保存时用的名字，list_templates 可查）。
    pub name: String,
    /// 模板包围盒左上角要落到的世界坐标 X。
    pub x: f64,
    /// 模板包围盒左上角要落到的世界坐标 Y。
    pub y: f64,
    /// 等比缩放 0.05~8.0。默认 1（原尺寸）。同一角色在各格中的缩放应保持相近，避免忽大忽小。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// true = 水平镜像（翻转角色朝向）。默认 false。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_x: Option<bool>,
    /// 整体旋转角度（度，顺时针，-45~45，默认 0）。动势专用：奔跑前倾
    /// 12~20、惊吓后仰 -8~-15、摔倒/翻滚 30~45。矩形/椭圆/菱形会转成多
    /// 边形实现旋转（手绘风视觉无差）；文字不旋转保持水平，只随组移动。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,
}

pub struct StampTemplateTool {
    pub events: UnboundedSender<AgentEvent>,
    pub templates_dir: std::path::PathBuf,
}

impl Tool for StampTemplateTool {
    const NAME: &'static str = "stamp_template";
    type Error = ToolError;
    type Args = StampTemplateArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<StampTemplateArgs>(
            Self::NAME,
            "把保存的模板实例化到画布 (x,y)：scale 缩放、flip_x=true 翻转朝向、rotation 整体倾斜做动势（奔跑前倾 12~20、惊吓后仰 -8~-15、摔倒 30~45）。漫画每一格都用本工具复用同一角色/场景模板，不要重画；角色要活起来就换姿势 + 换 rotation + 叠表情。",
        );
        async move { def }
    }

    fn call(
        &self,
        args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let templates_dir = self.templates_dir.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
            if !args.x.is_finite() || !args.y.is_finite() {
                return fail_tool(&events, id, name, args_json, ToolError::invalid_args("坐标必须是有限数值")).await;
            }
            if let Some(s) = args.scale {
                if !s.is_finite() || !(0.05..=8.0).contains(&s) {
                    return fail_tool(
                        &events,
                        id,
                        name,
                        args_json,
                        ToolError::invalid_args(format!("scale {s} 超出范围 0.05~8.0")),
                    )
                    .await;
                }
            }
            if let Some(r) = args.rotation {
                if !r.is_finite() || !(-45.0..=45.0).contains(&r) {
                    return fail_tool(
                        &events,
                        id,
                        name,
                        args_json,
                        ToolError::invalid_args(format!("rotation {r} 超出范围 -45~45 度")),
                    )
                    .await;
                }
            }
            let _ = events.unbounded_send(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.to_string(),
                args: args_json,
            });
            let emit_err = |events: &UnboundedSender<AgentEvent>, id: String, msg: String| {
                let _ = events.unbounded_send(AgentEvent::ToolResult {
                    id,
                    result: msg.clone(),
                    is_error: true,
                });
                Err(ToolError::not_found(msg))
            };
            let template =
                match crate::scene::templates::load(&templates_dir, args.name.trim()) {
                    Ok(t) => t,
                    Err(e) => return emit_err(&events, id, e),
                };
            let stamped = match crate::scene::templates::stamp(
                &template,
                args.x,
                args.y,
                args.scale.unwrap_or(1.0),
                args.flip_x.unwrap_or(false),
                args.rotation.unwrap_or(0.0),
            ) {
                Ok(v) => v,
                Err(e) => {
                    let _ = events.unbounded_send(AgentEvent::ToolResult {
                        id,
                        result: e.clone(),
                        is_error: true,
                    });
                    return Err(ToolError::invalid_args(e));
                }
            };
            let n = stamped.len();
            let (tx, rx) = futures::channel::oneshot::channel();
            let _ = events.unbounded_send(AgentEvent::InsertElements {
                elements: stamped,
                reply: tx,
            });
            let outcome: CanvasOpOutcome = rx
                .await
                .unwrap_or_else(|_| Err(CanvasOpError::internal("画布插入被取消（应用已关闭）")));
            let (is_error, message) = match &outcome {
                Ok(m) => (false, m.clone()),
                Err(e) => (true, e.message.clone()),
            };
            let _ = events.unbounded_send(AgentEvent::ToolResult {
                id,
                result: message.clone(),
                is_error,
            });
            outcome.map(|m| format!("已放置模板「{}」×{n} 个元素。{m}", template.name)).map_err(ToolError::from_op)
        }
    }
}

// --- List Templates ----------------------------------------------------------

pub struct ListTemplatesTool {
    pub events: UnboundedSender<AgentEvent>,
    pub templates_dir: std::path::PathBuf,
}

impl Tool for ListTemplatesTool {
    const NAME: &'static str = "list_templates";
    type Error = ToolError;
    type Args = NoArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<NoArgs>(
            Self::NAME,
            "列出模板库里所有可复用模板（名字、类别、大小、文字）。开始画漫画前先查一下，已有角色直接 stamp_template 复用。",
        );
        async move { def }
    }

    fn call(
        &self,
        _args: Self::Args,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
        let events = self.events.clone();
        let templates_dir = self.templates_dir.clone();
        let name = Self::NAME;
        async move {
            let id = next_tool_id(name);
            let _ = events.unbounded_send(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.to_string(),
                args: Value::Null,
            });
            let list = crate::scene::templates::list(&templates_dir);
            let result = if list.is_empty() {
                "模板库为空：先用绘图工具画出角色/场景，再 save_template 保存".to_string()
            } else {
                list.iter()
                    .map(|t| format!("- {}", t.one_line()))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let _ = events.unbounded_send(AgentEvent::ToolResult {
                id,
                result: result.clone(),
                is_error: false,
            });
            Ok(result)
        }
    }
}

// --- Speech Bubble -----------------------------------------------------------

/// Arguments for `draw_speech_bubble`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SpeechBubbleArgs {
    /// 气泡外接框左上角 X。
    pub x: f64,
    /// 气泡外接框左上角 Y。
    pub y: f64,
    /// 气泡宽（世界单位）。正文台词建议 ≥ 140。
    pub w: f64,
    /// 气泡高（世界单位）。一行台词建议 ≥ 56。
    pub h: f64,
    /// 台词内容。过长会自动换行（按气泡内宽），放不下就加高气泡或精简文字。
    pub text: String,
    /// 气泡形状：speech（椭圆对话泡，默认）/ burst（爆炸星形框，惊叫/巨响，
    /// 淡黄底）/ thought（思考云，云朵边 + 圆点尾迹，内心独白）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    /// 尾巴方向（指向说话者嘴部）：down_left（默认）/ down_right / up_left /
    /// up_right / none（thought 的圆点尾迹也沿此方向；burst 无尾巴）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    /// 台词字号。默认 16。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    /// 可选样式覆盖（描边/填充等）。默认 speech/thought 白底、burst 淡黄底，
    /// 黑边实心。
    #[serde(default, deserialize_with = "de_style")]
    pub style: CanvasStyle,
}

/// 漫画对话气泡：尾巴三角（先画，垫底）→ 白底椭圆（盖住接缝）→ 绑定文字
/// （居中、随容器移动）。一次 InsertElements 插入三个元素。
pub struct SpeechBubbleTool {
    pub events: UnboundedSender<AgentEvent>,
}

/// 气泡默认样式：白底黑边实心（漫画标准观感），style 参数可逐字段覆盖。
fn bubble_base_style() -> crate::scene::ElementStyle {
    let mut s = crate::scene::ElementStyle::default();
    s.stroke = 0x1e1e1e;
    s.stroke_width = 2.0;
    s.background = Some(0xff_ff_ff);
    s.fill_style = crate::scene::FillStyle::Solid;
    s.roughness = 1.0;
    s
}

impl Tool for SpeechBubbleTool {
    const NAME: &'static str = "draw_speech_bubble";
    type Error = ToolError;
    type Args = SpeechBubbleArgs;
    type Output = String;

    fn definition(
        &self,
        _prompt: String,
    ) -> impl std::future::Future<Output = ToolDefinition> + Send {
        let def = tool_def::<SpeechBubbleArgs>(
            Self::NAME,
            "画漫画气泡（自动换行居中）：shape=speech 椭圆对话泡（默认，尾巴指向说话者）/ burst 爆炸星形淡黄框（惊叫、巨响、怒吼）/ thought 思考云（内心独白，圆点尾迹）。tail 指定尾巴方向：down_left 默认 / down_right / up_left / up_right / none。返回气泡与文字的 id，改台词用 update_element 改文字 id。",
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
            if let Err(e) = validate_bubble(&args) {
                return fail_tool(&events, id, name, args_json, e).await;
            }
            let shape = args.shape.as_deref().unwrap_or("speech");
            let dir = args.tail.as_deref().unwrap_or("down_left");
            let (ux, uy): (f64, f64) = match dir {
                "none" => (0.0, 0.0),
                "down_left" => (-1.0, 1.0),
                "down_right" => (1.0, 1.0),
                "up_left" => (-1.0, -1.0),
                "up_right" => (1.0, -1.0),
                _ => unreachable!("validate_bubble 已拒绝未知方向"),
            };
            // burst 经典淡黄底；speech/thought 白底。
            let base = if shape == "burst" {
                let mut s = bubble_base_style();
                s.background = Some(0xff_f3_bf);
                s
            } else {
                bubble_base_style()
            };
            let style = args.style.clone().merge_into(base);
            let cx = args.x + args.w / 2.0;
            let cy = args.y + args.h / 2.0;
            let body_id = new_element_id();
            let mut elements: Vec<crate::scene::Element> = Vec::new();
            match shape {
                // 爆炸星形框（惊叫/巨响）：14 点 7 尖角，无尾巴，文字直接
                // 绑定在星形上。
                "burst" => {
                    let n = 14;
                    let pts: Vec<crate::scene::WPoint> = (0..n)
                        .map(|i| {
                            let t = i as f64 / n as f64 * std::f64::consts::TAU;
                            let (rx, ry) = if i % 2 == 0 {
                                (args.w * 0.56, args.h * 0.56)
                            } else {
                                (args.w * 0.39, args.h * 0.39)
                            };
                            crate::scene::WPoint::new(cx + rx * t.cos(), cy + ry * t.sin())
                        })
                        .collect();
                    elements.push(crate::scene::Element::from_absolute_points_with_id(
                        body_id,
                        |points| crate::scene::ElementKind::Polygon {
                            points,
                            smooth: false,
                        },
                        pts,
                        style.clone(),
                    ));
                }
                // 思考云：波动半径的平滑闭合曲线 + 沿 tail 方向的两颗圆点
                // 尾迹（老夫子式内心独白）。
                "thought" => {
                    let (rx, ry) = (args.w / 2.0, args.h / 2.0);
                    if dir != "none" {
                        let norm = (ux * ux + uy * uy).sqrt();
                        let (dux, duy) = (ux / norm, uy / norm);
                        let theta = duy.atan2(dux);
                        let ex = cx + rx * theta.cos();
                        let ey = cy + ry * theta.sin();
                        let len = (0.45 * args.w.min(args.h)).max(24.0);
                        for (k, d) in [(0.45, 16.0), (0.9, 10.0)] {
                            let dcx = ex + dux * len * k;
                            let dcy = ey + duy * len * k;
                            elements.push(crate::scene::Element::new_with_id(
                                new_element_id(),
                                crate::scene::ElementKind::Ellipse,
                                crate::scene::WBounds::new(
                                    dcx - d / 2.0,
                                    dcy - d / 2.0,
                                    d,
                                    d,
                                ),
                                style.clone(),
                            ));
                        }
                    }
                    let n = 12;
                    let pts: Vec<crate::scene::WPoint> = (0..n)
                        .map(|i| {
                            let t = i as f64 / n as f64 * std::f64::consts::TAU;
                            let r = if i % 2 == 0 { 1.0 } else { 0.86 };
                            crate::scene::WPoint::new(
                                cx + rx * r * t.cos(),
                                cy + ry * r * t.sin(),
                            )
                        })
                        .collect();
                    elements.push(crate::scene::Element::from_absolute_points_with_id(
                        body_id,
                        |points| crate::scene::ElementKind::Polygon {
                            points,
                            smooth: true,
                        },
                        pts,
                        style.clone(),
                    ));
                }
                // 默认椭圆对话泡：尾巴三角垫底 + 白底椭圆盖住接缝。
                _ => {
                    if dir != "none" {
                        // 尾巴：在椭圆边上取朝向点，底边两端向圆心收 15%（被椭圆
                        // 盖住接缝），尖端沿方向外推。
                        let norm = (ux * ux + uy * uy).sqrt();
                        let (ux, uy) = (ux / norm, uy / norm);
                        let theta = uy.atan2(ux);
                        let (rx, ry) = (args.w / 2.0, args.h / 2.0);
                        let bx = cx + rx * theta.cos();
                        let by = cy + ry * theta.sin();
                        let len = (0.30 * args.w.min(args.h)).max(18.0);
                        let tip = crate::scene::WPoint::new(bx + ux * len, by + uy * len);
                        let half = (0.06 * args.w).clamp(5.0, 14.0);
                        let pull = 0.85;
                        let b1 = crate::scene::WPoint::new(
                            cx + (bx + -uy * half - cx) * pull,
                            cy + (by + ux * half - cy) * pull,
                        );
                        let b2 = crate::scene::WPoint::new(
                            cx + (bx - -uy * half - cx) * pull,
                            cy + (by - ux * half - cy) * pull,
                        );
                        elements.push(crate::scene::Element::from_absolute_points_with_id(
                            new_element_id(),
                            |points| crate::scene::ElementKind::Polygon {
                                points,
                                smooth: false,
                            },
                            vec![b1, tip, b2],
                            style.clone(),
                        ));
                    }
                    elements.push(crate::scene::Element::new_with_id(
                        body_id,
                        crate::scene::ElementKind::Ellipse,
                        crate::scene::WBounds::new(args.x, args.y, args.w, args.h),
                        style,
                    ));
                }
            }
            // 台词：绑定到主体（椭圆/云/星）的居中标签；初始包围盒给个粗估，
            // 插入后由 pending_measure 精确重排。burst 的文字区按星形内圈收。
            let font_size = args.font_size.unwrap_or(16.0);
            let text_width = if shape == "burst" {
                (args.w * 0.62).max(30.0)
            } else {
                (args.w - 40.0).max(30.0)
            };
            let mut label = crate::scene::Element::new(
                crate::scene::ElementKind::Text {
                    text: crate::ai::canvas_ops::normalize_text(args.text.clone()),
                    font_size,
                    font_family: crate::render::HANDWRITTEN_FONT.to_string(),
                    wrap_width: Some(text_width),
                    min_height: None,
                    container_id: Some(body_id),
                    text_align: crate::scene::TextAlign::Center,
                    anchor: None,
                },
                crate::scene::WBounds::new(
                    cx - text_width / 2.0,
                    cy - font_size * 0.7,
                    text_width,
                    font_size * 1.4,
                ),
                crate::scene::ElementStyle::default(),
            );
            label.style.roughness = 0.0;
            elements.push(label);
            let n = elements.len();
            let _ = events.unbounded_send(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.to_string(),
                args: args_json,
            });
            let (tx, rx) = futures::channel::oneshot::channel();
            let _ = events.unbounded_send(AgentEvent::InsertElements {
                elements,
                reply: tx,
            });
            let outcome: CanvasOpOutcome = rx
                .await
                .unwrap_or_else(|_| Err(CanvasOpError::internal("画布插入被取消（应用已关闭）")));
            let (is_error, message) = match &outcome {
                Ok(m) => (false, m.clone()),
                Err(e) => (true, e.message.clone()),
            };
            let _ = events.unbounded_send(AgentEvent::ToolResult {
                id,
                result: message.clone(),
                is_error,
            });
            outcome
                .map(|m| {
                    let label = match shape {
                        "burst" => "爆炸气泡",
                        "thought" => "思考气泡",
                        _ => "对话气泡",
                    };
                    format!("已添加{label}（{n} 个元素）。{m}")
                })
                .map_err(ToolError::from_op)
        }
    }
}

fn validate_bubble(args: &SpeechBubbleArgs) -> Result<(), ToolError> {
    if args.text.trim().is_empty() {
        return Err(ToolError::invalid_args("台词内容不能为空"));
    }
    for (n, v) in [("x", args.x), ("y", args.y), ("w", args.w), ("h", args.h)] {
        if !v.is_finite() {
            return Err(ToolError::invalid_args(format!("{n} 必须是有限数值")));
        }
    }
    if args.w <= 0.0 || args.h <= 0.0 {
        return Err(ToolError::invalid_args("气泡宽高必须为正数"));
    }
    if let Some(fs) = args.font_size {
        if !fs.is_finite() || fs <= 0.0 {
            return Err(ToolError::invalid_args("字号必须为正数"));
        }
    }
    match args.shape.as_deref() {
        None | Some("speech") | Some("burst") | Some("thought") => {}
        Some(other) => {
            return Err(ToolError::invalid_args(format!(
                "shape 只支持 speech / burst / thought，收到 {other}"
            )))
        }
    }
    match args.tail.as_deref() {
        None | Some("down_left") | Some("down_right") | Some("up_left") | Some("up_right")
        | Some("none") => {}
        Some(other) => {
            return Err(ToolError::invalid_args(format!(
                "tail 只支持 down_left / down_right / up_left / up_right / none，收到 {other}"
            )))
        }
    }
    args.style.validate().map_err(ToolError::invalid_args)?;
    Ok(())
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
    active_skill: super::skills::ActiveSkill,
    templates_dir: std::path::PathBuf,
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
        Box::new(PoseElementTool {
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
        Box::new(SetPaperTextureTool {
            events: events.clone(),
        }),
        Box::new(PolygonTool {
            events: events.clone(),
        }),
        Box::new(AddImageTool {
            events: events.clone(),
        }),
        Box::new(SmoothShapeTool {
            events: events.clone(),
        }),
        Box::new(MindmapTool {
            events: events.clone(),
        }),
        Box::new(UseSkillTool {
            events: events.clone(),
            active: active_skill,
        }),
        Box::new(AddPageTool {
            events: events.clone(),
        }),
        Box::new(DeletePageTool {
            events: events.clone(),
        }),
        Box::new(SaveTemplateTool {
            events: events.clone(),
            snapshot: snapshot.clone(),
            templates_dir: templates_dir.clone(),
        }),
        Box::new(StampTemplateTool {
            events: events.clone(),
            templates_dir: templates_dir.clone(),
        }),
        Box::new(ListTemplatesTool {
            events: events.clone(),
            templates_dir: templates_dir.clone(),
        }),
        Box::new(SpeechBubbleTool {
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
        assert_eq!(
            crate::scene::mindmap::count_nodes(&crate::scene::mindmap::MindmapNodeInput::from(
                &root
            )),
            41
        );
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

    #[test]
    fn validate_pose_requires_id_and_points() {
        let mk = |id: &str, pts: Vec<(f64, f64)>| PoseElementArgs {
            id: id.into(),
            points: pts.into_iter().map(|(x, y)| OpPoint { x, y }).collect(),
        };
        assert!(validate_pose(&mk("a1b2c3d4", vec![(0.0, 0.0), (10.0, 10.0)])).is_ok());
        assert!(validate_pose(&mk("  ", vec![(0.0, 0.0), (1.0, 1.0)])).is_err());
        assert!(validate_pose(&mk("a1", vec![(0.0, 0.0)])).is_err());
        assert!(validate_pose(&mk("a1", vec![(0.0, 0.0), (f64::NAN, 1.0)])).is_err());
    }

    /// pose_element 链路：ToolCall → SetElementPoints → 回执 → ToolResult。
    #[test]
    fn pose_tool_emits_set_element_points_op() {
        use futures::task::noop_waker_ref;
        use std::task::Context;

        let (tx, mut rx) = futures::channel::mpsc::unbounded::<AgentEvent>();
        let tool = PoseElementTool {
            events: tx,
            snapshot: Arc::new(std::sync::Mutex::new(vec![snap("a1b2c3d4")])),
        };
        let args = PoseElementArgs {
            id: "a1b2c3d4".into(),
            points: vec![
                OpPoint { x: 150.0, y: 300.0 },
                OpPoint { x: 160.0, y: 330.0 },
                OpPoint { x: 120.0, y: 300.0 },
            ],
        };
        let mut fut = Box::pin(tool.call(args));
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(_) => panic!("completed before reply"),
        }
        let mut saw_op = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::CanvasOp { op, reply, .. } = event {
                match op {
                    CanvasOp::SetElementPoints { id, points } => {
                        assert_eq!(id, "a1b2c3d4");
                        assert_eq!(points.len(), 3);
                    }
                    other => panic!("期望 SetElementPoints，实际 {other:?}"),
                }
                let _ = reply.send(Ok("已调整 id=a1b2c3d4 的形状（3 个点）".into()));
                saw_op = true;
            }
        }
        assert!(saw_op, "no SetElementPoints op");
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Ready(Ok(msg)) => assert!(msg.contains("已调整")),
            _ => panic!("did not complete after reply"),
        }
    }

    fn mk_bubble() -> SpeechBubbleArgs {
        SpeechBubbleArgs {
            x: 100.0,
            y: 80.0,
            w: 200.0,
            h: 110.0,
            text: "你好，世界".into(),
            shape: None,
            tail: None,
            font_size: None,
            style: CanvasStyle::default(),
        }
    }

    #[test]
    fn validate_bubble_directions_and_bounds() {
        assert!(validate_bubble(&mk_bubble()).is_ok());
        for tail in ["down_left", "down_right", "up_left", "up_right", "none"] {
            let a = SpeechBubbleArgs {
                tail: Some(tail.into()),
                ..mk_bubble()
            };
            assert!(validate_bubble(&a).is_ok(), "tail={tail}");
        }
        for shape in ["speech", "burst", "thought"] {
            let a = SpeechBubbleArgs {
                shape: Some(shape.into()),
                ..mk_bubble()
            };
            assert!(validate_bubble(&a).is_ok(), "shape={shape}");
        }
        let unknown = SpeechBubbleArgs {
            tail: Some("sideways".into()),
            ..mk_bubble()
        };
        assert!(validate_bubble(&unknown).is_err());
        let bad_shape = SpeechBubbleArgs {
            shape: Some("round".into()),
            ..mk_bubble()
        };
        assert!(validate_bubble(&bad_shape).is_err());
        let empty = SpeechBubbleArgs {
            text: "  ".into(),
            ..mk_bubble()
        };
        assert!(validate_bubble(&empty).is_err());
        let zero = SpeechBubbleArgs {
            w: 0.0,
            ..mk_bubble()
        };
        assert!(validate_bubble(&zero).is_err());
    }

    /// Speech-bubble tool chain: ToolCall → InsertElements（3 个元素：尾巴、
    /// 椭圆、绑定文字）→ 主线程回执 → ToolResult is_error=false。
    #[test]
    fn bubble_tool_emits_three_bound_elements() {
        use futures::task::noop_waker_ref;
        use std::task::Context;

        let (tx, mut rx) = futures::channel::mpsc::unbounded::<AgentEvent>();
        let tool = SpeechBubbleTool { events: tx };
        let mut fut = Box::pin(tool.call(mk_bubble()));
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);

        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(_) => panic!("tool completed before reply"),
        }

        let mut inserted: Option<Vec<crate::scene::Element>> = None;
        let mut replied = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::InsertElements { elements, reply } => {
                    reply
                        .send(Ok("已插入 3 个元素，id 依次为：a1, b2, c3".to_string()))
                        .expect("reply send");
                    inserted = Some(elements);
                    replied = true;
                }
                AgentEvent::ToolResult { is_error, .. } => {
                    panic!("ToolResult before reply: is_error={is_error}");
                }
                _ => {}
            }
        }
        assert!(replied, "no InsertElements event to reply to");

        let elements = inserted.unwrap();
        assert_eq!(elements.len(), 3, "尾巴 + 椭圆 + 文字");
        // 第一个是尾巴三角（有尾巴方向时），中间是椭圆，最后是绑定到椭圆的居中文字。
        let tail = &elements[0];
        assert!(tail.is_point_based(), "尾巴应为多边形");
        let ellipse = &elements[1];
        assert!(matches!(ellipse.kind, crate::scene::ElementKind::Ellipse));
        // 椭圆白底实心（漫画标准观感）。
        assert_eq!(ellipse.style.background, Some(0xff_ff_ff));
        assert_eq!(ellipse.style.fill_style, crate::scene::FillStyle::Solid);
        match &elements[2].kind {
            crate::scene::ElementKind::Text {
                container_id, ..
            } => assert_eq!(*container_id, Some(ellipse.id)),
            other => panic!("期望绑定文字，实际 {other:?}"),
        }
        // 尾巴尖端在气泡外接框之下（down_left 方向：尖端 y > 气泡底边）。
        let tip_y = tail.absolute_points().iter().map(|p| p.y).fold(f64::MIN, f64::max);
        assert!(tip_y > 80.0 + 110.0, "尾巴应伸到气泡外 tip_y={tip_y}");

        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Ready(Ok(msg)) => assert!(msg.contains("对话气泡")),
            _ => panic!("tool did not complete after reply"),
        }
        let mut saw_ok_result = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::ToolResult {
                is_error, result, ..
            } = event
            {
                saw_ok_result = true;
                assert!(!is_error, "成功回执被记为错误: {result}");
            }
        }
        assert!(saw_ok_result);
    }

    /// burst 气泡：14 点星形（淡黄底）+ 绑定文字，无尾巴三角。
    #[test]
    fn burst_bubble_emits_star_and_label() {
        use futures::task::noop_waker_ref;
        use std::task::Context;

        let (tx, mut rx) = futures::channel::mpsc::unbounded::<AgentEvent>();
        let tool = SpeechBubbleTool { events: tx };
        let args = SpeechBubbleArgs {
            shape: Some("burst".into()),
            tail: Some("down_left".into()),
            ..mk_bubble()
        };
        let mut fut = Box::pin(tool.call(args));
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(_) => panic!("completed before reply"),
        }
        let mut elements = None;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::InsertElements { elements: e, reply } = event {
                let _ = reply.send(Ok("已插入 2 个元素".into()));
                elements = Some(e);
            }
        }
        let elements = elements.unwrap();
        assert_eq!(elements.len(), 2, "星形 + 文字，无尾巴");
        let star = &elements[0];
        match &star.kind {
            crate::scene::ElementKind::Polygon { points, smooth } => {
                assert_eq!(points.len(), 14);
                assert!(!*smooth);
            }
            other => panic!("期望星形多边形，实际 {other:?}"),
        }
        // 淡黄底。
        assert_eq!(star.style.background, Some(0xff_f3_bf));
        // 文字绑定星形，文字区收窄（*0.62）。
        match &elements[1].kind {
            crate::scene::ElementKind::Text {
                container_id: Some(cid),
                wrap_width,
                ..
            } => {
                assert_eq!(*cid, star.id);
                assert!((wrap_width.unwrap() - 200.0 * 0.62).abs() < 1e-9);
            }
            other => panic!("期望绑定文字，实际 {other:?}"),
        }
    }

    /// thought 气泡：两颗圆点尾迹 + 平滑云朵 + 绑定文字（无三角尾巴）。
    #[test]
    fn thought_bubble_emits_dots_cloud_label() {
        use futures::task::noop_waker_ref;
        use std::task::Context;

        let (tx, mut rx) = futures::channel::mpsc::unbounded::<AgentEvent>();
        let tool = SpeechBubbleTool { events: tx };
        let args = SpeechBubbleArgs {
            shape: Some("thought".into()),
            tail: Some("down_left".into()),
            ..mk_bubble()
        };
        let mut fut = Box::pin(tool.call(args));
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(_) => panic!("completed before reply"),
        }
        let mut elements = None;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::InsertElements { elements: e, reply } = event {
                let _ = reply.send(Ok("已插入 4 个元素".into()));
                elements = Some(e);
            }
        }
        let elements = elements.unwrap();
        assert_eq!(elements.len(), 4, "两颗圆点 + 云朵 + 文字");
        let dot1 = &elements[0];
        let dot2 = &elements[1];
        assert!(matches!(dot1.kind, crate::scene::ElementKind::Ellipse));
        assert!(matches!(dot2.kind, crate::scene::ElementKind::Ellipse));
        // 圆点沿 down_left 方向递减、远离气泡（在气泡外接框下方）。
        assert!(dot1.bounds.w > dot2.bounds.w);
        assert!(dot2.bounds.y > 80.0 + 110.0, "尾迹应在气泡下方之外");
        let cloud = &elements[2];
        match &cloud.kind {
            crate::scene::ElementKind::Polygon { points, smooth } => {
                assert_eq!(points.len(), 12);
                assert!(*smooth);
            }
            other => panic!("期望平滑云朵，实际 {other:?}"),
        }
        match &elements[3].kind {
            crate::scene::ElementKind::Text {
                container_id: Some(cid),
                ..
            } => assert_eq!(*cid, cloud.id),
            other => panic!("期望绑定文字，实际 {other:?}"),
        }
    }
}
