//! The boundless drawing agent: a rig `Agent` built on an OpenAI-compatible
//! endpoint, equipped with canvas drawing tools.
//!
//! **Design (mirrors lakemind):** tools own their full lifecycle. Each tool's
//! `call()` emits a three-phase event sequence (`ToolCall` → `CanvasOp` →
//! `ToolResult`) through the shared event channel; rig's own tool-call /
//! tool-result stream items are ignored. The agent loop only consumes the
//! stream for text/reasoning deltas and the final response, making it simple.
//!
//! Threading: the agent and its tools run on the tokio runtime; the canvas on
//! the GPUI main thread. Events flow through one unbounded channel that the
//! panel drains on the main thread (`CanvasOp` events mutate the scene, the
//! others update the UI).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context as _};
use futures::channel::mpsc::UnboundedSender;
use futures::StreamExt;
use rig_core::agent::MultiTurnStreamItem;
use rig_core::client::CompletionClient;
use rig_core::completion::GetTokenUsage;
use rig_core::message::Message as RigMessage;
use rig_core::providers::openai::{self, Client as OpenAIClient};
use rig_core::streaming::{StreamedAssistantContent, StreamingChat};

use super::canvas_ops::CanvasOp;
use super::client::ChatMessage;
use super::settings::AiSettings;
use super::tools::all_tools;

/// Monotonic counter for tool-call ids, so two tools starting in the same
/// instant still get distinct ids (the UI pairs `ToolCall`/`ToolResult` by id).
static TOOL_CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Generate a unique tool-call id. Called by each tool inside `call()`.
pub fn next_tool_id(name: &str) -> String {
    let n = TOOL_CALL_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("tool-{name}-{n}")
}

/// The concrete completion model type the agent is built on: OpenAI
/// Chat-Completions over a reqwest backend. Used as a type alias so the
/// streaming item generic resolves concretely.
pub type Model = openai::completion::CompletionModel;
/// Items yielded by an agent stream: `StreamedAssistantContent` + tool results.
#[allow(dead_code)]
pub type StreamItem = MultiTurnStreamItem<<Model as rig_core::completion::CompletionModel>::StreamingResponse>;

/// System prompt for the drawing agent. Describes the coordinate system,
/// available tools, and the expectation that the agent draws rather than only
/// narrating.
pub const SYSTEM_PROMPT: &str = r##"你是 boundless 白板应用的绘图助手。你的核心职责是通过调用绘图工具把内容画到画布上。

## 行动准则
1. 用户提到任何图表、流程图、示意图时，**立即调用绘图工具**，不要只回复文字。
2. 先画图，后说明。哪怕只画出一部分也比只说不画好。
3. 每次调用一个工具，简短思考下一步，再调下一个。用户能看到你的每一步操作。
4. 用中文简要说明你画了什么，不要逐条复述坐标。

## 工具说明
- draw_ellipse(x, y, w, h, text?)：画椭圆。用 text 参数写内部文字，如「开始」「结束」。
- draw_rectangle(x, y, w, h, text?)：画矩形。用 text 参数写步骤名，如「输入用户名和密码」。
- draw_diamond(x, y, w, h, text?)：画菱形（判断节点）。用 text 参数写条件，如「密码正确？」。
- draw_arrow(points, text?)：画带箭头的连线，points 是两个或更多坐标点。用于连接流程节点。\
用 text 参数在线上标注条件，如「是」「否」。
- draw_line(points, text?)：画无箭头的连线。同样支持 text 参数标注。
- draw_text(x, y, text)：画独立文本，用于标题或说明（不属于任何形状的文字）。
- 以上工具均可省略 style，沿用画板当前样式。

## 画布坐标系
- 原点在左上角，x 向右增大，y 向下增大。可见范围约 x∈[0,1600]、y∈[0,1000]。
- 典型矩形宽 140~180、高 60~80。元素间距 30~40，不要重叠。
- 颜色用 0xRRGGBB 整数，如 0x1e1e1e（黑色）、0xa5d8ff（浅蓝）。省略则用默认黑色描边。

## 布局规则（避免连线交叉）
- 主流程从上到下排列，分支向左右两侧展开，不要让连线跨越其他形状。
- 判断节点（菱形）的两个分支：一个向下（主路径），一个向左或向右（支路径），不要两个分支都向下交叉。
- 回环线（如"失败后回到输入"）走外侧绕行，不要穿过中间的形状。用多个折线点让线绕开障碍。
- 箭头端点对齐到形状的边缘（上边中点、下边中点、左边中点、右边中点），不要连到形状内部。
- 先规划好所有形状的位置，再画连线——这样你知道每条线的起点和终点在哪里，能避免交叉。

## 流程图范例
画一个登录流程图，你会这样调用：
1. draw_ellipse(350, 40, 120, 50, text=开始)
2. draw_rectangle(320, 130, 180, 60, text=输入账号密码)
3. draw_diamond(320, 230, 180, 80, text=验证成功？)
4. draw_rectangle(490, 360, 160, 60, text=进入主页)
5. draw_rectangle(150, 360, 160, 60, text=提示错误)
6. draw_arrow(points=[(410,90),(410,130)])
7. draw_arrow(points=[(410,190),(410,230)])
8. draw_arrow(points=[(500,270),(570,360)], text=是)
9. draw_arrow(points=[(320,270),(230,360)], text=否)
10. draw_arrow(points=[(150,360),(150,160),(310,160)])  # 回环：从提示错误绕外侧回到输入框左边"##;

/// One request to the drawing agent. Owns the per-request tool channel and the
/// running stream; drop the request to drop the stream (cancellation).
pub struct AgentRequest {
    /// Stream of agent events (text deltas, tool results, final response).
    pub events: futures::channel::mpsc::UnboundedReceiver<AgentEvent>,
    /// Set to true to cancel the in-flight request.
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// Events surfaced to the UI from an agent stream.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// A chunk of assistant text (streamed).
    Delta(String),
    /// A chunk of reasoning/thinking text (streamed). Shown in a collapsible
    /// panel that is expanded while streaming and auto-collapses on Done.
    Reasoning(String),
    /// A canvas op produced by a tool call — apply it to the board.
    CanvasOp(CanvasOp),
    /// The model made a tool call. `id` is rig's internal call id (used to pair
    /// with the later [`AgentEvent::ToolResult`]); `name` is the tool, `args` is
    /// the raw JSON arguments the model supplied. Shown as an expandable step
    /// in the "pending" state until the matching ToolResult arrives.
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// A tool call finished executing. `id` matches the `ToolCall` event's `id`
    /// so the UI can mark the corresponding step done; `result` is the tool's
    /// return text (e.g. "已添加到画布").
    ToolResult { id: String, result: String },
    /// Stream completed; `text` is the full final assistant message.
    /// `drew_anything` is true if at least one tool call was made this turn —
    /// the UI uses it to warn when the model replied without drawing.
    Done { text: String, drew_anything: bool },
    /// An error occurred.
    Error(String),
}

impl BoundlessAgent {
    /// Build the underlying rig agent for one request.
    ///
    /// Returns a fresh agent whose tools emit `ToolCall`/`CanvasOp`/`ToolResult`
    /// events through `events`. Built per-request so each request's toolset is
    /// wired to its own event channel and a clean tool-call history.
    fn build(
        settings: &AiSettings,
        events: UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<rig_core::agent::Agent<Model>> {
        if settings.api_key.is_empty() {
            return Err(anyhow!(
                "未配置 API Key：请在 AI 面板设置中填写，或设置环境变量 OPENAI_API_KEY"
            ));
        }
        // OpenAI-compatible client pointed at the configured base URL. We use
        // the Chat Completions API variant (`.completions_api()`) for the
        // broadest compatibility with "OpenAI-compatible" providers.
        let client: OpenAIClient = openai::Client::builder()
            .api_key(settings.api_key.trim().to_string())
            .base_url(settings.base_url.trim())
            .build()
            .context("无法创建 AI 客户端")?;
        let model = client.completions_api().completion_model(&settings.model);
        let agent = rig_core::agent::AgentBuilder::new(model)
            .preamble(SYSTEM_PROMPT)
            // Each tool call is now a separate turn (the system prompt encourages
            // step-by-step drawing), so a complex diagram needs many rounds.
            .default_max_turns(100)
            // Reasoning effort (low/medium/high) flattened to the top-level
            // request body as `reasoning_effort`. Non-reasoning models ignore
            // the extra field; the `flatten` in rig's OpenAI request struct
            // places it exactly where the API expects it.
            .additional_params(serde_json::json!({
                "reasoning_effort": settings.reasoning_effort.as_str()
            }))
            .tools(all_tools(events))
            .build();
        Ok(agent)
    }

    /// Start a streaming prompt to the drawing agent.
    ///
    /// `history` is the prior conversation (most recent last), excluding the
    /// new user message. The new user message is `prompt`. The agent's tools
    /// emit their lifecycle events (`ToolCall`/`CanvasOp`/`ToolResult`) directly
    /// through the returned event stream, so the caller drains one channel.
    pub fn stream(
        settings: &AiSettings,
        prompt: String,
        history: Vec<ChatMessage>,
    ) -> anyhow::Result<AgentRequest> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let agent = Self::build(settings, tx.clone())?;
        let chat_history: Vec<RigMessage> = history.into_iter().filter_map(msg_to_rig).collect();

        // stream_chat(prompt, history) returns a StreamingPromptRequest;
        // `.multi_turn(N)` raises the tool-calling depth, then awaiting it
        // yields the multi-turn stream.
        let request = agent.stream_chat(prompt, chat_history).multi_turn(100);

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_task = cancel.clone();

        super::client::tokio_runtime().spawn(async move {
            let mut stream = request.await;
            let mut full_text = String::new();
            // Whether any tool call passed through this turn — reported on Done
            // so the UI can warn when the model replied without drawing.
            let mut tool_called = false;
            while let Some(item) = stream.next().await {
                if cancel_task.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                match item {
                    Ok(stream_item) => {
                        if !handle_agent_item(
                            stream_item,
                            &tx,
                            &mut full_text,
                            &mut tool_called,
                        ) {
                            break;
                        }
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        // MaxTurnsError is a soft limit: the model drew as much
                        // as it could within the turn budget. Don't treat it as
                        // a hard error — finalize with whatever text/tools
                        // accumulated so the user sees the partial result
                        // instead of a scary error message.
                        if msg.contains("MaxTurnError") {
                            break;
                        }
                        let _ = tx.unbounded_send(AgentEvent::Error(msg));
                        return;
                    }
                }
            }
            let _ = tx.unbounded_send(AgentEvent::Done {
                text: full_text,
                drew_anything: tool_called,
            });
        });

        Ok(AgentRequest {
            events: rx,
            cancel,
        })
    }
}

/// A type with no state, used only to namespace the agent build/stream logic.
pub struct BoundlessAgent;

/// Handle one agent stream item: forward text/reasoning deltas as events and
/// accumulate the final text. Tool-call and tool-result stream items are
/// **ignored** — each tool emits its own richer `ToolCall`/`CanvasOp`/
/// `ToolResult` lifecycle from inside `call()` (see `tools::emit_tool_step`),
/// so rig's internal representation is redundant. Returns `false` if the stream
/// should stop (an error was emitted); `true` to continue. `tool_called` is set
/// true when a rig `ToolCall` stream item passes by (a fallback signal — the
/// primary lifecycle is tool-emitted), so the caller can report
/// `drew_anything` on Done.
fn handle_agent_item(
    item: MultiTurnStreamItem<
        <Model as rig_core::completion::CompletionModel>::StreamingResponse,
    >,
    tx: &futures::channel::mpsc::UnboundedSender<AgentEvent>,
    full_text: &mut String,
    tool_called: &mut bool,
) -> bool {
    match item {
        MultiTurnStreamItem::StreamAssistantItem(content) => match content {
            StreamedAssistantContent::Text(text) => {
                full_text.push_str(&text.text);
                let _ = tx.unbounded_send(AgentEvent::Delta(text.text));
            }
            // Reasoning deltas: stream the thinking text to the UI.
            StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                let _ = tx.unbounded_send(AgentEvent::Reasoning(reasoning));
            }
            // A complete reasoning block (some providers emit it at once).
            StreamedAssistantContent::Reasoning(r) => {
                let text = r.display_text();
                if !text.is_empty() {
                    let _ = tx.unbounded_send(AgentEvent::Reasoning(text));
                }
            }
            // rig's ToolCall stream item — the tool already emits its own
            // richer ToolCall event from call(). We only use this as a fallback
            // signal that *some* tool was invoked (for drew_anything).
            StreamedAssistantContent::ToolCall { .. } => {
                *tool_called = true;
            }
            _ => {}
        },
        // Tool results, completion calls, and any future variants: the tools
        // own their lifecycle; ignore rig's internal representations.
        MultiTurnStreamItem::StreamUserItem(_)
        | MultiTurnStreamItem::CompletionCall(_) => {}
        MultiTurnStreamItem::FinalResponse(final_resp) => {
            // Prefer the aggregated final text if the deltas didn't capture it.
            let text = final_resp.response().to_string();
            if !text.is_empty() && full_text.is_empty() {
                *full_text = text;
            }
        }
        // non_exhaustive: future variants are ignored.
        _ => {}
    }
    true
}

/// Convert a stored chat message into a rig message. System messages become
/// rig system messages; others become user/assistant. Unknown roles are dropped
/// (rig has no generic role).
fn msg_to_rig(m: ChatMessage) -> Option<RigMessage> {
    match m.role.as_str() {
        "system" => Some(RigMessage::System { content: m.content }),
        "user" => Some(RigMessage::User {
            content: rig_core::OneOrMany::one(rig_core::message::UserContent::text(m.content)),
        }),
        "assistant" => Some(RigMessage::Assistant {
            id: None,
            content: rig_core::OneOrMany::one(rig_core::message::AssistantContent::text(m.content)),
        }),
        _ => None,
    }
}

// Keep the GetTokenUsage / StreamingChoice imports referenced even if the
// generic bounds shift across rig versions.
#[allow(dead_code)]
fn _bounds_referenced<T: GetTokenUsage + Clone + Send + Sync + 'static>(_: T) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_user_and_assistant_messages() {
        let user = msg_to_rig(ChatMessage::user("hi")).unwrap();
        assert!(matches!(user, RigMessage::User { .. }));
        let asst = msg_to_rig(ChatMessage::assistant("hello")).unwrap();
        assert!(matches!(asst, RigMessage::Assistant { .. }));
        // unknown role is dropped
        let mut weird = ChatMessage::user("x");
        weird.role = "tool".into();
        assert!(msg_to_rig(weird).is_none());
    }
}
