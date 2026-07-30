//! AI side panel: settings, chat with SSE streaming, insert-to-canvas.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use gpui::prelude::*;
use gpui::*;

use crate::board::BoardView;
use crate::text::{utf16_to_utf8, utf8_to_utf16};

use super::client::{chat_stream, AiStreamEvent, ChatMessage};
use super::settings::AiSettings;

const SYSTEM_PROMPT: &str = "你是 boundless 白板应用中的创作助手。回答简洁清晰，使用中文，除非用户用其他语言提问。内容可能会被放入白板，优先使用简洁的分段文字。";

// ---------------------------------------------------------------------
// TextField: a minimal single-line input with IME support
// ---------------------------------------------------------------------

pub struct Submit;

pub struct TextField {
    text: String,
    caret: usize, // char offset
    marked: Option<Range<usize>>,
    placeholder: &'static str,
    masked: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<Submit> for TextField {}

impl TextField {
    pub fn new(placeholder: &'static str, masked: bool, cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            caret: 0,
            marked: None,
            placeholder,
            masked,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn text(&self) -> String {
        self.text.clone()
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.caret = self.text.chars().count();
        self.marked = None;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
    }

    fn display_text(&self) -> String {
        if self.masked {
            "•".repeat(self.text.chars().count())
        } else {
            self.text.clone()
        }
    }

    fn char_to_byte(&self, char_off: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_off)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    fn byte_to_char(&self, byte_off: usize) -> usize {
        self.text[..byte_off.min(self.text.len())].chars().count()
    }

    fn insert(&mut self, s: &str) {
        let byte = self.char_to_byte(self.caret);
        self.text.insert_str(byte, s);
        self.caret += s.chars().count();
    }

    fn backspace(&mut self) {
        if self.caret > 0 {
            let start = self.char_to_byte(self.caret - 1);
            let end = self.char_to_byte(self.caret);
            self.text.replace_range(start..end, "");
            self.caret -= 1;
        }
    }

    fn delete_forward(&mut self) {
        let len = self.text.chars().count();
        if self.caret < len {
            let start = self.char_to_byte(self.caret);
            let end = self.char_to_byte(self.caret + 1);
            self.text.replace_range(start..end, "");
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        // Only handle editing control keys; character keys must fall through
        // so GPUI generates WM_CHAR and routes it to replace_text_in_range.
        let handled = match event.keystroke.key.as_str() {
            "left" => {
                self.caret = self.caret.saturating_sub(1);
                true
            }
            "right" => {
                self.caret = (self.caret + 1).min(self.text.chars().count());
                true
            }
            "home" => {
                self.caret = 0;
                true
            }
            "end" => {
                self.caret = self.text.chars().count();
                true
            }
            "backspace" => {
                self.backspace();
                true
            }
            "delete" => {
                self.delete_forward();
                true
            }
            "enter" => {
                cx.emit(Submit);
                true
            }
            "a" if ctrl => true,
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
        cx.notify();
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let start = utf16_to_utf8(&self.text, range.start);
        let end = utf16_to_utf8(&self.text, range.end);
        self.text.get(start..end).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let utf16 = utf8_to_utf16(&self.text, self.char_to_byte(self.caret));
        Some(UTF16Selection {
            range: utf16..utf16,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.clone().map(|r| {
            utf8_to_utf16(&self.text, self.char_to_byte(r.start))
                ..utf8_to_utf16(&self.text, self.char_to_byte(r.end))
        })
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let char_range = range.map(|r| {
            let start = self.byte_to_char(utf16_to_utf8(&self.text, r.start));
            let end = self.byte_to_char(utf16_to_utf8(&self.text, r.end));
            start..end
        });
        if let Some(r) = char_range {
            // Replace an explicit range (used for IME composition updates).
            let start_byte = self.char_to_byte(r.start);
            let end_byte = self.char_to_byte(r.end);
            self.text.replace_range(start_byte..end_byte, text);
            self.caret = r.start + text.chars().count();
        } else {
            self.insert(text);
        }
        self.marked = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let char_range = range.map(|r| {
            let start = self.byte_to_char(utf16_to_utf8(&self.text, r.start));
            let end = self.byte_to_char(utf16_to_utf8(&self.text, r.end));
            start..end
        });
        let start = char_range.as_ref().map(|r| r.start).unwrap_or(self.caret);
        let start_byte = self.char_to_byte(start);
        let end_byte = char_range
            .as_ref()
            .map(|r| self.char_to_byte(r.end))
            .unwrap_or(start_byte);
        self.text.replace_range(start_byte..end_byte, new_text);
        let mark_start = start;
        let mark_end = mark_start + new_text.chars().count();
        self.marked = Some(mark_start..mark_end);
        self.caret = mark_end;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // IME popup anchors near the field; precise caret x would require
        // re-shaping here, the field is single-line so the field's own
        // bounds are a reasonable anchor.
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Click-to-caret is handled by focusing; fine-grained mapping is
        // unnecessary for a single-line field.
        Some(utf8_to_utf16(&self.text, self.char_to_byte(self.caret)))
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let focus = self.focus_handle.clone();
        let display = self.display_text();
        let placeholder = self.placeholder;
        let caret_byte = if self.masked {
            "•".len() * self.caret
        } else {
            self.char_to_byte(self.caret)
        };
        let focused = self.focus_handle.is_focused(window);

        let field = canvas(
            move |_bounds, window, _cx| {
                let font_size = px(13.0);
                let is_empty = display.is_empty();
                let color = if is_empty {
                    hsla(0., 0., 0.55, 1.)
                } else {
                    hsla(0., 0., 0.13, 1.)
                };
                let text: SharedString = if is_empty {
                    placeholder.into()
                } else {
                    display.into()
                };
                let len = text.len();
                let line = window.text_system().shape_line(
                    text,
                    font_size,
                    &[TextRun {
                        len,
                        font: gpui::font(".SystemUIFont"),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                );
                let caret_x = if is_empty {
                    px(0.0)
                } else {
                    line.x_for_index(caret_byte)
                };
                (line, caret_x)
            },
            move |bounds, (line, caret_x), window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
                let origin = point(bounds.origin.x + px(8.0), bounds.origin.y);
                let _ = line.paint(origin, bounds.size.height, window, cx);
                if focused {
                    window.paint_quad(fill(
                        Bounds {
                            origin: point(origin.x + caret_x, bounds.origin.y + px(6.0)),
                            size: size(px(1.0), bounds.size.height - px(12.0)),
                        },
                        hsla(0., 0., 0.13, 1.),
                    ));
                }
            },
        )
        .size_full();

        div()
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .w_full()
            .h_8()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xd6d4d0))
            .rounded_md()
            .cursor_text()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                this.focus_handle.focus(window);
                cx.stop_propagation();
                cx.notify();
            }))
            .on_key_down(cx.listener(Self::on_key))
            .child(field)
    }
}

// ---------------------------------------------------------------------
// AiPanel
// ---------------------------------------------------------------------

struct StreamingState {
    buffer: String,
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

pub struct AiPanel {
    board: WeakEntity<BoardView>,
    settings: AiSettings,
    show_settings: bool,
    messages: Vec<ChatMessage>,
    streaming: Option<StreamingState>,
    error: Option<String>,
    input: Entity<TextField>,
    base_url_field: Entity<TextField>,
    api_key_field: Entity<TextField>,
    model_field: Entity<TextField>,
    notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl AiPanel {
    pub fn new(board: WeakEntity<BoardView>, cx: &mut Context<Self>) -> Self {
        let settings = AiSettings::load();

        let input = cx.new(|cx| TextField::new("给 AI 发送消息…", false, cx));
        let base_url_field = cx.new(|cx| TextField::new("https://api.openai.com/v1", false, cx));
        let api_key_field = cx.new(|cx| TextField::new("sk-...", true, cx));
        let model_field = cx.new(|cx| TextField::new("gpt-4o-mini", false, cx));

        base_url_field.update(cx, |f, cx| f.set_text(settings.base_url.clone(), cx));
        api_key_field.update(cx, |f, cx| f.set_text(settings.api_key.clone(), cx));
        model_field.update(cx, |f, cx| f.set_text(settings.model.clone(), cx));

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&input, |this, _, _: &Submit, cx| {
            this.send_message(cx);
        }));

        Self {
            board,
            settings,
            show_settings: false,
            messages: Vec::new(),
            streaming: None,
            error: None,
            input,
            base_url_field,
            api_key_field,
            model_field,
            notice: None,
            _subscriptions: subscriptions,
        }
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let base_url = self.base_url_field.read(cx).text();
        let api_key = self.api_key_field.read(cx).text();
        let model = self.model_field.read(cx).text();
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

    fn send_message(&mut self, cx: &mut Context<Self>) {
        if self.streaming.is_some() {
            return;
        }
        let text = self.input.read(cx).text();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.update(cx, |f, cx| f.clear(cx));
        self.error = None;
        self.messages.push(ChatMessage::user(text));

        let mut request_messages = vec![ChatMessage::system(SYSTEM_PROMPT)];
        // Keep the conversation bounded.
        let history: Vec<ChatMessage> = self
            .messages
            .iter()
            .rev()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        request_messages.extend(history);

        let request = chat_stream(self.settings.clone(), request_messages);
        self.start_stream_task(request.events, request.cancel, cx);
        cx.notify();
    }

    fn continue_selection(&mut self, cx: &mut Context<Self>) {
        if self.streaming.is_some() {
            return;
        }
        let Some(selected) = self
            .board
            .upgrade()
            .and_then(|board| board.read(cx).selected_text_content())
        else {
            self.notice = Some("请先在画布上选中一个文本元素".to_string());
            cx.notify();
            return;
        };
        self.error = None;
        let prompt = format!("请续写以下内容，保持风格一致：\n\n{selected}");
        self.messages.push(ChatMessage::user(prompt));

        let mut request_messages = vec![ChatMessage::system(SYSTEM_PROMPT)];
        request_messages.extend(self.messages.iter().rev().take(20).cloned().collect::<Vec<_>>().into_iter().rev());

        let request = chat_stream(self.settings.clone(), request_messages);
        self.start_stream_task(request.events, request.cancel, cx);
        cx.notify();
    }

    fn start_stream_task(
        &mut self,
        mut events: futures::channel::mpsc::UnboundedReceiver<AiStreamEvent>,
        cancel: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let keep_going = this
                    .update(cx, |panel, cx| match event {
                        AiStreamEvent::Delta(text) => {
                            if let Some(s) = panel.streaming.as_mut() {
                                s.buffer.push_str(&text);
                            }
                            cx.notify();
                            true
                        }
                        AiStreamEvent::Done => {
                            panel.finish_streaming(cx);
                            false
                        }
                        AiStreamEvent::Error(err) => {
                            panel.error = Some(err);
                            panel.streaming = None;
                            cx.notify();
                            false
                        }
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        });
        self.streaming = Some(StreamingState {
            buffer: String::new(),
            cancel,
            _task: task,
        });
    }

    fn finish_streaming(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.streaming.take() {
            if !s.buffer.trim().is_empty() {
                self.messages.push(ChatMessage::assistant(s.buffer));
            }
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
        self.stop_streaming(cx);
        self.streaming = None;
        self.messages.clear();
        self.error = None;
        cx.notify();
    }
}

impl Render for AiPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                        panel_button("设置", self.show_settings).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.show_settings = !this.show_settings;
                                cx.notify();
                            },
                        )),
                    )
                    .child(panel_button("清空", false).on_click(cx.listener(
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
                .child(field_row("Base URL", self.base_url_field.clone()))
                .child(field_row("API Key", self.api_key_field.clone()))
                .child(field_row("模型", self.model_field.clone()))
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

        // ---- messages ----
        let mut messages = div().flex().flex_col().gap_2().p_3();
        if self.messages.is_empty() && !streaming {
            messages = messages.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x999999))
                    .child("输入问题开始对话；回复可一键插入画布。"),
            );
        }
        for (idx, msg) in self.messages.iter().enumerate() {
            messages = messages.child(self.render_message(idx, msg, cx));
        }
        if let Some(s) = &self.streaming {
            let mut body = s.buffer.clone();
            body.push('▍');
            messages = messages.child(message_bubble(
                "assistant",
                body,
                None,
            ));
        }
        let messages_area = div()
            .id("ai-messages")
            .flex_1()
            .overflow_y_scroll()
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

        // ---- input row ----
        let send_label = if streaming { "停止" } else { "发送" };
        let input_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(rgb(0xeeeeec))
            .child(div().flex_1().child(self.input.clone()))
            .child(
                panel_button("续写选中", false).on_click(cx.listener(|this, _, _, cx| {
                    this.continue_selection(cx);
                })),
            )
            .child(panel_button(send_label, false).on_click(cx.listener(
                move |this, _, _, cx| {
                    if streaming {
                        this.stop_streaming(cx);
                    } else {
                        this.send_message(cx);
                    }
                },
            )));

        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(360.0))
            .bg(rgb(0xfbfaf9))
            .border_l_1()
            .border_color(rgb(0xe3e2df))
            .shadow_lg()
            .flex()
            .flex_col()
            // Shield the canvas from panel interactions.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_key_down(|_, _, cx| cx.stop_propagation())
            .child(header)
            .child(settings_section)
            .child(messages_area)
            .child(error_bar)
            .child(input_row)
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
        message_bubble(&msg.role, content, insert_button)
    }
}

fn message_bubble(
    role: &str,
    content: String,
    extra: Option<Stateful<Div>>,
) -> Div {
    let is_user = role == "user";
    let bubble = div()
        .px_3()
        .py_2()
        .rounded_lg()
        .text_sm()
        .child(content)
        .when(is_user, |d| d.bg(rgb(0xdce8ff)))
        .when(!is_user, |d| d.bg(rgb(0xffffff)).border_1().border_color(rgb(0xe9e8e5)));
    let mut col = div().flex().flex_col().gap_1();
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

fn field_row(label: &'static str, field: Entity<TextField>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(0x777777)).child(label))
        .child(field)
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
