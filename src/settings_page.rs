//! App-wide settings page: an independent full-area overlay with a left
//! navigation sidebar. All settings the app accumulates are consolidated
//! here — the AI chat panel no longer hosts its own settings section.
//!
//! The page is owned by `BoardView` (created eagerly, shown via
//! `settings_open`), rendered above all other chrome inside the board's
//! content area, below the Windows menu bar. Saving pushes the new
//! `AiSettings` into the live AI panel through `BoardView::on_settings_saved`.

use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputState};
use gpui_component::{Icon, IconName};

use crate::ai::settings::AiSettings;
use crate::board::BoardView;

/// One entry in the left navigation sidebar. Adding a settings section =
/// a new variant here, a `label`/`icon` arm, and a render arm in `Render`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// OpenAI-compatible endpoint, key, and model for the AI assistant.
    AiModel,
    /// The workspace directory holding boards and per-workspace data.
    Workspace,
}

impl Section {
    /// Sidebar order; future sections append here.
    const ALL: &'static [Section] = &[Section::AiModel, Section::Workspace];

    fn label(self) -> &'static str {
        match self {
            Section::AiModel => "AI 模型",
            Section::Workspace => "工作区",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Section::AiModel => IconName::Bot,
            Section::Workspace => IconName::FolderOpen,
        }
    }
}

/// The settings page entity. Lives for the whole session so the input states
/// (and their IME state) survive open/close cycles; fields are re-synced from
/// disk on every open.
pub struct SettingsPage {
    board: WeakEntity<BoardView>,
    active: Section,
    /// AI 模型 fields, single-line.
    base_url_input: Entity<InputState>,
    api_key_input: Entity<InputState>,
    model_input: Entity<InputState>,
    /// (is_error, text) message shown next to the save button.
    notice: Option<(bool, String)>,
}

impl SettingsPage {
    pub fn new(board: WeakEntity<BoardView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = AiSettings::load();
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
        Self {
            board,
            active: Section::AiModel,
            base_url_input,
            api_key_input,
            model_input,
            notice: None,
        }
    }

    /// Re-sync the fields from disk. Called on every open so a reopened page
    /// shows the last *saved* values, not leftover unsaved edits.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AiSettings::load();
        self.base_url_input.update(cx, |s, cx| {
            s.set_value(settings.base_url.clone(), window, cx)
        });
        self.api_key_input.update(cx, |s, cx| {
            s.set_value(settings.api_key.clone(), window, cx)
        });
        self.model_input.update(cx, |s, cx| {
            s.set_value(settings.model.clone(), window, cx)
        });
        self.notice = None;
        cx.notify();
    }

    /// Persist the fields to disk and push the new settings into the live AI
    /// panel. Blank fields fall back to the defaults (the rule the old
    /// in-panel settings used); `reasoning_effort` is preserved from disk —
    /// the panel's toolbar toggle owns it.
    fn save(&mut self, cx: &mut Context<Self>) {
        let base_url = self.base_url_input.read(cx).value().to_string();
        let api_key = self.api_key_input.read(cx).value().to_string();
        let model = self.model_input.read(cx).value().to_string();
        let mut settings = AiSettings::load();
        settings.base_url = if base_url.trim().is_empty() {
            AiSettings::default().base_url
        } else {
            base_url.trim().to_string()
        };
        settings.api_key = api_key.trim().to_string();
        settings.model = if model.trim().is_empty() {
            AiSettings::default().model
        } else {
            model.trim().to_string()
        };
        match settings.save() {
            Ok(()) => {
                self.notice = Some((false, "设置已保存".to_string()));
                if let Some(board) = self.board.upgrade() {
                    board.update(cx, |board, cx| board.on_settings_saved(cx));
                }
            }
            Err(e) => self.notice = Some((true, format!("保存失败: {e}"))),
        }
        cx.notify();
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Header: page title + close (X) button. The menu bar's gear button
        // toggles the page; X (and Esc outside an input) closes it.
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(48.0))
            .px_4()
            .border_b_1()
            .border_color(rgb(0xeeeeec))
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("设置"),
            )
            .child(
                div()
                    .id("settings-close")
                    .p_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(0x666666))
                    .hover(|s| s.bg(rgb(0xefeeec)).text_color(rgb(0x1e1e1e)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(board) = this.board.upgrade() {
                            board.update(cx, |board, cx| board.close_settings(cx));
                        }
                    }))
                    .child(Icon::new(IconName::Close)),
            );

        // Left navigation sidebar: one row per section.
        let mut sidebar = div()
            .w(px(172.0))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_0p5()
            .p_2()
            .bg(rgb(0xfbfaf9))
            .border_r_1()
            .border_color(rgb(0xe3e2df));
        for (i, section) in Section::ALL.iter().enumerate() {
            let active = *section == self.active;
            let mut row = div()
                .id(("settings-nav", i))
                .h_8()
                .px_2()
                .rounded_md()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_sm()
                .cursor_pointer()
                .child(Icon::new(section.icon()))
                .child(section.label());
            row = if active {
                row.bg(rgb(0xdce8ff)).text_color(rgb(0x1a5fd7))
            } else {
                row.text_color(rgb(0x444444)).hover(|s| s.bg(rgb(0xefeeec)))
            };
            sidebar = sidebar.child(row.on_click(cx.listener(move |this, _, _, cx| {
                this.active = *section;
                this.notice = None;
                cx.notify();
            })));
        }

        // Right content pane for the active section.
        let content = match self.active {
            Section::AiModel => self.render_ai_model(cx),
            Section::Workspace => self.render_workspace(cx),
        };

        div()
            .id("settings-page")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(sidebar)
                    .child(content),
            )
    }
}

impl SettingsPage {
    fn render_ai_model(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let notice: Option<AnyElement> = match &self.notice {
            Some((true, text)) => Some(
                div()
                    .text_sm()
                    .text_color(rgb(0xc92a2a))
                    .child(text.clone())
                    .into_any_element(),
            ),
            Some((false, text)) => Some(
                div()
                    .text_sm()
                    .text_color(rgb(0x2f9e44))
                    .child(text.clone())
                    .into_any_element(),
            ),
            None => None,
        };
        div()
            .id("settings-content")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(32.0))
            .py(px(24.0))
            .child(
                div()
                    .w(px(520.0))
                    .max_w_full()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("AI 模型"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x777777))
                                    .child("配置 OpenAI 兼容接口，AI 助手通过它调用模型、在画布上直接创作。"),
                            ),
                    )
                    .child(field_row("Base URL", &self.base_url_input))
                    .child(field_row("API Key", &self.api_key_input))
                    .child(field_row("模型", &self.model_input))
                    .child(
                        // Footer: save result (left) + save button (right).
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .children(notice)
                            .child(
                                div()
                                    .id("settings-save")
                                    .h_7()
                                    .px_3()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .text_sm()
                                    .cursor_pointer()
                                    .bg(rgb(0x1a5fd7))
                                    .text_color(rgb(0xffffff))
                                    .hover(|s| s.bg(rgb(0x1550b8)))
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx)))
                                    .child("保存"),
                            ),
                    ),
            )
    }

    /// 工作区 section: shows the active workspace root and lets the user
    /// point the app at another directory. Switching routes chat sessions
    /// into `<new-root>/.boundless/`, flushes and closes the current board.
    fn render_workspace(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let root_text = self
            .board
            .upgrade()
            .map(|b| b.read(cx).workspace_root().display().to_string())
            .unwrap_or_default();
        div()
            .id("settings-content")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(32.0))
            .py(px(24.0))
            .child(
                div()
                    .w(px(520.0))
                    .max_w_full()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("工作区"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x777777))
                                    .child("白板文件保存在工作区目录下，对话记录等数据存放于 工作区/.boundless/。切换目录后即在新工作区中创作。"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(rgb(0x777777)).child("当前工作区"))
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .bg(rgb(0xfbfaf9))
                                    .border_1()
                                    .border_color(rgb(0xe3e2df))
                                    .rounded_md()
                                    .text_sm()
                                    .child(div().truncate().child(root_text)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            // Primary: pick any directory as the workspace.
                            .child(
                                div()
                                    .id("workspace-pick")
                                    .h_7()
                                    .px_3()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .text_sm()
                                    .cursor_pointer()
                                    .bg(rgb(0x1a5fd7))
                                    .text_color(rgb(0xffffff))
                                    .hover(|s| s.bg(rgb(0x1550b8)))
                                    .on_click(cx.listener(|_this, _, _, cx| {
                                        // ⚠️ The modal folder picker must run outside this
                                        // entity borrow: its message loop pumps GPUI tasks
                                        // (e.g. the board's autosave ticker) that need the
                                        // App borrow — borrowing re-entrantly panics the
                                        // process. See board.rs `save` for the full story.
                                        cx.spawn(async move |this, cx| {
                                            if let Some(dir) = rfd::FileDialog::new()
                                                .set_title("选择工作区目录")
                                                .pick_folder()
                                            {
                                                this.update(cx, |this, cx| {
                                                    if let Some(board) = this.board.upgrade() {
                                                        board.update(cx, |board, cx| {
                                                            board.switch_workspace(dir, cx)
                                                        });
                                                    }
                                                })
                                                .ok();
                                            }
                                        })
                                        .detach();
                                    }))
                                    .child("更改目录…"),
                            )
                            // Secondary: back to ~/.boundless/workspace.
                            .child(
                                div()
                                    .id("workspace-default")
                                    .h_7()
                                    .px_3()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .text_sm()
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(rgb(0xd6d4d0))
                                    .text_color(rgb(0x444444))
                                    .hover(|s| s.bg(rgb(0xefeeec)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(board) = this.board.upgrade() {
                                            board.update(cx, |board, cx| {
                                                board.switch_workspace(
                                                    crate::workspace::Workspace::default_root(),
                                                    cx,
                                                )
                                            });
                                        }
                                    }))
                                    .child("恢复默认位置"),
                            ),
                    ),
            )
    }
}

/// One labeled settings field: a small gray caption above the input.
fn field_row(label: &'static str, field: &Entity<InputState>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(0x777777)).child(label))
        .child(Input::new(field))
}