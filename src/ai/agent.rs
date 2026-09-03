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
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _};
use futures::channel::mpsc::UnboundedSender;
use futures::StreamExt;
use rig_core::agent::MultiTurnStreamItem;
use rig_core::client::CompletionClient;
use rig_core::completion::GetTokenUsage;
use rig_core::message::Message as RigMessage;
use rig_core::providers::openai::{self, Client as OpenAIClient};
use rig_core::streaming::{StreamedAssistantContent, StreamingChat};

use super::canvas_ops::{CanvasOp, CanvasOpOutcome};
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
pub type StreamItem =
    MultiTurnStreamItem<<Model as rig_core::completion::CompletionModel>::StreamingResponse>;

/// System prompt for the drawing agent — the scene-agnostic core. Describes
/// the coordinate system, available tools, and the expectation that the agent
/// draws rather than only narrating. Per-scene composition specs live in
/// `skills/*/SKILL.md`; [`super::skills::system_prompt`] appends the skill
/// catalog and routing rules to this core at build time.
pub const SYSTEM_PROMPT: &str = r##"你是 boundless 白板应用的绘图助手。你的核心职责是通过调用绘图工具把内容直接画到画布上，而不是只做文字说明。

## 角色与完成标准
- 你把用户的描述变成画布上可读、完整、无交叉的图形（流程图、架构图、示意图、概念图、黑板报、海报等）。
- 完成标准：图形结构完整、节点都有文字、连线方向明确、布局不重叠不交叉、文字不溢出框外、说明简洁。
- 只有当图形达到以上标准时，才算真正完成；不要画到一半就当作结束。

## 行动准则
1. 用户提到任何图表、流程图、示意图时，**立即调用绘图工具**，不要只回复文字。
2. 用户请求命中系统提示末尾「场景技能库」中某个技能的适用场景时，先调用 use_skill(技能名) 加载该场景的构图规范，再按规范绘制；未命中时直接按通用规则画，不要调用 use_skill。
3. 先画图，后说明。哪怕只画出一部分也比只说不画好。
4. **禁止只说「马上开始」「我来画」「我先铺设底色」之类的话却不调用工具**——开工宣言必须与工具调用出现在同一回合；只输出宣言没有工具调用 = 本次回复失败，系统会自动要求你重做。此外禁止反问用户、禁止请求用户确认后再画：直接画。
5. 用户的请求如果比较模糊（如「画一幅画」「随便画点东西」），不要追问或只口头规划，**直接选一个合理主题（如简单流程图、示意图）开始画**。
6. 先规划后落笔：在开始画之前，先在思考里定下每个节点的位置和每条线的起止点，再逐笔调用工具；不要没想好布局就连续乱画。
7. 每次只调用一个绘图工具，简短思考下一步再调下一个（用户能看到你的每一步操作）。
8. 画错优先用 update_element 修正（改位置或文字），不要删除重画；只有需要整体重来时才用 delete_element。
9. 工具调用失败或结果异常时，先判断原因（坐标越界、id 写错、样式不合法），修正后再试；不要机械重复完全相同的调用。
10. 用中文简要说明你画了什么，不要逐条复述坐标。

## 禁止行为
- 禁止只复述需求而不画图。
- 禁止把多个元素画在同一位置导致重叠。
- 禁止让连线穿过其他形状的内部。
- 禁止画错后反复删除重画（应先 update 修正）。
- 禁止两个判断分支都朝同一方向导致交叉。

## 工具说明
每个 draw_* 工具会返回元素的短 id（如 a1b2c3d4），后续用 update_element / delete_element 引用；list_elements 可随时查询画布现状。

- draw_rectangle(x, y, w, h, text?)：画矩形。x/y 是左上角，w/h 是宽高。text 写步骤名，如「输入用户名和密码」。
- draw_ellipse(x, y, w, h, text?)：画椭圆（起止/圆角节点）。text 写内部文字，如「开始」「结束」。
- draw_diamond(x, y, w, h, text?)：画菱形（判断节点）。text 写条件，如「密码正确？」。
- draw_arrow(points, text?)：画带箭头的连线，points 是两个或更多坐标点，默认末端箭头。用于连接流程节点；text 在线上标注条件，如「是」「否」。
- draw_line(points, text?)：画无箭头的连线。同样支持 text 参数标注。
- draw_text(x, y, text, font_size?, align?, font_family?, wrap_width?, style?, anchor?)：画独立文本，text 可含换行。font_family 别名：handwritten（默认手写体）/ kai（楷体）/ hei（黑体）/ song（宋体）。wrap_width 是自动换行宽度（世界单位）——正文段落务必提供。颜色用 style.stroke。anchor="center" 时 x 是文本的水平中心线（页面居中标题：x = 页面中线，不要自己算左上角偏移）；省略时 x 是左上角。
- draw_polygon(points, style?)：画封闭多边形（≥3 顶点），水墨的山、岸首选。
- draw_mindmap(root, cx?, cy?)：一次调用画出整张思维导图。root 是嵌套树：{"text":"中心主题","children":[{"text":"一级分支","children":[{"text":"要点"}]}]}。布局（节点位置、曲线连线、分支配色、防重叠防交叉）全部自动计算——只给文字，不要自己用矩形+连线拼导图。节点文字 ≤ 20 字单行关键词，全图 ≤ 40 节点、≤ 5 层。
- set_canvas_background(preset?, color?)：设置画布底色。preset: greenboard（墨绿粉笔板）/ blackboard（黑板黑）/ white（白板）。
- update_element(id, x?, y?, text?, style?, font_size?)：修改已有元素——移动（x/y）、改文字（text）、改样式（style，只改提供的字段）或改字号（font_size，仅文本）。画错了优先用它修正，不必删除重画。
- delete_element(id)：删除一个元素（及其标签）。
- clear_canvas()：清空画布上的所有元素。用户要求「重新开始」「全部重画」时使用。
- list_elements()：列出画布所有元素的 id、类型、文字和位置，用于查询现状和完成前自检。
- 所有 draw_* 与 update_element 均可省略 style，沿用画板当前样式；style 可选字段：stroke（描边 0xRRGGBB）、fill（填充）、fill_style（hachure 手绘排线 / solid 整块填充）、stroke_width（线宽）、roughness（粗糙度 0~2）、opacity（透明度 0~1）。

## 画布坐标系与尺寸
- 原点在左上角，x 向右增大，y 向下增大。可见范围约 x∈[0,1600]、y∈[0,1000]。
- 典型矩形宽 140~180、高 60~80。元素间距 30~40，不要重叠。
- 同一层级/同一类节点尺寸保持一致，节点中心对齐或左对齐，避免忽大忽小、参差不齐。
- 颜色用 0xRRGGBB 整数，如 0x1e1e1e（黑色）、0xa5d8ff（浅蓝）。省略则用默认黑色描边。

## 布局规则（避免连线交叉）
- 主流程从上到下排列，分支向左右两侧展开，不要让连线跨越其他形状。
- 判断节点（菱形）的两个分支：一个向下（主路径），一个向左或向右（支路径），不要两个分支都向下交叉。
- 回环线（如"失败后回到输入"）走外侧绕行，不要穿过中间的形状。用多个折线点让线绕开障碍。
- 箭头端点对齐到形状的边缘中点（上/下/左/右中点），不要连到形状内部。
- 先规划好所有形状的位置，再画连线——这样你知道每条线的起点和终点在哪里，能避免交叉。

## 完成前自检
画完后调用一次 list_elements 核对：节点是否齐全、是否有重叠、连线是否交叉、文字是否溢出。发现问题用 update_element 修正后再结束。

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
#[derive(Debug)]
pub enum AgentEvent {
    /// A chunk of assistant text (streamed).
    Delta(String),
    /// A chunk of reasoning/thinking text (streamed). Shown in a collapsible
    /// panel that is expanded while streaming and auto-collapses on Done.
    Reasoning(String),
    /// A canvas op produced by a tool call — apply it to the board and reply
    /// through `reply` with the authoritative outcome. `pre_assigned_id` is the
    /// UUID the tool generated for this element (so the tool can report it back
    /// to the model); `apply_canvas_op` uses it as the element's id on create
    /// ops. None for non-create ops (update/delete).
    CanvasOp {
        op: CanvasOp,
        pre_assigned_id: Option<uuid::Uuid>,
        reply: futures::channel::oneshot::Sender<CanvasOpOutcome>,
    },
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
    /// return text (e.g. "已添加到画布"), and `is_error` marks a failure (e.g.
    /// a validation rejection or a missing element id).
    ToolResult {
        id: String,
        result: String,
        is_error: bool,
    },
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
        snapshot: Arc<Mutex<Vec<super::tools::ElementSnapshot>>>,
        active_skill: super::skills::ActiveSkill,
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
            // The scene-agnostic core plus the skill catalog and routing
            // rules (per-scene specs are loaded by the model via use_skill).
            .preamble(super::skills::system_prompt().as_str())
            // Each tool call is now a separate turn (the system prompt encourages
            // step-by-step drawing), so a complex diagram needs many rounds.
            .default_max_turns(100)
            // Reasoning models can burn through a provider's default token
            // limit during the thinking phase, truncating the response before
            // any tool call is emitted - the agent then ends having only
            // narrated ("马上开画") without drawing. A generous ceiling lets
            // the reasoning finish and the tool call land.
            .max_tokens(16000)
            // Reasoning effort (low/medium/high) flattened to the top-level
            // request body as `reasoning_effort`. Non-reasoning models ignore
            // the extra field; the `flatten` in rig's OpenAI request struct
            // places it exactly where the API expects it.
            .additional_params(serde_json::json!({
                "reasoning_effort": settings.reasoning_effort.as_str()
            }))
            .tools(all_tools(events, snapshot, active_skill))
            .build();
        Ok(agent)
    }

    /// Start a streaming prompt to the drawing agent.
    ///
    /// `history` is the prior conversation (most recent last), excluding the
    /// new user message. The new user message is `prompt`. The agent's tools
    /// emit their lifecycle events (`ToolCall`/`CanvasOp`/`ToolResult`) directly
    /// through the returned event stream, so the caller drains one channel.
    /// `active_skill` is the cross-turn handle the `use_skill` tool writes to;
    /// the caller (panel) reads it back to keep the loaded spec in scope.
    pub fn stream(
        settings: &AiSettings,
        prompt: String,
        history: Vec<ChatMessage>,
        snapshot: Arc<Mutex<Vec<super::tools::ElementSnapshot>>>,
        runtime_context: String,
        active_skill: super::skills::ActiveSkill,
    ) -> anyhow::Result<AgentRequest> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let agent = Self::build(settings, tx.clone(), snapshot, active_skill)?;
        let chat_history: Vec<RigMessage> = history.into_iter().filter_map(msg_to_rig).collect();

        // Prepend the fresh canvas snapshot as a user-role runtime context so
        // the model sees current state without a list_elements round-trip. It
        // is rebuilt every turn, so an earlier snapshot is superseded rather
        // than accumulating in the stored history.
        let prompt = with_runtime_context(prompt, &runtime_context);

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
                        if !handle_agent_item(stream_item, &tx, &mut full_text, &mut tool_called) {
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

        Ok(AgentRequest { events: rx, cancel })
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
    item: MultiTurnStreamItem<<Model as rig_core::completion::CompletionModel>::StreamingResponse>,
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

/// Header for the per-turn runtime-context snapshot, mirroring the harness
/// convention that each snapshot supersedes earlier ones (so stale state never
/// accumulates across turns).
const RUNTIME_CONTEXT_HEADER: &str = "当前画布运行时上下文。此快照取代之前的所有运行时上下文快照。";

/// Prepend the fresh runtime context to the user's message. Empty context is a
/// no-op so a failure to snapshot the board never blocks the request.
fn with_runtime_context(prompt: String, runtime_context: &str) -> String {
    let ctx = runtime_context.trim();
    if ctx.is_empty() {
        prompt
    } else {
        format!("{RUNTIME_CONTEXT_HEADER}\n\n{ctx}\n\n{prompt}")
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

    #[test]
    fn runtime_context_prepends_once_and_omits_when_empty() {
        // Empty context is a no-op: the user's message passes through unchanged.
        assert_eq!(with_runtime_context("画个流程图".into(), ""), "画个流程图");
        assert_eq!(
            with_runtime_context("画个流程图".into(), "   \n"),
            "画个流程图"
        );

        // Non-empty context is prepended once, header first, prompt last.
        let out = with_runtime_context("画个流程图".into(), "画布为空，尚无任何元素。");
        assert!(out.starts_with(RUNTIME_CONTEXT_HEADER));
        assert!(out.contains("画布为空，尚无任何元素。"));
        assert!(out.ends_with("画个流程图"));
        // Header appears exactly once (the context is never duplicated).
        assert_eq!(out.matches(RUNTIME_CONTEXT_HEADER).count(), 1);
    }
}
