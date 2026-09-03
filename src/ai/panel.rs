//! AI side panel: settings, chat with SSE streaming, insert-to-canvas.
//!
//! Input fields use gpui-component's `InputState`/`Input` (multi-line with
//! auto-grow, IME, selection, clipboard) instead of a hand-rolled TextField.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use gpui::prelude::*;
use gpui::*;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::text::TextView;
use gpui_component::IconName;
use gpui_component::Sizable;

use crate::board::BoardView;

use super::agent::{AgentEvent, AgentRequest, BoundlessAgent};
use super::client::ChatMessage;
use super::settings::{AiSettings, ReasoningLevel};
use super::store::{
    self, create_session, delete_session, list_sessions, load_messages, SessionMeta,
};
use super::tools::ElementSnapshot;

// ---------------------------------------------------------------------
// AiPanel
// ---------------------------------------------------------------------

struct StreamingState {
    buffer: String,
    cancel: Arc<AtomicBool>,
    /// Ordered reasoning/tool steps as the agent works, in execution order.
    /// Reasoning deltas append to the last step iff it's already a Reasoning
    /// step; a tool call always starts a fresh step. This keeps the
    /// "think → tool → think → tool" loop visible.
    steps: Vec<super::client::AssistantStep>,
    /// True from the first reasoning delta until the model moves on to text
    /// output, a tool call, or Done. The live reasoning step stays expanded
    /// while this is true — it must NOT flip off on mere streaming pauses
    /// (network chunking), or the panel flickers collapse/expand.
    reasoning_active: bool,
    _task: Task<()>,
}

pub struct AiPanel {
    board: WeakEntity<BoardView>,
    settings: AiSettings,
    show_settings: bool,
    /// Whether the session-list section is shown.
    show_sessions: bool,
    /// All known sessions (newest first), for the list UI.
    sessions: Vec<SessionMeta>,
    /// The currently active session's id.
    session_id: String,
    messages: Vec<ChatMessage>,
    streaming: Option<StreamingState>,
    error: Option<String>,
    /// Multi-line chat input (auto-grow 3..8 lines).
    input: Entity<InputState>,
    /// Settings fields.
    base_url_input: Entity<InputState>,
    api_key_input: Entity<InputState>,
    model_input: Entity<InputState>,
    notice: Option<String>,
    /// Current reasoning-effort selection, mirrored to settings on send.
    reasoning: ReasoningLevel,
    /// When true, the chat input is cleared at the next render (InputState's
    /// set_value needs a Window, which is only available in render / event
    /// handlers that receive one).
    pending_clear_input: bool,
    /// 本条用户消息是否已做过「只说不画」的自动重试（每条消息最多一次）。
    narration_retried: bool,
    /// Indices of steps the user has manually expanded/collapsed in the live
    /// streaming bubble. A step present here is force-toggled from its default
    /// (reasoning steps default open while streaming, closed after; tool steps
    /// default closed). Absent = use the default.
    open_stream_steps: std::collections::HashSet<usize>,
    /// Keys of (message index, step index) the user has toggled open in a
    /// completed message. All steps default to collapsed once streaming ends.
    open_done_steps: std::collections::HashSet<(usize, usize)>,
    /// Scroll handle for the messages area, used to auto-scroll to bottom.
    messages_scroll: ScrollHandle,
    /// Scroll handle for the live (actively streaming) reasoning body. Pinned
    /// to the body's bottom on every reasoning flush, and shared with the
    /// body's wheel handler for nested-scroll containment.
    live_reasoning_scroll: ScrollHandle,
    /// Current panel width in px. User-resizable via the left edge handle.
    width: f32,
    /// The currently active scenario skill, written by the `use_skill` tool
    /// when the model loads a spec. Its body is prepended to the next turn's
    /// runtime context so the spec stays in scope (chat history carries no
    /// tool results). Cleared on new session.
    active_skill: super::skills::ActiveSkill,
    _subscriptions: Vec<Subscription>,
}

/// Default / min / max panel width in px.
pub const DEFAULT_WIDTH: f32 = 360.0;
const MIN_WIDTH: f32 = 280.0;
const MAX_WIDTH: f32 = 640.0;

/// Maximum height of a step body (streaming reasoning, or an expanded
/// reasoning/tool detail). Longer content scrolls inside the body instead of
/// filling the whole messages area.
const STEP_BODY_MAX_H: f32 = 320.0;

/// Marker value carried by the resize drag. The width is computed statelessly
/// from the live pointer position in `on_drag_move` (panel is right-docked), so
/// this needs no fields — it exists only so GPUI's drag API has a value/preview
/// type to track. Implements `Render` because the drag API requires a preview
/// view; its render is an invisible zero-size element.
#[derive(Clone, Copy, Default)]
struct ResizeDrag;

impl Render for ResizeDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Invisible drag preview: resizing shouldn't show a floating chip.
        div().size_0()
    }
}

impl AiPanel {
    pub fn new(board: WeakEntity<BoardView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = AiSettings::load();

        // Chat input: multi-line, auto-grow 3..8 lines.
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("给 AI 发送消息…")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        // Settings inputs: single-line.
        let base_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://api.openai.com/v1")
                .default_value(settings.base_url.clone())
        });
        let api_key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-...")
                .masked(true)
                .default_value(settings.api_key.clone())
        });
        let model_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("gpt-4o-mini")
                .default_value(settings.model.clone())
        });

        let mut subscriptions = Vec::new();
        // Enter (without Shift) in the chat input sends the message.
        subscriptions.push(
            cx.subscribe(&input, |this, _entity, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { secondary } = event {
                    if !secondary {
                        this.send_message(cx);
                    }
                }
            }),
        );

        // Load stored sessions; resume the most recent one if any, else start a
        // fresh (not-yet-persisted) session.
        let sessions = list_sessions();
        let (session_id, messages) = match sessions.first() {
            Some(latest) => {
                let id = latest.id.clone();
                let msgs = load_messages(&id).unwrap_or_default();
                (id, msgs)
            }
            None => (create_session(), Vec::new()),
        };

        Self {
            board,
            reasoning: settings.reasoning_effort,
            settings,
            show_settings: false,
            show_sessions: false,
            sessions,
            session_id,
            messages,
            streaming: None,
            error: None,
            input,
            base_url_input,
            api_key_input,
            model_input,
            notice: None,
            pending_clear_input: false,
            narration_retried: false,
            open_stream_steps: HashSet::new(),
            open_done_steps: HashSet::new(),
            messages_scroll: ScrollHandle::new(),
            live_reasoning_scroll: ScrollHandle::new(),
            width: DEFAULT_WIDTH,
            active_skill: super::skills::ActiveSkill::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Current panel width in px (for the board to offset its chrome).
    pub fn width(&self) -> f32 {
        self.width
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let base_url = self.base_url_input.read(cx).value().to_string();
        let api_key = self.api_key_input.read(cx).value().to_string();
        let model = self.model_input.read(cx).value().to_string();
        self.settings = AiSettings {
            base_url: if base_url.trim().is_empty() {
                AiSettings::default().base_url
            } else {
                base_url.trim().to_string()
            },
            api_key: api_key.trim().to_string(),
            model: if model.trim().is_empty() {
                AiSettings::default().model
            } else {
                model.trim().to_string()
            },
            // Preserve the current reasoning-effort choice (set via the bottom
            // toolbar), so saving settings doesn't reset it.
            reasoning_effort: self.settings.reasoning_effort,
        };
        match self.settings.save() {
            Ok(()) => {
                self.notice = Some("设置已保存".to_string());
                self.show_settings = false;
            }
            Err(e) => self.notice = Some(format!("保存失败: {e}")),
        }
        cx.notify();
    }

    /// Set the reasoning-effort level, persist it to settings, and refresh.
    fn set_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        self.reasoning = level;
        self.settings.reasoning_effort = level;
        if let Err(e) = self.settings.save() {
            self.notice = Some(format!("保存设置失败: {e}"));
        }
        cx.notify();
    }

    fn send_message(&mut self, cx: &mut Context<Self>) {
        if self.streaming.is_some() {
            return;
        }
        let text = self.input.read(cx).value().to_string();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Keep settings' reasoning level in sync with the toolbar selection.
        self.settings.reasoning_effort = self.reasoning;
        // Defer clearing the input: InputState::set_value needs a Window,
        // which subscribe callbacks don't have. The flag is consumed in render.
        self.pending_clear_input = true;
        self.narration_retried = false;
        self.error = None;
        let msg = ChatMessage::user(text);
        self.persist(&msg);
        self.messages.push(msg);
        self.start_agent(cx);
    }

    /// Append a message to the current session's JSONL file. Failures are
    /// surfaced as a notice rather than aborting the conversation - an
    /// in-memory-only session is still usable.
    fn persist(&mut self, msg: &ChatMessage) {
        if let Err(e) = store::append_message(&self.session_id, msg) {
            self.notice = Some(format!("会话保存失败: {e}"));
        }
    }

    /// Refresh the cached session list from disk (preview/mtime may change as
    /// messages are appended).
    fn refresh_sessions(&mut self) {
        self.sessions = list_sessions();
    }

    /// Start a brand-new, empty session and switch to it.
    fn new_session(&mut self, cx: &mut Context<Self>) {
        self.stop_streaming(cx);
        self.streaming = None;
        self.error = None;
        self.session_id = create_session();
        self.messages.clear();
        self.active_skill.clear();
        self.show_sessions = false;
        cx.notify();
    }

    /// Switch to an existing session, loading its messages.
    fn switch_session(&mut self, id: String, cx: &mut Context<Self>) {
        if id == self.session_id {
            self.show_sessions = false;
            cx.notify();
            return;
        }
        self.stop_streaming(cx);
        self.streaming = None;
        self.error = None;
        self.active_skill.clear();
        match load_messages(&id) {
            Ok(msgs) => {
                self.session_id = id;
                self.messages = msgs;
                self.show_sessions = false;
            }
            Err(e) => {
                self.notice = Some(format!("加载会话失败: {e}"));
            }
        }
        cx.notify();
    }

    /// Delete a session. If it's the current one, start a fresh session.
    fn remove_session(&mut self, id: String, cx: &mut Context<Self>) {
        if let Err(e) = delete_session(&id) {
            self.notice = Some(format!("删除会话失败: {e}"));
            cx.notify();
            return;
        }
        if id == self.session_id {
            self.new_session(cx);
        }
        self.refresh_sessions();
        cx.notify();
    }

    /// Kick off the drawing agent with the current prompt + recent history.
    /// The agent creates a per-request canvas-op channel (its tools' sender);
    /// we drain both the agent event stream and the canvas-op receiver in the
    /// spawned task, applying ops to the board on the main thread.
    fn start_agent(&mut self, cx: &mut Context<Self>) {
        let prompt = match self.messages.last() {
            Some(ChatMessage { role, content, .. }) if role == "user" => content.clone(),
            _ => return,
        };
        // Recent history excluding the just-added prompt (the agent takes the
        // new prompt separately). Keep it bounded.
        let history: Vec<ChatMessage> = self
            .messages
            .iter()
            .rev()
            .skip(1)
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        // Build the per-turn canvas state in one board access: the element list
        // feeds the `list_elements` tool, and the runtime-context string is
        // prepended to the prompt so the agent sees current state upfront.
        let (snapshot, runtime_context) = self
            .board
            .update(cx, |board, _cx| {
                let snapshot = board.element_snapshot();
                let context = board.runtime_context();
                (snapshot, context)
            })
            .unwrap_or_default();
        // Share the snapshot so tools see live state: the main thread refreshes
        // it after every applied op (list_elements + update/delete id checks).
        let snapshot: Arc<Mutex<Vec<ElementSnapshot>>> = Arc::new(Mutex::new(snapshot));
        // Structured run log: every tool call/result/end of this request
        // lands in ~/.boundless/agent-logs/run-<ts>.jsonl for later analysis.
        super::log::begin_run(&prompt, &self.settings.model);

        // Prepend the active skill's spec to the runtime context: a skill
        // loaded via use_skill in an earlier turn is not in the chat history
        // (tool results aren't stored), so the spec rides along with the
        // fresh snapshot each turn.
        let mut runtime_context = runtime_context;
        if let Some(name) = self.active_skill.get() {
            match super::skills::find(&name) {
                Some(skill) => {
                    runtime_context = format!(
                        "当前活动技能规范（{}）：\n\n{}\n\n{runtime_context}",
                        skill.name, skill.body
                    );
                }
                None => self.active_skill.clear(),
            }
        }

        match BoundlessAgent::stream(
            &self.settings,
            prompt,
            history,
            snapshot.clone(),
            runtime_context,
            self.active_skill.clone(),
        ) {
            Ok(request) => self.start_stream_task(request, snapshot, cx),
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
            }
        }
        cx.notify();
    }

    fn start_stream_task(
        &mut self,
        request: AgentRequest,
        snapshot: Arc<Mutex<Vec<ElementSnapshot>>>,
        cx: &mut Context<Self>,
    ) {
        // Reset the user's step toggles so each new stream starts from the
        // defaults (reasoning open while streaming, tools collapsed).
        self.open_stream_steps.clear();
        // Fresh scroll handle for the new stream's live reasoning body.
        self.live_reasoning_scroll = ScrollHandle::new();
        let mut events = request.events;
        let cancel = request.cancel;
        let board = self.board.clone();
        let task = cx.spawn(async move |this, cx| {
            // High-frequency deltas (text/reasoning) arrive one-per-token.
            // Instead of calling `this.update()` on every token (which contends
            // for the main-thread lock and can block behind an in-progress
            // render), we buffer them and flush to the panel state on a ~50ms
            // timer — one `this.update()` + `cx.notify()` per flush, not per
            // token. Low-frequency events (tool calls, canvas ops, done, error)
            // are handled immediately. GPUI coalesces same-frame `cx.notify()`
            // calls, and TextView's keyed-state cache debounces the markdown
            // parse to 200ms on a background thread, so the flush rate only
            // controls how often we re-shape the growing text.
            let mut pending_reasoning = String::new();
            let mut pending_delta = String::new();
            let flush_duration = std::time::Duration::from_millis(50);
            loop {
                // Race the next agent event against the flush timer — but only
                // arm the timer when deltas are actually pending. (An always-on
                // timer that acts on empty ticks makes the reasoning panel
                // collapse on every brief network pause and re-expand on the
                // next token — visible as constant flickering.)
                let next = if pending_delta.is_empty() && pending_reasoning.is_empty() {
                    futures::future::Either::Left(events.next().await)
                } else {
                    let timer = cx.background_executor().timer(flush_duration);
                    futures::future::Either::Right(
                        futures::future::select(events.next(), timer).await,
                    )
                };
                // Normalize into an optional event (None = timer fired).
                let event = match next {
                    futures::future::Either::Left(Some(e)) => Some(e),
                    futures::future::Either::Left(None) => break, // stream ended
                    futures::future::Either::Right(futures::future::Either::Left((Some(e), _))) => {
                        Some(e)
                    }
                    // The agent always emits Done/Error before the stream ends
                    // (and pending deltas were flushed before it), so a None
                    // here carries no data loss.
                    futures::future::Either::Right(futures::future::Either::Left((None, _))) => {
                        break
                    }
                    futures::future::Either::Right(futures::future::Either::Right(_)) => None,
                };
                match event {
                    Some(AgentEvent::Delta(text)) => {
                        pending_delta.push_str(&text);
                    }
                    Some(AgentEvent::Reasoning(text)) => {
                        pending_reasoning.push_str(&text);
                    }
                    Some(ev) => {
                        // Flush any pending deltas before handling a discrete
                        // event so the ordering is preserved (text → tool etc.).
                        let pd = std::mem::take(&mut pending_delta);
                        let pr = std::mem::take(&mut pending_reasoning);
                        let keep_going = this
                            .update(cx, |panel, cx| {
                                if !pr.is_empty() {
                                    Self::apply_reasoning(panel, &pr);
                                }
                                if !pd.is_empty() {
                                    Self::apply_delta(panel, &pd);
                                }
                                let go = Self::handle_event(panel, ev, &board, &snapshot, cx);
                                panel.messages_scroll.scroll_to_bottom();
                                go
                            })
                            .unwrap_or(false);
                        if !keep_going {
                            break;
                        }
                    }
                    None => {
                        // Timer fired — flush pending deltas in one update.
                        // Note: `reasoning_active` is deliberately NOT touched
                        // here. Reasoning streams pause naturally between
                        // network chunks; collapsing the panel on a pause and
                        // re-expanding on the next token is what caused the
                        // visible flickering. It flips only on real event
                        // boundaries (text delta / tool call / Done).
                        let pd = std::mem::take(&mut pending_delta);
                        let pr = std::mem::take(&mut pending_reasoning);
                        if !pd.is_empty() || !pr.is_empty() {
                            this.update(cx, |panel, cx| {
                                if !pr.is_empty() {
                                    Self::apply_reasoning(panel, &pr);
                                }
                                if !pd.is_empty() {
                                    Self::apply_delta(panel, &pd);
                                }
                                // Scroll the parent message list to follow
                                // both reasoning and text output. (The live
                                // reasoning body additionally scrolls itself
                                // via its own handle — see apply_reasoning.)
                                panel.messages_scroll.scroll_to_bottom();
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                }
            }
        });
        self.streaming = Some(StreamingState {
            buffer: String::new(),
            cancel,
            steps: Vec::new(),
            reasoning_active: false,
            _task: task,
        });
    }

    /// Append buffered text deltas to the panel's streaming state.
    fn apply_delta(panel: &mut AiPanel, text: &str) {
        if let Some(s) = panel.streaming.as_mut() {
            s.reasoning_active = false;
            s.buffer.push_str(text);
            match s.steps.last() {
                Some(super::client::AssistantStep::Text { .. }) => {
                    if let Some(super::client::AssistantStep::Text { text: t }) = s.steps.last_mut()
                    {
                        t.push_str(text);
                    }
                }
                _ => s.steps.push(super::client::AssistantStep::Text {
                    text: text.to_string(),
                }),
            }
        }
    }

    /// Append buffered reasoning deltas to the panel's streaming state.
    fn apply_reasoning(panel: &mut AiPanel, text: &str) {
        if let Some(s) = panel.streaming.as_mut() {
            s.reasoning_active = true;
            match s.steps.last() {
                Some(super::client::AssistantStep::Reasoning { .. }) => {
                    if let Some(super::client::AssistantStep::Reasoning { text: t }) =
                        s.steps.last_mut()
                    {
                        t.push_str(text);
                    }
                }
                _ => s.steps.push(super::client::AssistantStep::Reasoning {
                    text: text.to_string(),
                }),
            }
        }
        // Keep the live reasoning body pinned to its bottom as it grows. The
        // flag is applied during the next prepaint against the freshly
        // measured content size, so this is exact (no one-frame lag).
        panel.live_reasoning_scroll.scroll_to_bottom();
    }

    /// Handle a discrete (non-delta) event: tool call, canvas op, done, error.
    /// Returns false if the stream should stop.
    fn handle_event(
        panel: &mut AiPanel,
        event: AgentEvent,
        board: &WeakEntity<BoardView>,
        snapshot: &Arc<Mutex<Vec<ElementSnapshot>>>,
        cx: &mut Context<AiPanel>,
    ) -> bool {
        match event {
            AgentEvent::Delta(_) | AgentEvent::Reasoning(_) => true,
            AgentEvent::CanvasOp {
                op,
                pre_assigned_id,
                reply,
            } => {
                // Apply on the main thread, refresh the shared snapshot, and
                // relay the authoritative outcome back to the waiting tool.
                let outcome = board
                    .update(cx, |board, cx| {
                        let outcome = board.apply_canvas_op(op, pre_assigned_id, cx);
                        let fresh = board.element_snapshot();
                        let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                        *snap = fresh;
                        outcome
                    })
                    .unwrap_or_else(|_| {
                        Err(crate::ai::canvas_ops::CanvasOpError::internal(
                            "画布操作失败：视图已销毁",
                        ))
                    });
                let _ = reply.send(outcome);
                cx.notify();
                true
            }
            AgentEvent::ToolCall { id, name, args } => {
                super::log::log_tool_call(&name, &args);
                if let Some(s) = panel.streaming.as_mut() {
                    s.reasoning_active = false;
                    s.steps.push(super::client::AssistantStep::Tool {
                        name,
                        args,
                        done: false,
                        error: false,
                        id,
                        result: String::new(),
                    });
                }
                cx.notify();
                true
            }
            AgentEvent::ToolResult {
                id,
                result,
                is_error,
            } => {
                super::log::log_tool_result(is_error, &result);
                if let Some(s) = panel.streaming.as_mut() {
                    for step in s.steps.iter_mut() {
                        if let super::client::AssistantStep::Tool {
                            id: step_id,
                            done,
                            error,
                            result: step_result,
                            ..
                        } = step
                        {
                            if *step_id == id && !*done {
                                *done = true;
                                *error = is_error;
                                *step_result = result.clone();
                                break;
                            }
                        }
                    }
                }
                cx.notify();
                true
            }
            AgentEvent::Done {
                text,
                drew_anything,
            } => {
                super::log::end_run(drew_anything, &text);
                // 兜底：模型只输出文字没调用任何绘图工具（flash 级模型的
                // 偶发行为）→ 带着原考题自动重试一次，防止用户白等。
                if !drew_anything && !panel.narration_retried {
                    panel.narration_retried = true;
                    panel.streaming = None;
                    cx.notify();
                    panel.start_agent(cx);
                    return false;
                }
                panel.finish_streaming(text, drew_anything, cx);
                false
            }
            AgentEvent::Error(err) => {
                super::log::log_error(&err);
                panel.error = Some(err);
                panel.streaming = None;
                cx.notify();
                false
            }
        }
    }

    fn finish_streaming(
        &mut self,
        final_text: String,
        drew_anything: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.streaming.take() {
            // Prefer the aggregated final text; fall back to the streamed buffer
            // (some providers only emit deltas, never a final response).
            let text = if final_text.trim().is_empty() {
                s.buffer
            } else {
                final_text
            };
            if !text.trim().is_empty() {
                let mut msg = ChatMessage::assistant(text);
                // Preserve the ordered reasoning/tool steps so they survive
                // after streaming - the user can re-expand any step.
                msg.steps = s.steps;
                self.persist(&msg);
                self.messages.push(msg);
            }
            // The model narrated but never called a drawing tool - surface it
            // instead of silently ending, so the user knows nothing was drawn
            // (common with reasoning models that truncate before the tool call,
            // or when the model just plans verbally).
            if !drew_anything {
                self.error = Some("模型未调用任何绘图工具，没有内容被画到画布上。可以再试一次或把需求说得更具体。".to_string());
            }
            // Steps of the finished turn are now persisted; drop live toggles.
            self.open_done_steps.clear();
            // Refresh the list so this session's preview/mtime updates.
            self.refresh_sessions();
            cx.notify();
        }
    }

    fn stop_streaming(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = &self.streaming {
            s.cancel.store(true, Ordering::Relaxed);
        }
        // The stream loop will emit Done, which finalizes the partial reply.
        cx.notify();
    }

    fn insert_to_canvas(&mut self, content: String, cx: &mut Context<Self>) {
        self.board
            .update(cx, |board, cx| board.insert_ai_text(content, cx))
            .ok();
    }

    fn clear_chat(&mut self, cx: &mut Context<Self>) {
        // "清空" starts a fresh conversation; the previous session's history
        // is preserved on disk and remains selectable from the session list.
        self.new_session(cx);
    }
}

impl Render for AiPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Consume the deferred input-clear flag. send_message (called from a
        // subscribe callback that has no Window) sets it; here we have a Window
        // so InputState::set_value can run. Clear the flag first to avoid
        // re-entrancy.
        if self.pending_clear_input {
            self.pending_clear_input = false;
            self.input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        let streaming = self.streaming.is_some();

        // ---- header ----
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(0xeeeeec))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("AI 创作助手"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(
                        panel_button("会话", self.show_sessions).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.show_sessions = !this.show_sessions;
                                if this.show_sessions {
                                    this.show_settings = false;
                                    this.refresh_sessions();
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        panel_button("设置", self.show_settings).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.show_settings = !this.show_settings;
                                if this.show_settings {
                                    this.show_sessions = false;
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        panel_button("新建", false)
                            .on_click(cx.listener(|this, _, _, cx| this.clear_chat(cx))),
                    ),
            );

        // ---- settings section ----
        let settings_section = if self.show_settings {
            let notice = self.notice.clone().unwrap_or_default();
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(0xeeeeec))
                .child(field_row("Base URL", self.base_url_input.clone()))
                .child(field_row("API Key", self.api_key_input.clone()))
                .child(field_row("模型", self.model_input.clone()))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(div().text_xs().text_color(rgb(0x2f9e44)).child(notice))
                        .child(
                            panel_button("保存设置", false)
                                .on_click(cx.listener(|this, _, _, cx| this.save_settings(cx))),
                        ),
                )
        } else {
            div().hidden()
        };

        // ---- sessions section ----
        // Shows the conversation history list with a "new session" button.
        let sessions_section = if self.show_sessions {
            let mut col = div()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(0xeeeeec))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(div().text_xs().text_color(rgb(0x777777)).child("会话历史"))
                        .child(
                            panel_button("＋ 新建", false)
                                .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                        ),
                );
            if self.sessions.is_empty() {
                col = col.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x999999))
                        .child("还没有会话记录。"),
                );
            }
            // Render each session row. Cap the list height so long histories
            // scroll rather than pushing the input box off-screen.
            let mut list = div()
                .id("session-list")
                .flex()
                .flex_col()
                .gap_1()
                .max_h(px(320.0))
                .overflow_y_scroll();
            for (idx, s) in self.sessions.iter().enumerate() {
                let is_current = s.id == self.session_id;
                let preview = s.preview.clone();
                let count = s.count;
                let id_for_del = s.id.clone();
                let id_for_switch = s.id.clone();
                let row = div()
                    .id(("session", idx))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(is_current, |d| {
                        d.bg(rgb(0xdce8ff)).text_color(rgb(0x1a5fd7))
                    })
                    .when(!is_current, |d| d.hover(|s| s.bg(rgb(0xefeeec))))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.switch_session(id_for_switch.clone(), cx);
                        }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .child(div().truncate().child(preview))
                            .child(
                                div()
                                    .text_color(rgb(if is_current { 0x1a5fd7 } else { 0x999999 }))
                                    .child(format!("{count} 条")),
                            ),
                    )
                    .child(
                        // Delete button: stop propagation so clicking it doesn't
                        // also switch into the (about-to-be-deleted) session.
                        div()
                            .id(("session-del", idx))
                            .px_1()
                            .text_xs()
                            .text_color(rgb(0xc92a2a))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xfff0f0)).rounded_md())
                            .child("删除")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.remove_session(id_for_del.clone(), cx);
                                }),
                            ),
                    );
                list = list.child(row);
            }
            col = col.child(list);
            col
        } else {
            div().hidden()
        };

        // ---- messages ----
        let mut messages = div().flex().flex_col().gap_2().p_3().w_full();
        if self.messages.is_empty() && !streaming {
            messages =
                messages.child(div().text_sm().text_color(rgb(0x999999)).child(
                    "描述你想画的内容，例如「画一个登录流程图」，AI 会直接把它画到画布上。",
                ));
        }
        for (idx, msg) in self.messages.iter().enumerate() {
            messages = messages.child(self.render_message(idx, msg, window, cx));
        }
        if let Some(s) = &self.streaming {
            // A single agent step bubble that evolves as the stream progresses.
            // The model's work is shown as an ordered sequence of steps —
            // reasoning chunks and tool calls interleaved in the order they
            // actually happened — so the "think → tool → think → tool → answer"
            // loop reads like a person narrating their work.
            let mut step = div().flex().flex_col().gap_1().w_full().px_1().py_1();

            // While no content has arrived yet, show "🤖 思考中…" with a bot icon.
            let has_content = !s.steps.is_empty() || !s.buffer.is_empty();
            if !has_content {
                step = step.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .text_color(rgb(0x999999))
                        .child(
                            div()
                                .w(px(14.0))
                                .h(px(14.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(gpui_component::Icon::new(IconName::Bot)),
                        )
                        .child(div().child("思考中…")),
                );
            }

            // Render each step in order — reasoning, tool calls, and text are all
            // individual, independently expandable steps (no grouping). This
            // makes the "think → tool → think → tool" rhythm visible: each tool
            // call is its own full-width step like a reasoning block, not folded
            // into a compact chip row.
            for (i, item) in s.steps.iter().enumerate() {
                let is_last = i + 1 == s.steps.len();
                let default_open = match item {
                    super::client::AssistantStep::Reasoning { .. } => is_last && s.reasoning_active,
                    super::client::AssistantStep::Text { .. } => {
                        // Text steps are rendered inline by render_stream_step;
                        // pass the streaming cursor flag via default_open.
                        is_last && !s.buffer.is_empty()
                    }
                    _ => false,
                };
                step = step.child(self.render_stream_step(i, item, default_open, window, cx));
            }

            messages = messages.child(step);
        }
        let messages_area = div()
            .id("ai-messages")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.messages_scroll)
            .child(messages);

        // ---- error bar ----
        let error_bar = if let Some(err) = &self.error {
            div()
                .px_3()
                .py_2()
                .bg(rgb(0xfff0f0))
                .text_xs()
                .text_color(rgb(0xc92a2a))
                .child(err.clone())
        } else {
            div().hidden()
        };

        // ---- input area ----
        // A single bordered box containing the multi-line Input (auto-grows) and
        // a bottom toolbar row inside the box. The send button is an icon via
        // gpui-component's Button.
        let streaming_now = streaming;
        let model_name = self.settings.model.clone();
        let current_reasoning = self.reasoning;

        // Reasoning-effort segmented control: 低 / 中 / 高.
        let mut reasoning_control = div()
            .flex()
            .flex_row()
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(rgb(0xd6d4d0));
        for (i, level) in [
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ]
        .into_iter()
        .enumerate()
        {
            let active = level == current_reasoning;
            let mut seg = div()
                .id(("reasoning", i))
                .px_2()
                .py_1()
                .text_xs()
                .cursor_pointer()
                .child(level.label());
            if active {
                seg = seg.bg(rgb(0x1a5fd7)).text_color(rgb(0xffffff));
            } else {
                seg = seg.hover(|s| s.bg(rgb(0xefeeec))).text_color(rgb(0x555555));
            }
            reasoning_control =
                reasoning_control.child(seg.on_click(cx.listener(move |this, _, _, cx| {
                    this.set_reasoning(level, cx);
                })));
        }

        // Send/stop icon button (gpui-component Button).
        let send_btn = Button::new("send-btn")
            .icon(if streaming_now {
                IconName::Close
            } else {
                IconName::ArrowUp
            })
            .small()
            .on_click(cx.listener(move |this, _, _window, cx| {
                if streaming_now {
                    this.stop_streaming(cx);
                } else {
                    this.send_message(cx);
                }
            }));

        // Bottom toolbar row (inside the input box): model label | spacer |
        // reasoning | send icon. Pinned to the bottom-right inside the box.
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px(px(6.0))
            .py(px(4.0))
            .child(
                div()
                    .id("model-label")
                    .text_xs()
                    .text_color(rgb(0x999999))
                    .child(format!("模型: {model_name}"))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_settings = true;
                        this.show_sessions = false;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(reasoning_control)
            .child(send_btn);

        // The input box: a single bordered container holding the multi-line
        // Input (which fills the space) and the toolbar pinned to the bottom
        // *inside* the box.
        let input_area = div()
            .flex()
            .flex_col()
            .mx_3()
            .mb_2()
            .mt_1()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xd6d4d0))
            .rounded_md()
            .child(Input::new(&self.input).appearance(false))
            .child(toolbar);

        // --- Resizable left edge ---
        // Use GPUI's drag system rather than raw mouse-move on the panel: a
        // drag is captured globally, so the resize keeps tracking even when the
        // pointer leaves the (shrinking/growing) panel — which is what made the
        // earlier attempt stutter and drop.
        //
        // Width math is stateless: the panel is docked to the window's right
        // edge, so the desired width is simply (window right edge - pointer x).
        // No need to remember the drag's start position, which avoids drift.
        //
        // The handle is an absolute child of the (positioned) panel, anchored
        // to the left edge with top_0/bottom_0 + left_0. Kept to a slim 4px so
        // it sits just inside the border without eating panel content.
        let resize_handle = div()
            .id("ai-resize-handle")
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .w(px(4.0))
            .cursor(CursorStyle::ResizeLeftRight)
            .hover(|s| s.bg(rgb(0x1a5fd7)).opacity(0.12))
            .on_drag(ResizeDrag, |_v, _offset, _w, cx| {
                // The drag value only exists to drive on_drag_move; the width
                // is computed from the live pointer in on_drag_move.
                cx.new(|_| ResizeDrag)
            });

        // on_drag_move fires globally while the resize drag is active.
        let panel_mv = cx.weak_entity();

        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(self.width))
            .bg(rgb(0xfbfaf9))
            .border_l_1()
            .border_color(rgb(0xe3e2df))
            .shadow_lg()
            .flex()
            .flex_col()
            .on_drag_move::<ResizeDrag>(move |event, window, cx| {
                // Desired width = distance from the pointer to the window's
                // right edge (the panel is right-docked). Both values are in
                // *window/content* coordinates: mouse event positions are
                // relative to the window origin, and viewport_size() is the
                // window's drawable size in the same space. (Don't use
                // window.bounds() - that's in screen coordinates.)
                let win_right = f32::from(window.viewport_size().width);
                let pointer_x = f32::from(event.event.position.x);
                let w = (win_right - pointer_x).clamp(MIN_WIDTH, MAX_WIDTH);
                panel_mv
                    .update(cx, |this, cx| {
                        let delta = w - this.width;
                        if delta.abs() > 0.01 {
                            this.width = w;
                            // Keep the canvas's centered content centered as
                            // the panel grows/shrinks: the right-docked panel
                            // moves the visible canvas center by delta/2, so pan
                            // the board camera by -delta/2 to follow it. This
                            // matches the shift applied on open/close.
                            this.board
                                .update(cx, |board, cx| {
                                    board.camera.pan_by_screen(px(-delta / 2.0), px(0.0));
                                    cx.notify();
                                })
                                .ok();
                            cx.notify();
                        }
                    })
                    .ok();
            })
            // Pointer shielding for the canvas is handled in the board itself
            // (BoardView::over_ai_panel + early-returns in the canvas mouse/
            // scroll handlers), NOT via stop_propagation here. GPUI dispatches
            // all mouse listeners — element handlers and the window-level
            // on_mouse_event callbacks that TextView uses for text selection —
            // through one shared loop that breaks on stop_propagation, so a
            // panel-root mouse handler would abort that loop before TextView's
            // selection listeners run, making message text impossible to
            // select. Doing the shielding geometrically on the canvas side
            // avoids that entirely while still keeping the canvas from stealing
            // focus / panning / zooming under the panel.
            //
            // NOTE: there is also intentionally NO `.on_key_down`
            // stop-propagation. On Windows, GPUI generates the WM_CHAR that
            // delivers typed characters to a focused TextField ONLY if the
            // WM_KEYDOWN is left unhandled (propagate=true). A blanket
            // `on_key_down` → stop_propagation would mark the key "handled" and
            // suppress TranslateMessage, so no character would ever reach
            // `replace_text_in_range` — every input field would show a caret
            // but accept no typing. Canvas tool shortcuts are already disabled
            // while a text field has focus via the "Board && !TextInput" key
            // context (see main.rs), so this guard is unnecessary.
            .child(resize_handle)
            .child(header)
            .child(settings_section)
            .child(sessions_section)
            .child(messages_area)
            .child(error_bar)
            .child(input_area)
    }
}

impl AiPanel {
    /// Render one step of the live streaming bubble. `default_open` is true for
    /// the reasoning step that is currently being streamed into.
    fn render_stream_step(
        &self,
        idx: usize,
        item: &super::client::AssistantStep,
        default_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match item {
            super::client::AssistantStep::Reasoning { text } => {
                // Live reasoning defaults open while streaming, closed after.
                let open = self.open_stream_steps.contains(&idx) ^ default_open;
                // Collapsed = header only (no body). Expanded = text in a
                // height-capped, internally scrolling box. While actively
                // streaming, use plain_body (instant SharedString, pinned to
                // its bottom); once done, use styled_body (selectable
                // markdown, manual scroll).
                let body: Option<AnyElement> = if open {
                    if default_open {
                        Some(
                            plain_body(
                                StyledBodyKind::Reasoning,
                                text.clone(),
                                &self.live_reasoning_scroll,
                                cx.entity_id(),
                            )
                            .into_any_element(),
                        )
                    } else {
                        Some(
                            styled_body(
                                "reasoning-body",
                                idx,
                                StyledBodyKind::Reasoning,
                                text.clone(),
                                cx.entity_id(),
                                window,
                                cx,
                            )
                            .into_any_element(),
                        )
                    }
                } else {
                    // Collapsed: no body shown.
                    None
                };
                step_toggle(
                    ElementId::named_usize("reasoning-toggle", idx),
                    format!("reasoning-header-{idx}"),
                    Some(IconName::Bot),
                    "思考过程",
                    open,
                    rgb(0x888888),
                    None,
                    cx.listener(move |this, _, _, cx| {
                        if this.open_stream_steps.contains(&idx) {
                            this.open_stream_steps.remove(&idx);
                        } else {
                            this.open_stream_steps.insert(idx);
                        }
                        cx.notify();
                    }),
                    body,
                )
                .into_any_element()
            }
            super::client::AssistantStep::Tool {
                name,
                args,
                done,
                error,
                result,
                ..
            } => {
                // Each tool call is its own full-width expandable step - like a
                // reasoning panel, but with a tool label + status glyph. The
                // header's icon/verb/color identify add/modify/delete/query;
                // the status glyph distinguishes pending / done / failed.
                let open = self.open_stream_steps.contains(&idx);
                let body_text = tool_body_text(name, args, result, *error);
                let (status, status_color) = if !*done {
                    ("⏳", rgb(0x999999))
                } else if *error {
                    ("✕", rgb(0xc92a2a))
                } else {
                    ("✓", rgb(0x2f9e44))
                };
                let (title, title_color) = tool_header(name, args);
                step_toggle(
                    ElementId::named_usize("tool-toggle", idx),
                    format!("tool-header-{idx}"),
                    None,
                    title,
                    open,
                    title_color,
                    Some(div().text_color(status_color).child(status)),
                    cx.listener(move |this, _, _, cx| {
                        if this.open_stream_steps.contains(&idx) {
                            this.open_stream_steps.remove(&idx);
                        } else {
                            this.open_stream_steps.insert(idx);
                        }
                        cx.notify();
                    }),
                    open.then(|| {
                        styled_body(
                            "tool-body",
                            idx,
                            StyledBodyKind::Tool,
                            body_text,
                            cx.entity_id(),
                            window,
                            cx,
                        )
                    }),
                )
                .into_any_element()
            }
            // Text steps: inline rendered, not collapsible. `default_open` carries
            // the "is this the active streaming text step" flag — when true we
            // append the ▍ cursor and render as plain text for instant flush.
            // When not active (a completed text step), use TextView::markdown
            // for selectable, formatted rendering.
            super::client::AssistantStep::Text { text } => {
                if default_open {
                    // Active streaming: plain SharedString, instant display.
                    div().text_sm().child(format!("{text}▍")).into_any_element()
                } else {
                    // Completed: selectable markdown.
                    div()
                        .text_sm()
                        .child(
                            TextView::markdown(
                                ElementId::named_usize("ai-stream-text", idx),
                                text.clone(),
                                window,
                                cx,
                            )
                            .selectable(true),
                        )
                        .into_any_element()
                }
            }
        }
    }

    /// Render one step of a completed assistant message. All steps default to
    /// collapsed once streaming ends; the user can expand any of them.
    fn render_done_step(
        &self,
        msg_idx: usize,
        step_idx: usize,
        item: &super::client::AssistantStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = (msg_idx, step_idx);
        match item {
            super::client::AssistantStep::Reasoning { text } => {
                let open = self.open_done_steps.contains(&key);
                step_toggle(
                    ElementId::named_usize("reasoning-toggle-done", msg_idx * 100000 + step_idx),
                    format!("reasoning-header-done-{msg_idx}-{step_idx}"),
                    Some(IconName::Bot),
                    "思考过程",
                    open,
                    rgb(0x888888),
                    None,
                    cx.listener(move |this, _, _, cx| {
                        if this.open_done_steps.contains(&key) {
                            this.open_done_steps.remove(&key);
                        } else {
                            this.open_done_steps.insert(key);
                        }
                        cx.notify();
                    }),
                    open.then(|| {
                        styled_body(
                            "reasoning-body-done",
                            msg_idx * 100000 + step_idx,
                            StyledBodyKind::Reasoning,
                            text.clone(),
                            cx.entity_id(),
                            window,
                            cx,
                        )
                    }),
                )
                .into_any_element()
            }
            super::client::AssistantStep::Tool {
                name,
                args,
                done,
                error,
                result,
                ..
            } => {
                let open = self.open_done_steps.contains(&key);
                let body_text = tool_body_text(name, args, result, *error);
                let (status, status_color) = if !*done {
                    ("⏳", rgb(0x999999))
                } else if *error {
                    ("✕", rgb(0xc92a2a))
                } else {
                    ("✓", rgb(0x2f9e44))
                };
                let (title, title_color) = tool_header(name, args);
                step_toggle(
                    ElementId::named_usize("tool-toggle-done", msg_idx * 100000 + step_idx),
                    format!("tool-header-done-{msg_idx}-{step_idx}"),
                    None,
                    title,
                    open,
                    title_color,
                    Some(div().text_color(status_color).child(status)),
                    cx.listener(move |this, _, _, cx| {
                        if this.open_done_steps.contains(&key) {
                            this.open_done_steps.remove(&key);
                        } else {
                            this.open_done_steps.insert(key);
                        }
                        cx.notify();
                    }),
                    open.then(|| {
                        styled_body(
                            "tool-body-done",
                            msg_idx * 100000 + step_idx,
                            StyledBodyKind::Tool,
                            body_text,
                            cx.entity_id(),
                            window,
                            cx,
                        )
                    }),
                )
                .into_any_element()
            }
            // Text steps: inline rendered, selectable, not collapsible.
            super::client::AssistantStep::Text { text } => div()
                .text_sm()
                .child(
                    TextView::markdown(
                        ElementId::named_usize("ai-msg-text", msg_idx * 100000 + step_idx),
                        text.clone(),
                        window,
                        cx,
                    )
                    .selectable(true),
                )
                .into_any_element(),
        }
    }

    fn render_message(
        &self,
        idx: usize,
        msg: &ChatMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_user = msg.role == "user";
        let content = msg.content.clone();
        let steps = msg.normalized_steps();
        let insert_button = if !is_user {
            let content = content.clone();
            Some(
                panel_button("插入画布", false).on_click(cx.listener(move |this, _, _, cx| {
                    this.insert_to_canvas(content.clone(), cx);
                })),
            )
        } else {
            None
        };

        // For assistant messages with reasoning/tool steps, render them inside
        // a step bubble (same style as the streaming step) so the thinking
        // process and tools survive after streaming completes, in order.
        if !is_user && !steps.is_empty() {
            let mut step = div().flex().flex_col().gap_1().w_full().px_1().py_1();

            // Render each step in order — no grouping. Each tool call is its
            // own full-width expandable step, matching the streaming bubble.
            for (i, item) in steps.iter().enumerate() {
                step = step.child(self.render_done_step(idx, i, item, window, cx));
            }

            // Text response: render content as a final text block iff the steps
            // didn't already carry inline Text steps (legacy messages, or turns
            // where the model only replied with text and no reasoning/tools).
            let has_text_step = steps
                .iter()
                .any(|s| matches!(s, super::client::AssistantStep::Text { .. }));
            if !has_text_step {
                // NB: a distinct id namespace from the "ai-msg-text" used by
                // Text steps above — TextView keys its parsed-content state by
                // ElementId, and msg_idx*100000+step_idx can numerically
                // collide with a plain message index.
                step = step.child(
                    div().text_sm().child(
                        TextView::markdown(("ai-msg-content", idx), content, window, cx)
                            .selectable(true),
                    ),
                );
            }

            let mut col = div().flex().flex_col().gap_1().items_start();
            col = col.child(step);
            if let Some(extra) = insert_button {
                col = col.child(extra);
            }
            return col.into_any_element();
        }

        message_bubble(idx, &msg.role, content, insert_button, window, cx).into_any_element()
    }
}

fn message_bubble(
    idx: usize,
    role: &str,
    content: String,
    extra: Option<Stateful<Div>>,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    let is_user = role == "user";
    let id = if is_user {
        ElementId::named_usize("ai-msg-user", idx)
    } else {
        ElementId::named_usize("ai-msg-plain", idx)
    };
    // User bubbles size to their text width (max ~85% of panel) and align right;
    // AI bubbles take full width so wrapped text aligns left cleanly.
    let bubble = if is_user {
        div()
            .px_3()
            .py_2()
            .rounded_lg()
            .text_sm()
            .max_w(px(360.0))
            .min_w_0()
            .child(TextView::markdown(id, content, window, cx).selectable(true))
            .bg(rgb(0xdce8ff))
    } else {
        // AI messages: plain text, no card background/border — content flows
        // naturally (reasoning → tools → text) without a visual container.
        div()
            .px_1()
            .py_1()
            .text_sm()
            .w_full()
            .min_w_0()
            .child(TextView::markdown(id, content, window, cx).selectable(true))
    };
    let mut col = div().flex().flex_col().gap_1();
    // User messages: bubble aligns right (items_end prevents the default
    // stretch that would force the bubble to full width). AI messages: bubble
    // takes full width (items_start + w_full on the bubble).
    if is_user {
        col = col.items_end();
    } else {
        col = col.items_start();
    }
    col = col.child(bubble);
    if let Some(extra) = extra {
        col = col.child(extra);
    }
    col
}

fn field_row(label: &'static str, field: Entity<InputState>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(0x777777)).child(label))
        .child(Input::new(&field))
}

fn panel_button(label: &'static str, active: bool) -> Stateful<Div> {
    let mut b = div()
        .id(label)
        .flex()
        .items_center()
        .justify_center()
        .h_7()
        .px_2()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .child(label);
    if active {
        b = b.bg(rgb(0xdce8ff)).text_color(rgb(0x1a5fd7));
    } else {
        b = b.hover(|s| s.bg(rgb(0xefeeec)));
    }
    b
}

/// Map an internal rig tool name to a short Chinese label for the live status.
fn tool_label(name: &str) -> &'static str {
    match name {
        "draw_rectangle" => "矩形",
        "draw_ellipse" => "椭圆",
        "draw_diamond" => "菱形",
        "draw_line" => "直线",
        "draw_arrow" => "箭头",
        "draw_text" => "文本",
        "draw_polygon" => "多边形",
        "draw_mindmap" => "思维导图",
        "set_canvas_background" => "底色",
        "use_skill" => "技能",
        "add_page" => "页面",
        _ => "图形",
    }
}

/// The kind of canvas operation a tool performs - drives the step header's
/// icon, verb and color so add/modify/delete/query are distinguishable at a
/// glance instead of all reading "✏️ 图形".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolOp {
    Add,
    Update,
    Delete,
    Clear,
    Query,
    /// Canvas-level configuration (e.g. set_canvas_background).
    Config,
    /// Scenario-skill spec loading (use_skill).
    Skill,
    Other,
}

fn tool_op(name: &str) -> ToolOp {
    match name {
        "draw_rectangle" | "draw_ellipse" | "draw_diamond" | "draw_line" | "draw_arrow"
        | "draw_text" | "draw_polygon" | "draw_mindmap" => ToolOp::Add,
        "update_element" => ToolOp::Update,
        "delete_element" => ToolOp::Delete,
        "clear_canvas" => ToolOp::Clear,
        "list_elements" => ToolOp::Query,
        "set_canvas_background" => ToolOp::Config,
        "use_skill" => ToolOp::Skill,
        "add_page" => ToolOp::Config,
        _ => ToolOp::Other,
    }
}

impl ToolOp {
    fn icon(&self) -> &'static str {
        match self {
            ToolOp::Add => "➕",
            ToolOp::Update => "✎",
            ToolOp::Delete => "🗑",
            ToolOp::Clear => "🧹",
            ToolOp::Query => "📋",
            ToolOp::Config => "🎨",
            ToolOp::Skill => "📖",
            ToolOp::Other => "🔧",
        }
    }
    fn verb(&self) -> &'static str {
        match self {
            ToolOp::Add => "新增",
            ToolOp::Update => "修改",
            ToolOp::Delete => "删除",
            ToolOp::Clear => "清空",
            ToolOp::Query => "查询",
            ToolOp::Config => "设置",
            ToolOp::Skill => "加载",
            ToolOp::Other => "操作",
        }
    }
    fn color(&self) -> Rgba {
        match self {
            ToolOp::Add => rgb(0x2f9e44),    // green
            ToolOp::Update => rgb(0x1a5fd7), // blue
            ToolOp::Delete => rgb(0xc92a2a), // red
            ToolOp::Clear => rgb(0xc92a2a),  // red
            ToolOp::Query => rgb(0x888888),  // gray
            ToolOp::Config => rgb(0x1a5fd7),
            ToolOp::Skill => rgb(0x7048e8),  // violet: scenario-skill loading
            ToolOp::Other => rgb(0x1a5fd7),
        }
    }
}

/// Build the step-header title and its color for a tool call. The icon + verb
/// identify the operation type; the trailing detail identifies the target
/// (shape + position for adds, element id for update/delete).
fn tool_header(name: &str, args: &serde_json::Value) -> (String, Rgba) {
    let op = tool_op(name);
    let title = match op {
        ToolOp::Add => {
            let label = tool_label(name);
            let preview = tool_chip_preview(name, args);
            if preview.is_empty() {
                format!("{} {}{}", op.icon(), op.verb(), label)
            } else {
                format!("{} {}{} {}", op.icon(), op.verb(), label, preview)
            }
        }
        ToolOp::Update => {
            let id = short_id(args);
            let change = update_change_preview(args);
            if change.is_empty() {
                format!("{} {} #{}", op.icon(), op.verb(), id)
            } else {
                format!("{} {} #{} {}", op.icon(), op.verb(), id, change)
            }
        }
        ToolOp::Delete => format!("{} {} #{}", op.icon(), op.verb(), short_id(args)),
        ToolOp::Clear => format!("{} {}画布", op.icon(), op.verb()),
        ToolOp::Query => format!("{} {}元素", op.icon(), op.verb()),
        ToolOp::Skill => {
            let skill = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{} 加载技能 {skill}", op.icon())
        }
        ToolOp::Config | ToolOp::Other => {
            format!("{} {}{}", op.icon(), op.verb(), tool_label(name))
        }
    };
    (title, op.color())
}

/// Short (8-char) id prefix from a tool's args, if present (draw tools return
/// the full id; update/delete carry it in `id`).
fn short_id(args: &serde_json::Value) -> String {
    args.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "?".to_string())
}

/// What an `update_element` call changes, for the chip: new position and/or
/// new text. Omitted fields come through as null/absent and are skipped.
fn update_change_preview(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let x = obj.get("x").and_then(|v| v.as_f64());
    let y = obj.get("y").and_then(|v| v.as_f64());
    let text = obj.get("text").and_then(|v| v.as_str());
    let font_size = obj.get("font_size").and_then(|v| v.as_f64());
    let mut parts = Vec::new();
    if let (Some(x), Some(y)) = (x, y) {
        parts.push(format!("位置 ({x:.0},{y:.0})"));
    }
    if let Some(t) = text {
        let one_line: String = t.chars().filter(|c| *c != '\n').take(10).collect();
        if t.chars().count() > 10 {
            parts.push(format!("文字「{one_line}…」"));
        } else {
            parts.push(format!("文字「{one_line}」"));
        }
    }
    if let Some(fs) = font_size {
        parts.push(format!("字号 {fs:.0}"));
    }
    if let Some(style) = obj.get("style").and_then(|v| v.as_object()) {
        let stroke = color_hex(style.get("stroke")).map(|c| format!("描边 {c}"));
        let fill = color_hex(style.get("fill")).map(|c| format!("填充 {c}"));
        parts.extend([stroke, fill].into_iter().flatten());
    }
    parts.join(" ")
}

/// Which flavor of expanded body a step shows — reasoning text, or a tool-call
/// description. Both get the same left-border treatment; this just selects the
/// label color.
enum StyledBodyKind {
    Reasoning,
    Tool,
}

/// Wheel handler giving a nested scroll body proper containment: scroll the
/// inner box while it can move, then stop the event so the outer messages
/// area doesn't scroll too. (GPUI's built-in scroll listener never stops
/// propagation, so without this both containers scroll at once.) When the
/// inner box is at its limit — or has no overflow — the event falls through
/// and the outer area scrolls as usual.
///
/// Custom `on_scroll_wheel` listeners are registered after the element's
/// built-in scroll listener and therefore run before it in the bubble phase,
/// so this applies the scroll itself and then stops the event. Mirrors
/// gpui-component's `InputState::on_scroll_wheel`.
///
/// `notify` is the panel's EntityId, captured at render time. Do NOT use
/// `window.current_view()` here instead: it unwraps the render-stack, which
/// is empty when a wheel event is dispatched outside a paint (e.g. during
/// window creation) — a hard panic, and an abort across the Windows FFI
/// boundary.
fn contained_scroll(
    handle: &ScrollHandle,
    notify: EntityId,
    event: &ScrollWheelEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let delta = event.delta.pixel_delta(window.line_height());
    // Match the built-in listener: with only vertical scrolling enabled, a
    // horizontal wheel delta scrolls vertically.
    let dy = if delta.y != px(0.0) { delta.y } else { delta.x };
    let old = handle.offset();
    let max = handle.max_offset();
    let new_y = (old.y + dy).clamp(-max.height, px(0.0));
    if new_y != old.y {
        handle.set_offset(point(old.x, new_y));
        cx.stop_propagation();
        cx.notify(notify);
    }
}

/// The expandable body under a step header: left vertical border + selectable
/// markdown text, for steps whose content is final (won't grow). The markdown
/// parse is debounced 200ms in a background thread by TextView, so it's smooth
/// even for large text. Capped at [`STEP_BODY_MAX_H`]; longer content scrolls
/// inside the body.
///
/// Structural constraints baked in here (each was a past bug):
///
/// - `base_id` + `index` must be unique per step. Both the scroll state and
///   TextView's parsed-content cache are keyed by ElementId — sharing ids
///   across bodies makes every expanded body render the same (last-parsed)
///   text.
/// - The TextView sits inside a natural-height wrapper div. TextView's root
///   is `size_full()`; resolved directly against this max-height container it
///   would take the *container's* height, clipping the content with zero
///   scroll range.
/// - The wheel handler provides nested-scroll containment (see
///   [`contained_scroll`]); without it wheeling over the body also scrolls
///   the outer messages area.
fn styled_body(
    base_id: &'static str,
    index: usize,
    kind: StyledBodyKind,
    text: String,
    notify: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let color = match kind {
        StyledBodyKind::Reasoning => rgb(0x666666),
        StyledBodyKind::Tool => rgb(0x444444),
    };
    // Persisted per-body scroll handle: the wheel handler scrolls the box
    // manually, and the offset survives re-renders.
    let scroll = window
        .use_keyed_state(
            ElementId::named_usize(format!("{base_id}-handle"), index),
            cx,
            |_, _| ScrollHandle::new(),
        )
        .read(cx)
        .clone();
    let wheel = scroll.clone();
    div()
        .id(ElementId::named_usize(format!("{base_id}-scroll"), index))
        .max_h(px(STEP_BODY_MAX_H))
        .overflow_y_scroll()
        .track_scroll(&scroll)
        .on_scroll_wheel(move |event, window, cx| {
            contained_scroll(&wheel, notify, event, window, cx)
        })
        .text_xs()
        .text_color(color)
        .border_l_1()
        .border_color(rgb(0xeeeeec))
        .pl_2()
        .py_1()
        .child(
            div().child(
                TextView::markdown(
                    ElementId::named_usize(format!("{base_id}-text"), index),
                    text,
                    window,
                    cx,
                )
                .selectable(true),
            ),
        )
}

/// Same visual style as [`styled_body`] but renders the text as a raw
/// `SharedString` — no markdown parsing, no `TextView`, no 200ms debounce —
/// so each token flush of the **active** (still-streaming) reasoning step
/// shows instantly. Capped at [`STEP_BODY_MAX_H`] with its own scroll;
/// `scroll` is the panel's live-reasoning handle, pinned to the bottom on
/// every flush. Plain text has intrinsic height, so the scroll range is
/// always correct (unlike TextView's `size_full()` root, which needs the
/// wrapper div used in [`styled_body`]).
fn plain_body(
    kind: StyledBodyKind,
    text: impl Into<SharedString>,
    scroll: &ScrollHandle,
    notify: EntityId,
) -> Stateful<Div> {
    let color = match kind {
        StyledBodyKind::Reasoning => rgb(0x666666),
        StyledBodyKind::Tool => rgb(0x444444),
    };
    let wheel = scroll.clone();
    div()
        // Only one live reasoning body exists at a time (the last step while
        // reasoning is active), so this id is unique.
        .id("live-reasoning-body")
        .max_h(px(STEP_BODY_MAX_H))
        .overflow_y_scroll()
        .track_scroll(scroll)
        .on_scroll_wheel(move |event, window, cx| {
            contained_scroll(&wheel, notify, event, window, cx)
        })
        .text_xs()
        .text_color(color)
        .border_l_1()
        .border_color(rgb(0xeeeeec))
        .pl_2()
        .py_1()
        .child(text.into())
}

/// Build a clickable, hover-reveal step header with an optional body below it.
/// `icon` is an optional leading icon; `title` is the header label; `open`
/// controls the ▾/▸ arrow and whether `body` is rendered. The toggle arrow sits
/// at the end of the header and is hidden until the header is hovered.
#[allow(clippy::too_many_arguments)]
fn step_toggle<B: IntoElement>(
    id: impl Into<ElementId>,
    group: String,
    icon: Option<IconName>,
    title: impl Into<String>,
    open: bool,
    title_color: Rgba,
    trailing_child: Option<Div>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    body: Option<B>,
) -> impl IntoElement {
    let title = title.into();
    let mut header = div()
        .id(id)
        .group(group.clone())
        .flex()
        .flex_row()
        .gap_1()
        .items_center()
        .text_xs()
        .text_color(title_color)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0xf5f5f5)).rounded_md());
    if let Some(name) = icon {
        header = header.child(
            div()
                .w(px(14.0))
                .h(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .child(gpui_component::Icon::new(name)),
        );
    }
    header = header.child(div().child(title));
    if let Some(extra) = trailing_child {
        header = header.child(extra);
    }
    header = header.child(
        div()
            .text_xs()
            .text_color(rgb(0x888888))
            .opacity(0.0)
            .group_hover(group.clone(), |s| s.opacity(1.0))
            .child(if open { "▾" } else { "▸" }),
    );
    header = header.on_click(on_click);
    let mut wrapper = div().flex().flex_col().gap_1().w_full();
    wrapper = wrapper.child(header);
    if let Some(body) = body {
        wrapper = wrapper.child(body);
    }
    wrapper
}

/// Format a color JSON value (`0xRRGGBB` integer) as `#RRGGBB`, if present.
fn color_hex(v: Option<&serde_json::Value>) -> Option<String> {
    let n = v.and_then(|x| x.as_f64())? as u32;
    Some(format!("#{:06X}", n))
}

/// A very short inline label for a tool chip — what distinguishes this call at
/// a glance without expanding. For text it's the content (truncated); for
/// shapes it's the position; returns empty if nothing useful can be shown.
fn tool_chip_preview(name: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let f = |k: &str| obj.get(k).and_then(|v| v.as_f64());
    match name {
        "draw_text" => {
            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let one_line: String = text.chars().filter(|c| *c != '\n').take(10).collect();
            if text.chars().count() > 10 {
                format!("「{one_line}…」")
            } else if !one_line.is_empty() {
                format!("「{one_line}」")
            } else {
                String::new()
            }
        }
        "draw_rectangle" | "draw_ellipse" | "draw_diamond" => {
            let (x, y) = (f("x"), f("y"));
            if let (Some(x), Some(y)) = (x, y) {
                format!("({x:.0},{y:.0})")
            } else {
                String::new()
            }
        }
        "draw_line" | "draw_arrow" => {
            let n = obj
                .get("points")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if n > 0 {
                format!("{n} 点")
            } else {
                String::new()
            }
        }
        "draw_mindmap" => {
            let nodes = json_tree_nodes(obj.get("root"));
            if nodes > 0 {
                format!("{nodes} 节点")
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Count nodes in a nested mind map tree from its raw JSON form
/// (`{"text":..., "children":[...]}`).
fn json_tree_nodes(v: Option<&serde_json::Value>) -> usize {
    let Some(v) = v else {
        return 0;
    };
    let children = v
        .get("children")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().map(|c| json_tree_nodes(Some(c))).sum::<usize>())
        .unwrap_or(0);
    1 + children
}

/// Build the full expanded-body text for a tool step: the friendly parameter
/// description, plus the tool's result text (if any, e.g. "已添加到画布").
fn tool_body_text(name: &str, args: &serde_json::Value, result: &str, error: bool) -> String {
    let mut out = tool_call_detail(name, args);
    if !result.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(if error { "→ ✕ " } else { "→ ✓ " });
        out.push_str(result);
    }
    out
}

/// A friendly, human-readable description of one tool call: what shape it drew
/// and the key parameters (coordinates, size, points, color…). Falls back to a
/// pretty-printed JSON blob if the arguments don't match a known shape or carry
/// none of the expected fields.
fn tool_call_detail(name: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return args.to_string(),
    };
    let f = |k: &str| obj.get(k).and_then(|v| v.as_f64());
    let out = match name {
        "draw_rectangle" | "draw_ellipse" | "draw_diamond" => {
            let kind = tool_label(name);
            let (x, y, w, h) = (f("x"), f("y"), f("w"), f("h"));
            let mut out = String::new();
            if let (Some(x), Some(y)) = (x, y) {
                out.push_str(&format!("{kind} ({x:.0}, {y:.0})"));
            }
            if let (Some(w), Some(h)) = (w, h) {
                if !out.is_empty() {
                    out.push('，');
                }
                out.push_str(&format!("宽 {w:.0} × 高 {h:.0}"));
            }
            push_style(&mut out, obj.get("style"));
            out
        }
        "draw_line" | "draw_arrow" => {
            let kind = tool_label(name);
            let n = obj
                .get("points")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let mut out = format!("{kind}，经过 {n} 个点");
            push_style(&mut out, obj.get("style"));
            out
        }
        "draw_text" => {
            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let mut out = format!("文本「{}」", text.replace('\n', " "));
            let (x, y) = (f("x"), f("y"));
            if let (Some(x), Some(y)) = (x, y) {
                out.push_str(&format!("，位置 ({x:.0}, {y:.0})"));
            }
            if let Some(fs) = f("font_size") {
                out.push_str(&format!("，字号 {:.0}", fs));
            }
            out
        }
        "update_element" => {
            let mut out = format!("元素 #{}", short_id(args));
            let change = update_change_preview(args);
            if !change.is_empty() {
                out.push_str(&format!("：{change}"));
            }
            out
        }
        "delete_element" => format!("元素 #{}", short_id(args)),
        "clear_canvas" => "清空画布上的所有元素".to_string(),
        "list_elements" => "查询画布上的所有元素".to_string(),
        "use_skill" => {
            let s = obj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("场景技能「{s}」的构图规范（按需加载，非画布元素）")
        }
        "add_page" => {
            let title = obj
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("未命名");
            let ratio = obj
                .get("ratio")
                .and_then(|v| v.as_str())
                .unwrap_or("16:9");
            format!("「{title}」（{ratio}）")
        }
        _ => String::new(),
    };
    if out.trim().is_empty() {
        serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string())
    } else {
        out
    }
}

/// Append a short style summary (stroke / fill colors) to a tool description.
fn push_style(out: &mut String, style: Option<&serde_json::Value>) {
    let style = match style.and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return,
    };
    let stroke = color_hex(style.get("stroke"));
    let fill = color_hex(style.get("fill"));
    if stroke.is_some() || fill.is_some() {
        out.push('\n');
        let parts: Vec<String> = [
            stroke.map(|c| format!("描边 {c}")),
            fill.map(|c| format!("填充 {c}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        out.push_str(&parts.join(" · "));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        tool_body_text, tool_call_detail, tool_chip_preview, tool_header, tool_op, ToolOp,
    };

    #[test]
    fn use_skill_step_shows_skill_name() {
        let args: serde_json::Value = serde_json::from_str(r#"{"name":"mindmap"}"#).unwrap();
        let (title, _) = tool_header("use_skill", &args);
        assert_eq!(title, "📖 加载技能 mindmap");
        let detail = tool_call_detail("use_skill", &args);
        assert!(detail.contains("mindmap"));
    }

    #[test]
    fn tool_detail_describes_rectangle_with_coords_and_size() {
        // Colors as decimals: 0x1e1e1e = 1973790, 0xa5d8ff = 10868991.
        let args: serde_json::Value = serde_json::from_str(
            r#"{"x":100.0,"y":200.0,"w":120.0,"h":60.0,"style":{"stroke":1973790,"fill":10868991}}"#,
        )
        .unwrap();
        let detail = tool_call_detail("draw_rectangle", &args);
        assert!(detail.contains("矩形"));
        assert!(detail.contains("(100, 200)"));
        assert!(detail.contains("宽 120 × 高 60"));
        assert!(detail.contains("描边 #1E1E1E"));
        assert!(detail.contains("填充 #A5D8FF"));
    }

    #[test]
    fn tool_detail_describes_text_content() {
        let args: serde_json::Value =
            serde_json::from_str(r#"{"x":5.0,"y":6.0,"text":"开始","font_size":24.0}"#).unwrap();
        let detail = tool_call_detail("draw_text", &args);
        assert!(detail.contains("文本「开始」"));
        assert!(detail.contains("位置 (5, 6)"));
        assert!(detail.contains("字号 24"));
    }

    #[test]
    fn tool_detail_counts_points_for_arrow() {
        let args: serde_json::Value = serde_json::from_str(
            r#"{"points":[{"x":0.0,"y":0.0},{"x":10.0,"y":0.0},{"x":10.0,"y":20.0}]}"#,
        )
        .unwrap();
        let detail = tool_call_detail("draw_arrow", &args);
        assert!(detail.contains("箭头"));
        assert!(detail.contains("3 个点"));
    }

    #[test]
    fn tool_detail_falls_back_to_json_for_unknown_args() {
        let args: serde_json::Value = serde_json::from_str(r#"{"weird":true}"#).unwrap();
        let detail = tool_call_detail("draw_rectangle", &args);
        // No recognizable shape fields → pretty JSON fallback.
        assert!(detail.contains("weird"));
    }

    #[test]
    fn chip_preview_shows_text_content_truncated() {
        let short = serde_json::from_str(r#"{"text":"开始"}"#).unwrap();
        assert_eq!(tool_chip_preview("draw_text", &short), "「开始」");
        // Long text is truncated to 10 chars with an ellipsis.
        let long = serde_json::from_str(r#"{"text":"输入用户名和密码并提交"}"#).unwrap();
        let p = tool_chip_preview("draw_text", &long);
        assert!(p.starts_with("「"));
        assert!(p.ends_with("…」"));
    }

    #[test]
    fn chip_preview_shows_shape_position() {
        let args = serde_json::from_str(r#"{"x":100.0,"y":200.0,"w":120.0,"h":60.0}"#).unwrap();
        assert_eq!(tool_chip_preview("draw_rectangle", &args), "(100,200)");
    }

    #[test]
    fn chip_preview_empty_for_unknown_or_missing() {
        let args = serde_json::from_str(r#"{"weird":true}"#).unwrap();
        assert_eq!(tool_chip_preview("draw_rectangle", &args), "");
    }

    #[test]
    fn chip_preview_shows_point_count_for_line() {
        let args = serde_json::from_str(
            r#"{"points":[{"x":0.0,"y":0.0},{"x":1.0,"y":1.0},{"x":2.0,"y":0.0}]}"#,
        )
        .unwrap();
        assert_eq!(tool_chip_preview("draw_arrow", &args), "3 点");
    }

    #[test]
    fn body_text_appends_result_when_present() {
        let args = serde_json::from_str(r#"{"x":100.0,"y":200.0,"w":120.0,"h":60.0}"#).unwrap();
        // No result -> just the detail, no arrow.
        let without = tool_body_text("draw_rectangle", &args, "", false);
        assert!(!without.contains("已添加到画布"));
        // Success result -> detail + "→ ✓ 已添加到画布".
        let with = tool_body_text("draw_rectangle", &args, "已添加到画布", false);
        assert!(with.contains("矩形"));
        assert!(with.contains("→ ✓ 已添加到画布"));
        // Error result is marked with ✕.
        let err = tool_body_text("update_element", &args, "找不到元素 id=abc", true);
        assert!(err.contains("→ ✕ 找不到元素 id=abc"));
    }

    #[test]
    fn tool_header_distinguishes_add_update_delete_query() {
        // Add: "➕ 新增矩形 (100,200)"
        let (title, _) = tool_header(
            "draw_rectangle",
            &serde_json::from_str(r#"{"x":100.0,"y":200.0,"w":10.0,"h":20.0}"#).unwrap(),
        );
        assert_eq!(title, "➕ 新增矩形 (100,200)");

        // Update carries the id + what changed (position, no arrow).
        let (title, _) = tool_header(
            "update_element",
            &serde_json::from_str(r#"{"id":"abc12345-aaaa-bbbb","x":5.0,"y":6.0}"#).unwrap(),
        );
        assert_eq!(title, "✎ 修改 #abc12345 位置 (5,6)");

        // Update of text only.
        let (title, _) = tool_header(
            "update_element",
            &serde_json::from_str(r#"{"id":"abc12345","text":"开始"}"#).unwrap(),
        );
        assert_eq!(title, "✎ 修改 #abc12345 文字「开始」");

        // Delete carries the id.
        let (title, _) = tool_header(
            "delete_element",
            &serde_json::from_str(r#"{"id":"abc12345-aaaa-bbbb"}"#).unwrap(),
        );
        assert_eq!(title, "🗑 删除 #abc12345");

        // Query needs no args.
        let (title, _) = tool_header("list_elements", &serde_json::Value::Null);
        assert_eq!(title, "📋 查询元素");

        // Clear canvas.
        let (title, _) = tool_header("clear_canvas", &serde_json::Value::Null);
        assert_eq!(title, "🧹 清空画布");
        assert_eq!(tool_op("clear_canvas"), ToolOp::Clear);
    }

    #[test]
    fn tool_op_colors_differ_by_operation() {
        // Add (green) != Update (blue) != Delete (red) != Query (gray).
        assert_ne!(
            tool_op("draw_rectangle").color(),
            tool_op("update_element").color()
        );
        assert_ne!(
            tool_op("draw_rectangle").color(),
            tool_op("delete_element").color()
        );
        assert_ne!(
            tool_op("draw_rectangle").color(),
            tool_op("list_elements").color()
        );
        assert_eq!(tool_op("list_elements"), ToolOp::Query);
    }
}
