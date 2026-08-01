//! The boundless drawing agent: a rig `Agent` built on an OpenAI-compatible
//! endpoint, equipped with canvas drawing tools.
//!
//! Threading model: the agent (and its tools) run on the tokio runtime, the
//! canvas on the GPUI main thread. A [`CanvasOp`] channel bridges them: the
//! caller creates a fresh sender per request, the tools forward ops through it,
//! and the caller drains ops on the main thread to mutate the scene. The agent
//! itself is cheap to rebuild (a cloned HTTP client + model name + toolset), so
//! we build one per request — that also gives each request its own toolset
//! wired to its own canvas channel.

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
pub const SYSTEM_PROMPT: &str = "\
你是 boundless 白板应用的绘图助手。当用户描述一个图表、流程图、示意图或任何可视内容时，\
你应该通过调用绘图工具把内容直接画到画布上，而不是只用文字描述。

## 画布与坐标系
- 画布使用世界坐标：原点在左上角，x 向右增大，y 向下增大。
- 一个合理的可见范围大约是 x∈[0,1600]、y∈[0,1000]。尽量在这个范围内布局。
- 单位是“世界单位”，与字号同尺度。典型矩形宽 120~200、高 60~80；典型字号 16~24。

## 颜色
颜色用 0xRRGGBB 整数表示，例如黑色 0x1e1e1e、浅蓝 0xa5d8ff。可省略以使用默认黑色描边、无填充。

## 可用工具
- draw_rectangle / draw_ellipse / draw_diamond：画形状，参数 x,y,w,h（左上角+宽高）。
- draw_line：画无箭头折线，参数 points 为≥2 个点。
- draw_arrow：画带箭头折线，参数 points 为≥2 个点；默认末端(end_arrowhead=true)有箭头。用于流程图方向。
- draw_text：添加文本，参数 x,y,text（可含换行）、可选 font_size、align。
- 每个工具的 style 都可省略；省略字段会沿用画板当前样式。

## 绘图建议
- 流程图：用 draw_rectangle 表示步骤，draw_diamond 表示判断，draw_arrow 连接它们，draw_text 写标签。\
先画形状再画箭头，箭头端点对齐到形状边缘。
- 文字标签放在对应形状内部或正下方。
- 紧凑布局，元素之间留约 20~40 单位间距，不要重叠。

## 沟通
用中文简要说明你画了什么。每次绘图后用一两句话总结。不要逐条复述坐标。";

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
    /// A canvas op produced by a tool call — apply it to the board.
    CanvasOp(CanvasOp),
    /// The model made a tool call (by tool name); optional UI hint.
    ToolCall(String),
    /// Stream completed; `text` is the full final assistant message.
    Done { text: String },
    /// An error occurred.
    Error(String),
}

impl BoundlessAgent {
    /// Build the underlying rig agent for one request.
    ///
    /// Returns a fresh agent whose tools forward `CanvasOp`s through `sender`.
    /// Built per-request so each request's toolset is wired to its own canvas
    /// channel and a clean tool-call history.
    fn build(
        settings: &AiSettings,
        sender: UnboundedSender<CanvasOp>,
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
            // Give the model room for several tool rounds (e.g. draw shapes,
            // then arrows, then text) within a single request.
            .default_max_turns(8)
            // Reasoning effort (low/medium/high) flattened to the top-level
            // request body as `reasoning_effort`. Non-reasoning models ignore
            // the extra field; the `flatten` in rig's OpenAI request struct
            // places it exactly where the API expects it.
            .additional_params(serde_json::json!({
                "reasoning_effort": settings.reasoning_effort.as_str()
            }))
            .tools(all_tools(sender))
            .build();
        Ok(agent)
    }

    /// Start a streaming prompt to the drawing agent.
    ///
    /// `history` is the prior conversation (most recent last), excluding the
    /// new user message. The new user message is `prompt`. The agent creates
    /// its own canvas-op channel (the tools' sender) and bridges ops into the
    /// returned event stream as `AgentEvent::CanvasOp`, so the caller only has
    /// one stream to drain.
    pub fn stream(
        settings: &AiSettings,
        prompt: String,
        history: Vec<ChatMessage>,
    ) -> anyhow::Result<AgentRequest> {
        // Per-request canvas-op channel: the tools forward ops through the
        // sender; the agent merges ops into the event stream below.
        let (canvas_tx, mut canvas_rx) = futures::channel::mpsc::unbounded::<CanvasOp>();
        let agent = Self::build(settings, canvas_tx)?;
        let chat_history: Vec<RigMessage> = history.into_iter().filter_map(msg_to_rig).collect();

        // stream_chat(prompt, history) returns a StreamingPromptRequest;
        // `.multi_turn(N)` raises the tool-calling depth, then awaiting it
        // yields the multi-turn stream.
        let request = agent.stream_chat(prompt, chat_history).multi_turn(8);

        let (tx, rx) = futures::channel::mpsc::unbounded();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_task = cancel.clone();

        super::client::tokio_runtime().spawn(async move {
            let mut stream = request.await;
            let mut full_text = String::new();
            // Merge the agent stream with the canvas-op stream so ops are
            // forwarded promptly (a tool's op is buffered in canvas_rx as soon
            // as the tool runs, even between two stream items).
            loop {
                if cancel_task.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                // Race the next agent item against the next canvas op.
                let next = futures::future::select(stream.next(), canvas_rx.next()).await;
                match next {
                    futures::future::Either::Left((item, _)) => {
                        // stream item is Option<Result<MultiTurnStreamItem, StreamingError>>
                        let Some(item) = item else { break };
                        match item {
                            Ok(stream_item) => {
                                if !handle_agent_item(stream_item, &tx, &mut full_text) {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = tx.unbounded_send(AgentEvent::Error(format!("{e:#}")));
                                return;
                            }
                        }
                    }
                    futures::future::Either::Right((op, _)) => {
                        let Some(op) = op else { break };
                        let _ = tx.unbounded_send(AgentEvent::CanvasOp(op));
                    }
                }
            }
            let _ = tx.unbounded_send(AgentEvent::Done { text: full_text });
        });

        Ok(AgentRequest {
            events: rx,
            cancel,
        })
    }
}

/// A type with no state, used only to namespace the agent build/stream logic.
pub struct BoundlessAgent;

/// Handle one agent stream item: forward it as an `AgentEvent` and accumulate
/// the final text. Returns `false` if the stream should stop (an error was
/// emitted); `true` to continue.
fn handle_agent_item(
    item: MultiTurnStreamItem<
        <Model as rig_core::completion::CompletionModel>::StreamingResponse,
    >,
    tx: &futures::channel::mpsc::UnboundedSender<AgentEvent>,
    full_text: &mut String,
) -> bool {
    match item {
        MultiTurnStreamItem::StreamAssistantItem(content) => match content {
            StreamedAssistantContent::Text(text) => {
                full_text.push_str(&text.text);
                let _ = tx.unbounded_send(AgentEvent::Delta(text.text));
            }
            // Tool calls are surfaced to the UI as hints; the actual canvas
            // effect arrives via the merged CanvasOp stream.
            StreamedAssistantContent::ToolCall { tool_call, .. } => {
                let _ = tx.unbounded_send(AgentEvent::ToolCall(tool_call.function.name.clone()));
            }
            _ => {}
        },
        MultiTurnStreamItem::StreamUserItem(_) | MultiTurnStreamItem::CompletionCall(_) => {}
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
