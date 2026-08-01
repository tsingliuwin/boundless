//! AI side panel: settings, chat with SSE streaming, insert-to-canvas.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use gpui::prelude::*;
use gpui::*;

use crate::board::BoardView;
use crate::text::{utf16_to_utf8, utf8_to_utf16};

use super::agent::{AgentEvent, AgentRequest, BoundlessAgent};
use super::client::ChatMessage;
use super::settings::{AiSettings, ReasoningLevel};
use super::store::{
    self, create_session, delete_session, list_sessions, load_messages, SessionMeta,
};

// ---------------------------------------------------------------------
// TextField: a minimal single-line input with IME support
// ---------------------------------------------------------------------

pub struct Submit;

pub struct TextField {
    text: String,
    caret: usize, // char offset
    /// Active selection (char offsets, ordered). None = no selection; the caret
    /// is the active boundary. Needed so copy/cut/paste/select-all work.
    selection: Option<Range<usize>>,
    marked: Option<Range<usize>>,
    placeholder: &'static str,
    masked: bool,
    /// Whether this field supports multiple lines (Shift+Enter inserts a
    /// newline; plain Enter emits `Submit`). Single-line fields emit `Submit`
    /// on any Enter.
    multiline: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<Submit> for TextField {}

impl TextField {
    pub fn new(placeholder: &'static str, masked: bool, cx: &mut Context<Self>) -> Self {
        Self::with_multiline(placeholder, masked, false, cx)
    }

    /// Create a multi-line field (the chat input).
    pub fn new_multiline(placeholder: &'static str, cx: &mut Context<Self>) -> Self {
        Self::with_multiline(placeholder, false, true, cx)
    }

    fn with_multiline(
        placeholder: &'static str,
        masked: bool,
        multiline: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            text: String::new(),
            caret: 0,
            selection: None,
            marked: None,
            placeholder,
            masked,
            multiline,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn text(&self) -> String {
        self.text.clone()
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.caret = self.text.chars().count();
        self.selection = None;
        self.marked = None;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
    }

    /// Number of display lines for a multi-line field: counts `\n`-separated
    /// lines (at least 1). Used to size the input box to its content so the
    /// last line is never clipped by the toolbar.
    fn line_count(&self) -> usize {
        if self.masked {
            return 1;
        }
        self.text.split('\n').count().max(1)
    }

    /// The char range of the active selection, if any. Always normalized
    /// (start <= end) regardless of selection direction.
    fn selection_range(&self) -> Option<Range<usize>> {
        self.selection.as_ref().map(|r| {
            if r.start <= r.end {
                r.start..r.end
            } else {
                r.end..r.start
            }
        })
    }

    /// Delete the active selection (if any) and return whether anything was
    /// removed. Leaves `caret` at the deletion point and clears the selection.
    fn delete_selection(&mut self) -> bool {
        if let Some(r) = self.selection_range() {
            let start = self.char_to_byte(r.start);
            let end = self.char_to_byte(r.end);
            self.text.replace_range(start..end, "");
            self.caret = r.start;
            self.selection = None;
            true
        } else {
            false
        }
    }

    /// Move the caret one char left, extending the selection (Shift+Left).
    fn extend_selection_left(&mut self) {
        let new_caret = self.caret.saturating_sub(1);
        let prev = self.selection.take();
        self.selection = match prev {
            Some(r) => {
                // Extend the nearer boundary toward the new caret.
                if r.end == self.caret {
                    Some(r.start..new_caret)
                } else {
                    Some(self.caret..r.start)
                }
            }
            None => Some(new_caret..self.caret),
        };
        self.caret = new_caret;
    }

    /// Move the caret one char right, extending the selection (Shift+Right).
    fn extend_selection_right(&mut self) {
        let last = self.text.chars().count();
        let new_caret = (self.caret + 1).min(last);
        let prev = self.selection.take();
        self.selection = match prev {
            Some(r) => {
                if r.end == self.caret {
                    Some(r.start..new_caret)
                } else {
                    Some(self.caret..r.start)
                }
            }
            None => Some(self.caret..new_caret),
        };
        self.caret = new_caret;
    }

    /// Insert `s` at the caret, replacing any active selection first.
    fn insert(&mut self, s: &str) {
        if !self.delete_selection() {
            let byte = self.char_to_byte(self.caret);
            self.text.insert_str(byte, s);
            self.caret += s.chars().count();
        } else {
            // Selection was removed; insert at the (now-collapsed) caret.
            let byte = self.char_to_byte(self.caret);
            self.text.insert_str(byte, s);
            self.caret += s.chars().count();
        }
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

    fn backspace(&mut self) {
        // Delete the selection if there is one; otherwise the previous char.
        if !self.delete_selection() && self.caret > 0 {
            let start = self.char_to_byte(self.caret - 1);
            let end = self.char_to_byte(self.caret);
            self.text.replace_range(start..end, "");
            self.caret -= 1;
        }
    }

    fn delete_forward(&mut self) {
        // Delete the selection if there is one; otherwise the next char.
        if !self.delete_selection() {
            let len = self.text.chars().count();
            if self.caret < len {
                let start = self.char_to_byte(self.caret);
                let end = self.char_to_byte(self.caret + 1);
                self.text.replace_range(start..end, "");
            }
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        // Only handle editing control keys; character keys must fall through
        // so GPUI generates WM_CHAR and routes it to replace_text_in_range.
        let handled = match event.keystroke.key.as_str() {
            "left" => {
                // Shift+arrow extends the selection; plain arrow collapses it.
                if event.keystroke.modifiers.shift {
                    self.extend_selection_left();
                } else {
                    self.caret = self.caret.saturating_sub(1);
                    self.selection = None;
                }
                true
            }
            "right" => {
                if event.keystroke.modifiers.shift {
                    self.extend_selection_right();
                } else {
                    self.caret = (self.caret + 1).min(self.text.chars().count());
                    self.selection = None;
                }
                true
            }
            "home" => {
                if event.keystroke.modifiers.shift {
                    self.selection = Some(0..self.caret);
                } else {
                    self.selection = None;
                }
                self.caret = 0;
                true
            }
            "end" => {
                let last = self.text.chars().count();
                if event.keystroke.modifiers.shift {
                    self.selection = Some(self.caret..last);
                } else {
                    self.selection = None;
                }
                self.caret = last;
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
                if self.multiline && event.keystroke.modifiers.shift {
                    // Shift+Enter inserts a literal newline in multi-line fields.
                    self.insert("\n");
                } else {
                    // Plain Enter submits (both single- and multi-line). For a
                    // multi-line field, IME-style newline insertion isn't
                    // supported via Enter to keep "send" a single key.
                    cx.emit(Submit);
                }
                true
            }
            // Select all.
            "a" if ctrl => {
                let last = self.text.chars().count();
                self.selection = Some(0..last);
                self.caret = last;
                true
            }
            // Copy.
            "c" if ctrl => {
                if let Some(r) = self.selection_range() {
                    let start = self.char_to_byte(r.start);
                    let end = self.char_to_byte(r.end);
                    if let Some(slice) = self.text.get(start..end) {
                        cx.write_to_clipboard(ClipboardItem::new_string(slice.to_string()));
                    }
                }
                true
            }
            // Cut.
            "x" if ctrl => {
                if let Some(r) = self.selection_range() {
                    let start = self.char_to_byte(r.start);
                    let end = self.char_to_byte(r.end);
                    if let Some(slice) = self.text.get(start..end) {
                        cx.write_to_clipboard(ClipboardItem::new_string(slice.to_string()));
                    }
                    self.selection = Some(r);
                    self.delete_selection();
                }
                true
            }
            // Paste.
            "v" if ctrl => {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(text) = item.text() {
                        self.insert(&text);
                    }
                }
                true
            }
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
        let char_range = match range {
            Some(r) => {
                let start = self.byte_to_char(utf16_to_utf8(&self.text, r.start));
                let end = self.byte_to_char(utf16_to_utf8(&self.text, r.end));
                start..end
            }
            // When IME commits a composition it passes None: replace the
            // current marked (composition) region, not insert at the caret.
            // Otherwise the marked pinyin would remain alongside the committed
            // candidate (https://github.com/...).
            None => self.marked.clone().unwrap_or_else(|| match self.selection_range() {
                Some(s) => s,
                None => self.caret..self.caret,
            }),
        };
        // Delete the target range, then insert at its start.
        let start_byte = self.char_to_byte(char_range.start);
        let end_byte = self.char_to_byte(char_range.end);
        self.text.replace_range(start_byte..end_byte, text);
        self.caret = char_range.start + text.chars().count();
        self.selection = None;
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
        let char_range = match range {
            Some(r) => {
                let start = self.byte_to_char(utf16_to_utf8(&self.text, r.start));
                let end = self.byte_to_char(utf16_to_utf8(&self.text, r.end));
                start..end
            }
            // When IME updates a composition it passes None: replace the
            // previous marked region with the new composition string.
            None => self.marked.clone().unwrap_or_else(|| match self.selection_range() {
                Some(s) => s,
                None => self.caret..self.caret,
            }),
        };
        let start_byte = self.char_to_byte(char_range.start);
        let end_byte = self.char_to_byte(char_range.end);
        self.text.replace_range(start_byte..end_byte, new_text);
        let mark_start = char_range.start;
        let mark_end = mark_start + new_text.chars().count();
        self.marked = Some(mark_start..mark_end);
        self.caret = mark_end;
        self.selection = None;
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
        let multiline = self.multiline;
        let font_size = px(13.0);
        let line_height = font_size * 1.4;
        let caret_byte = if self.masked {
            "•".len() * self.caret
        } else {
            self.char_to_byte(self.caret)
        };
        let focused = self.focus_handle.is_focused(window);
        // Selection byte offsets for the (possibly masked) display text.
        let sel_bytes = self.selection_range().map(|r| {
            let s = if self.masked {
                "•".len() * r.start
            } else {
                self.char_to_byte(r.start)
            };
            let e = if self.masked {
                "•".len() * r.end
            } else {
                self.char_to_byte(r.end)
            };
            s..e
        });

        let field = canvas(
            move |bounds, window, _cx| {
                let is_empty = display.is_empty();
                let color = if is_empty {
                    hsla(0., 0., 0.55, 1.)
                } else {
                    hsla(0., 0., 0.13, 1.)
                };
                let text: SharedString = if is_empty {
                    placeholder.into()
                } else {
                    display.clone().into()
                };

                if !multiline {
                    // --- single line (settings fields): unchanged behavior ---
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
                    let sel_rect = sel_bytes.clone().map(|r| {
                        let x0 = line.x_for_index(r.start);
                        let x1 = line.x_for_index(r.end);
                        (x0.min(x1), (x1 - x0).abs())
                    });
                    FieldLayout::Single {
                        line,
                        caret_x,
                        sel_rect,
                    }
                } else {
                    // --- multi line (chat input): shape each \n-separated line,
                    //     soft-wrapping long lines at the available width. ---
                    let inner_w = bounds.size.width - px(16.0); // minus L/R padding
                    let wrap_px = inner_w.max(px(8.0));
                    // Split into paragraphs by '\n', then wrap each paragraph.
                    let mut rows: Vec<RowLayout> = Vec::new();
                    let mut byte_offset = 0usize;
                    for para in text.split('\n') {
                        for seg in wrap_paragraph(para, wrap_px, font_size, color, window) {
                            let line = window.text_system().shape_line(
                                if seg.is_empty() { " ".into() } else { seg.clone().into() },
                                font_size,
                                &[TextRun {
                                    len: if seg.is_empty() { 1 } else { seg.len() },
                                    font: gpui::font(".SystemUIFont"),
                                    color,
                                    background_color: None,
                                    underline: None,
                                    strikethrough: None,
                                }],
                                None,
                            );
                            let line = if seg.is_empty() {
                                line.with_len(0)
                            } else {
                                line
                            };
                            let row_byte_start = byte_offset;
                            let row_byte_end = byte_offset + seg.len();
                            rows.push(RowLayout {
                                line,
                                byte_start: row_byte_start,
                                byte_end: row_byte_end,
                            });
                            byte_offset += seg.len();
                        }
                        byte_offset += 1; // the '\n'
                    }
                    if rows.is_empty() {
                        // empty text: one zero-width row so the caret renders
                        let line = window.text_system().shape_line(
                            " ".into(),
                            font_size,
                            &[TextRun {
                                len: 1,
                                font: gpui::font(".SystemUIFont"),
                                color,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        rows.push(RowLayout {
                            line: line.with_len(0),
                            byte_start: 0,
                            byte_end: 0,
                        });
                    }
                    // Locate the caret's row and x within it.
                    let caret_row = rows
                        .iter()
                        .position(|r| caret_byte <= r.byte_end)
                        .unwrap_or(rows.len() - 1);
                    let row = &rows[caret_row];
                    let caret_x = if is_empty {
                        px(0.0)
                    } else {
                        let local = caret_byte.saturating_sub(row.byte_start);
                        row.line.x_for_index(local)
                    };
                    // Selection: build per-row highlight rects.
                    let mut sel_rows: Vec<(usize, Pixels, Pixels)> = Vec::new();
                    if let Some(r) = sel_bytes.clone() {
                        let (s, e) = (r.start.min(r.end), r.start.max(r.end));
                        for (i, row) in rows.iter().enumerate() {
                            if s >= row.byte_end || e <= row.byte_start {
                                continue;
                            }
                            let xs = row.line.x_for_index(s.saturating_sub(row.byte_start));
                            let xe = row.line.x_for_index(e.saturating_sub(row.byte_start).min(row.byte_end - row.byte_start));
                            sel_rows.push((i, xs.min(xe), (xe - xs).abs()));
                        }
                    }
                    FieldLayout::Multi {
                        rows,
                        caret_row,
                        caret_x,
                        sel_rows,
                        line_height,
                    }
                }
            },
            move |bounds, layout, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
                let origin = point(bounds.origin.x + px(8.0), bounds.origin.y + px(6.0));
                match layout {
                    FieldLayout::Single {
                        line,
                        caret_x,
                        sel_rect,
                    } => {
                        if let Some((sel_x, sel_w)) = sel_rect {
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(origin.x + sel_x, bounds.origin.y + px(6.0)),
                                    size: size(sel_w, bounds.size.height - px(12.0)),
                                },
                                hsla(0.58, 1.0, 0.78, 1.0),
                            ));
                        }
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
                    }
                    FieldLayout::Multi {
                        rows,
                        caret_row,
                        caret_x,
                        sel_rows,
                        line_height,
                    } => {
                        // Selection backgrounds (per row).
                        for (i, x, w) in &sel_rows {
                            let y = origin.y + *i as f32 * line_height;
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(origin.x + *x, y),
                                    size: size(*w, line_height),
                                },
                                hsla(0.58, 1.0, 0.78, 1.0),
                            ));
                        }
                        for (i, row) in rows.iter().enumerate() {
                            let y = origin.y + i as f32 * line_height;
                            let _ = row.line.paint(point(origin.x, y), line_height, window, cx);
                        }
                        if focused {
                            let y = origin.y + caret_row as f32 * line_height;
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(origin.x + caret_x, y),
                                    size: size(px(1.0), line_height),
                                },
                                hsla(0., 0., 0.13, 1.),
                            ));
                        }
                    }
                }
            },
        );

        // Size the canvas: single-line fills its fixed-height box; multi-line
        // sizes to its full content height so a scrolling container can scroll
        // through all lines (size_full would clip overflow and break scroll).
        let field = if multiline {
            let content_h = line_height * self.line_count() as f32 + px(2.0);
            field.w_full().h(content_h)
        } else {
            field.size_full()
        };

        let container = div()
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .w_full()
            .cursor_text()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                this.focus_handle.focus(window);
                cx.stop_propagation();
                cx.notify();
            }))
            .on_key_down(cx.listener(Self::on_key));

        // The two branches differ in type (`.id` -> Stateful<Div>), so unify
        // them as AnyElement. The multi-line input grows with its content: the
        // height is computed from the current line count (min 3, max 8 lines),
        // and once content exceeds 8 lines the box scrolls internally. This
        // guarantees the last line is never clipped by the toolbar, because the
        // box is always tall enough for its content (or capped + scrolling).
        if multiline {
            const MIN_LINES: usize = 3;
            const MAX_LINES: usize = 8;
            let lines = self.line_count().clamp(MIN_LINES, MAX_LINES);
            let h = line_height * lines as f32 + px(12.0); // +vertical padding
            container
                .id("chat-input")
                .h(h)
                .overflow_y_scroll()
                .px(px(8.0))
                .pt(px(6.0))
                .pb(px(6.0))
                .child(field)
                .into_any_element()
        } else {
            container
                .h_8()
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgb(0xd6d4d0))
                .rounded_md()
                .child(field)
                .into_any_element()
        }
    }
}

/// Pre-rendered geometry for one display row of a multi-line field.
struct RowLayout {
    line: ShapedLine,
    /// Byte range of this row within the full display text (excl. newline).
    byte_start: usize,
    byte_end: usize,
}

/// Distinguishes single- vs multi-line layout produced by the canvas prep
/// closure, so the paint closure knows how to render.
enum FieldLayout {
    Single {
        line: ShapedLine,
        caret_x: Pixels,
        sel_rect: Option<(Pixels, Pixels)>,
    },
    Multi {
        rows: Vec<RowLayout>,
        caret_row: usize,
        caret_x: Pixels,
        sel_rows: Vec<(usize, Pixels, Pixels)>,
        line_height: Pixels,
    },
}

/// Split a paragraph (no newlines) into wrap segments that each fit `wrap_px`,
/// breaking on spaces when possible. Mirrors render::wrap_segment but returns
/// owned strings and works on a SharedString-less input.
fn wrap_paragraph(
    s: &str,
    wrap_px: Pixels,
    font_size: Pixels,
    color: Hsla,
    window: &Window,
) -> Vec<String> {
    if wrap_px <= px(0.0) || s.is_empty() {
        return vec![s.to_string()];
    }
    let shape = |t: &str| -> Pixels {
        window
            .text_system()
            .shape_line(
                t.to_string().into(),
                font_size,
                &[TextRun {
                    len: t.len(),
                    font: gpui::font(".SystemUIFont"),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width
    };
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut last_space = None;
    for &(byte_off, ch) in &chars {
        let candidate = &s[start..byte_off + ch.len_utf8()];
        if shape(candidate) > wrap_px && byte_off > start {
            let break_at = last_space.unwrap_or(byte_off);
            out.push(s[start..break_at].to_string());
            start = break_at;
            if s[start..].starts_with(' ') {
                start += 1;
            }
            last_space = None;
        }
        if ch == ' ' {
            last_space = Some(byte_off + 1);
        }
    }
    if start < s.len() {
        out.push(s[start..].to_string());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

// ---------------------------------------------------------------------
// AiPanel
// ---------------------------------------------------------------------

struct StreamingState {
    buffer: String,
    cancel: Arc<AtomicBool>,
    /// Most recent tool the model called, shown as a live "drawing…" hint.
    last_tool: Option<String>,
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
    input: Entity<TextField>,
    base_url_field: Entity<TextField>,
    api_key_field: Entity<TextField>,
    model_field: Entity<TextField>,
    notice: Option<String>,
    /// Current reasoning-effort selection, mirrored to settings on send.
    reasoning: ReasoningLevel,
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
    pub fn new(board: WeakEntity<BoardView>, cx: &mut Context<Self>) -> Self {
        let settings = AiSettings::load();

        let input = cx.new(|cx| TextField::new_multiline("给 AI 发送消息…", cx));
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
            base_url_field,
            api_key_field,
            model_field,
            notice: None,
            width: DEFAULT_WIDTH,
            _subscriptions: subscriptions,
        }
    }

    /// Current panel width in px (for the board to offset its chrome).
    pub fn width(&self) -> f32 {
        self.width
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
        let text = self.input.read(cx).text();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Keep settings' reasoning level in sync with the toolbar selection.
        self.settings.reasoning_effort = self.reasoning;
        self.input.update(cx, |f, cx| f.clear(cx));
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
            Some(ChatMessage { role, content }) if role == "user" => content.clone(),
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
                                s.buffer.push_str(&text);
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
                                s.last_tool = Some(name);
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
                if !keep_going {
                    break;
                }
            }
        });
        self.streaming = Some(StreamingState {
            buffer: String::new(),
            cancel,
            last_tool: None,
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
                let msg = ChatMessage::assistant(text);
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
        let mut messages = div().flex().flex_col().gap_2().p_3();
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
            // Live "drawing…" status bubble. Show it whenever a tool is active,
            // even before text streams back, so the user sees the agent working.
            if let Some(tool) = &s.last_tool {
                let label = tool_label(tool);
                let status = div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .text_xs()
                    .text_color(rgb(0x1a5fd7))
                    .child(div().child("✏️"))
                    .child(div().child(format!("正在绘制{label}…")));
                messages = messages.child(status);
            }
            let mut body = s.buffer.clone();
            body.push('▍');
            messages = messages.child(message_bubble("assistant", body, None));
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

        // ---- input area ----
        // A single bordered box containing the multi-line input (which grows to
        // fill) and a bottom toolbar row that sits visually *inside* the input
        // box. The send button is an icon.
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

        // Send/stop icon button.
        let send_icon_color = if streaming_now {
            hsla(0., 0., 0.5, 1.)
        } else {
            hsla(0.58, 1.0, 0.45, 1.)
        };
        let send_btn = div()
            .id("send-btn")
            .w(px(24.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0xefeeec)))
            .child(if streaming_now {
                crate::icons::stop(send_icon_color).into_any_element()
            } else {
                crate::icons::send(send_icon_color).into_any_element()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
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
        // input (which fills the space) and the toolbar pinned to the bottom
        // *inside* the box. This is what makes the send button sit in the
        // input's bottom-right corner.
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
            .child(self.input.clone())
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
