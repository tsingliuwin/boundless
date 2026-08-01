//! AI side panel: settings, chat with SSE streaming, insert-to-canvas.
//!
//! Input fields use gpui-component's `InputState`/`Input` (multi-line with
//! auto-grow, IME, selection, clipboard) instead of a hand-rolled TextField.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use gpui::prelude::*;
use gpui::*;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::IconName;
use gpui_component::Sizable;

use crate::board::BoardView;

use super::agent::{AgentEvent, AgentRequest, BoundlessAgent};
use super::client::ChatMessage;
use super::settings::{AiSettings, ReasoningLevel};
use super::store::{
    self, create_session, delete_session, list_sessions, load_messages, SessionMeta,
};


// ---------------------------------------------------------------------
// AiPanel
// ---------------------------------------------------------------------


struct StreamingState {
    buffer: String,
    cancel: Arc<AtomicBool>,
    /// Accumulated reasoning/thinking text (shown in a collapsible panel).
    reasoning: String,
    /// True while the model is actively emitting reasoning deltas. The
    /// reasoning panel is expanded while this is true, auto-collapsed on Done.
    reasoning_active: bool,
    /// Ordered list of tool names the model has called so far, shown as steps.
    tool_calls: Vec<String>,
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
    /// User override for the reasoning panel: Some(true/false) = force
    /// expand/collapse; None = auto (expand while streaming, collapse after).
    reasoning_user_open: Option<bool>,
    /// Scroll handle for the messages area, used to auto-scroll to bottom.
    messages_scroll: ScrollHandle,
    /// Current panel width in px. User-resizable via the left edge handle.
    width: f32,
    _subscriptions: Vec<Subscription>,
}

/// Default / min / max panel width in px.
pub const DEFAULT_WIDTH: f32 = 360.0;
const MIN_WIDTH: f32 = 280.0;
const MAX_WIDTH: f32 = 640.0;

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
        subscriptions.push(cx.subscribe(&input, |this, _entity, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { secondary } = event {
                if !secondary {
                    this.send_message(cx);
                }
            }
        }));

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
            reasoning_user_open: None,
            messages_scroll: ScrollHandle::new(),
            width: DEFAULT_WIDTH,
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

        match BoundlessAgent::stream(&self.settings, prompt, history) {
            Ok(request) => self.start_stream_task(request, cx),
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
            }
        }
        cx.notify();
    }

    fn start_stream_task(&mut self, request: AgentRequest, cx: &mut Context<Self>) {
        // Reset the user's reasoning-panel toggle so it auto-expands on the
        // new stream's reasoning, regardless of what they did last time.
        self.reasoning_user_open = None;
        let mut events = request.events;
        let cancel = request.cancel;
        let board = self.board.clone();
        let task = cx.spawn(async move |this, cx| {
            // The agent merges text deltas, tool-call hints, and canvas ops
            // into this single event stream (see agent::handle_agent_item and
            // the select loop). We just drain and dispatch.
            while let Some(event) = events.next().await {
                let keep_going = this
                    .update(cx, |panel, cx| match event {
                        AgentEvent::Delta(text) => {
                            if let Some(s) = panel.streaming.as_mut() {
                                // Text arrives after reasoning ends; mark reasoning done.
                                s.reasoning_active = false;
                                s.buffer.push_str(&text);
                            }
                            cx.notify();
                            true
                        }
                        AgentEvent::Reasoning(text) => {
                            if let Some(s) = panel.streaming.as_mut() {
                                s.reasoning_active = true;
                                s.reasoning.push_str(&text);
                            }
                            cx.notify();
                            true
                        }
                        AgentEvent::CanvasOp(op) => {
                            // Apply the op to the board on the main thread.
                            board
                                .update(cx, |board, cx| board.apply_canvas_op(op, cx))
                                .ok();
                            cx.notify();
                            true
                        }
                        AgentEvent::ToolCall(name) => {
                            if let Some(s) = panel.streaming.as_mut() {
                                s.reasoning_active = false;
                                s.tool_calls.push(name);
                            }
                            cx.notify();
                            true
                        }
                        AgentEvent::Done { text } => {
                            panel.finish_streaming(text, cx);
                            false
                        }
                        AgentEvent::Error(err) => {
                            panel.error = Some(err);
                            panel.streaming = None;
                            cx.notify();
                            false
                        }
                    })
                    .unwrap_or(false);
                // Auto-scroll the messages area to show the latest content,
                // whether streaming or just finished. scroll_to_bottom sets a
                // flag that GPUI consumes during the next paint.
                this.update(cx, |panel, _cx| {
                    panel.messages_scroll.scroll_to_bottom();
                })
                .ok();
                if !keep_going {
                    break;
                }
            }
        });
        self.streaming = Some(StreamingState {
            buffer: String::new(),
            cancel,
            reasoning: String::new(),
            reasoning_active: false,
            tool_calls: Vec::new(),
            _task: task,
        });
    }

    fn finish_streaming(&mut self, final_text: String, cx: &mut Context<Self>) {
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
                // Preserve the reasoning and tool calls so they survive after
                // streaming — the user can re-expand the thinking panel.
                if !s.reasoning.is_empty() {
                    msg.reasoning = Some(s.reasoning);
                }
                if !s.tool_calls.is_empty() {
                    msg.tool_calls = s.tool_calls;
                }
                self.persist(&msg);
                self.messages.push(msg);
            }
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
            self.input
                .update(cx, |s, cx| s.set_value("", window, cx));
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
                    .child(panel_button("新建", false).on_click(cx.listener(
                        |this, _, _, cx| this.clear_chat(cx),
                    ))),
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
                        .child(panel_button("保存设置", false).on_click(cx.listener(
                            |this, _, _, cx| this.save_settings(cx),
                        ))),
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
                        .child(panel_button("＋ 新建", false).on_click(cx.listener(
                            |this, _, _, cx| this.new_session(cx),
                        ))),
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
            messages = messages.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x999999))
                    .child("描述你想画的内容，例如「画一个登录流程图」，AI 会直接把它画到画布上。"),
            );
        }
        for (idx, msg) in self.messages.iter().enumerate() {
            messages = messages.child(self.render_message(idx, msg, cx));
        }
        if let Some(s) = &self.streaming {
            // A single agent step bubble that evolves as the stream progresses:
            // "思考中…" → reasoning panel → tool calls → text response. It's
            // always rendered (no separate "thinking" bubble) so the transition
            // feels continuous.
            let mut step = div()
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .px_3()
                .py_2()
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgb(0xe9e8e5))
                .rounded_lg();

            // While no content has arrived yet, show a compact "thinking…"
            // line inside the same bubble.
            let has_content =
                !s.reasoning.is_empty() || !s.tool_calls.is_empty() || !s.buffer.is_empty();
            if !has_content {
                step = step.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x999999))
                        .child("思考中…"),
                );
            }

            // Reasoning / thinking panel: auto-expanded while streaming
            // (reasoning_active), auto-collapsed after. The user can manually
            // toggle it at any time — their choice takes priority until the
            // next stream starts. Capped height so it can't fill the area.
            if !s.reasoning.is_empty() {
                // Determine open state: user override wins, else auto (active).
                let reasoning_open = self.reasoning_user_open.unwrap_or(s.reasoning_active);
                let reasoning_text = s.reasoning.clone();
                // Clickable header.
                let header = div()
                    .id("reasoning-toggle")
                    .flex()
                    .flex_row()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(rgb(0x888888))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0xf5f5f5)).rounded_md())
                    .child(div().child(if reasoning_open { "▾" } else { "▸" }))
                    .child(div().child("思考过程"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reasoning_user_open =
                            Some(!this.reasoning_user_open.unwrap_or(false));
                        cx.notify();
                    }));
                step = step.child(header);
                if reasoning_open {
                    step = step.child(
                        div()
                            .id("reasoning-scroll")
                            .max_h(px(200.0))
                            .overflow_y_scroll()
                            .text_xs()
                            .text_color(rgb(0x666666))
                            .py_1()
                            .child(reasoning_text),
                    );
                }
            }

            // Tool call steps: show each as a labeled tag with a check icon.
            if !s.tool_calls.is_empty() {
                let mut tools = div().flex().flex_col().gap_1();
                for name in &s.tool_calls {
                    let label = tool_label(name);
                    tools = tools.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(rgb(0xdce8ff))
                                    .text_color(rgb(0x1a5fd7))
                                    .child(format!("✏️ {label}")),
                            )
                            .child(div().text_color(rgb(0x2f9e44)).child("✓")),
                    );
                }
                step = step.child(tools);
            }

            // Text response (streaming, with cursor).
            if !s.buffer.is_empty() {
                let mut body = s.buffer.clone();
                body.push('▍');
                step = step.child(div().text_sm().child(body));
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
        for (i, level) in [ReasoningLevel::Low, ReasoningLevel::Medium, ReasoningLevel::High]
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
                seg = seg
                    .hover(|s| s.bg(rgb(0xefeeec)))
                    .text_color(rgb(0x555555));
            }
            reasoning_control = reasoning_control.child(seg.on_click(cx.listener(
                move |this, _, _, cx| {
                    this.set_reasoning(level, cx);
                },
            )));
        }

        // Send/stop icon button (gpui-component Button).
        let send_btn = Button::new("send-btn")
            .icon(if streaming_now { IconName::Close } else { IconName::ArrowUp })
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
                // window.bounds() — that's in screen coordinates.)
                let win_right = f32::from(window.viewport_size().width);
                let pointer_x = f32::from(event.event.position.x);
                let w = (win_right - pointer_x).clamp(MIN_WIDTH, MAX_WIDTH);
                panel_mv
                    .update(cx, |this, cx| {
                        if (this.width - w).abs() > 0.01 {
                            this.width = w;
                            cx.notify();
                        }
                    })
                    .ok();
            })
            // Shield the canvas from pointer interactions over the panel.
            // NOTE: there is intentionally NO `.on_key_down` stop-propagation
            // here. On Windows, GPUI generates the WM_CHAR that delivers typed
            // characters to a focused TextField ONLY if the WM_KEYDOWN is left
            // unhandled (propagate=true). A blanket `on_key_down` →
            // stop_propagation would mark the key "handled" and suppress
            // TranslateMessage, so no character would ever reach
            // `replace_text_in_range` — every input field would show a caret
            // but accept no typing. Canvas tool shortcuts are already disabled
            // while a text field has focus via the "Board && !TextInput" key
            // context (see main.rs), so this guard is unnecessary.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
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
    fn render_message(
        &self,
        idx: usize,
        msg: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_user = msg.role == "user";
        let content = msg.content.clone();
        let reasoning = msg.reasoning.clone();
        let tool_calls = msg.tool_calls.clone();
        let insert_button = if !is_user {
            let content = content.clone();
            Some(
                panel_button("插入画布", false).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.insert_to_canvas(content.clone(), cx);
                    },
                )),
            )
        } else {
            None
        };
        let _ = idx;

        // For assistant messages with reasoning/tool-calls, render them inside
        // a step bubble (same style as the streaming step) so the thinking
        // process and tools survive after streaming completes.
        if !is_user && (reasoning.is_some() || !tool_calls.is_empty()) {
            let mut step = div()
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .px_3()
                .py_2()
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgb(0xe9e8e5))
                .rounded_lg();

            // Reasoning panel (collapsed by default after completion).
            if let Some(reasoning_text) = reasoning {
                let reasoning_open =
                    self.reasoning_user_open.unwrap_or(false);
                let header = div()
                    .id("reasoning-toggle-done")
                    .flex()
                    .flex_row()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(rgb(0x888888))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0xf5f5f5)).rounded_md())
                    .child(div().child(if reasoning_open { "▾" } else { "▸" }))
                    .child(div().child("思考过程"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reasoning_user_open =
                            Some(!this.reasoning_user_open.unwrap_or(false));
                        cx.notify();
                    }));
                step = step.child(header);
                if reasoning_open {
                    step = step.child(
                        div()
                            .id("reasoning-scroll-done")
                            .max_h(px(200.0))
                            .overflow_y_scroll()
                            .text_xs()
                            .text_color(rgb(0x666666))
                            .py_1()
                            .child(reasoning_text),
                    );
                }
            }

            // Tool call tags.
            if !tool_calls.is_empty() {
                let mut tools = div().flex().flex_col().gap_1();
                for name in &tool_calls {
                    let label = tool_label(name);
                    tools = tools.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(rgb(0xdce8ff))
                                    .text_color(rgb(0x1a5fd7))
                                    .child(format!("✏️ {label}")),
                            )
                            .child(div().text_color(rgb(0x2f9e44)).child("✓")),
                    );
                }
                step = step.child(tools);
            }

            // Text response.
            step = step.child(div().text_sm().child(content));

            let mut col = div().flex().flex_col().gap_1().items_start();
            col = col.child(step);
            if let Some(extra) = insert_button {
                col = col.child(extra);
            }
            return col.into_any_element();
        }

        message_bubble(&msg.role, content, insert_button).into_any_element()
    }
}

fn message_bubble(
    role: &str,
    content: String,
    extra: Option<Stateful<Div>>,
) -> Div {
    let is_user = role == "user";
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
            .child(content)
            .bg(rgb(0xdce8ff))
    } else {
        div()
            .px_3()
            .py_2()
            .rounded_lg()
            .text_sm()
            .w_full()
            .min_w_0()
            .child(content)
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xe9e8e5))
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
        _ => "图形",
    }
}
