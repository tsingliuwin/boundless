//! App-wide settings page: an independent full-area overlay built on the
//! gpui-kit `Settings` component (sidebar + search + grouped items).
//! All settings the app accumulates are consolidated here — the AI chat
//! panel no longer hosts its own settings section.
//!
//! The page is owned by `BoardView` (created eagerly, shown via
//! `settings_open`), rendered above all other chrome inside the board's
//! content area, below the Windows menu bar. Saving pushes the new
//! `AiSettings` into the live AI panel through `BoardView::on_settings_saved`.

use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputState};
use gpui_component::setting::{SettingGroup, SettingItem, SettingPage as KitSettingPage, Settings};
use gpui_component::{Icon, IconName};

use crate::ai::settings::AiSettings;
use crate::board::BoardView;

/// The settings page entity. Lives for the whole session so the input states
/// (and their IME state) survive open/close cycles; fields are re-synced from
/// disk on every open.
pub struct SettingsPage {
    board: WeakEntity<BoardView>,
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
        self.model_input
            .update(cx, |s, cx| s.set_value(settings.model.clone(), window, cx));
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
        let theme = crate::theme::chrome(cx);
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
            .border_color(theme.border)
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
                    .text_color(theme.muted_foreground)
                    .hover(|s| s.bg(theme.muted).text_color(theme.foreground))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(board) = this.board.upgrade() {
                            board.update(cx, |board, cx| board.close_settings(cx));
                        }
                    }))
                    .child(Icon::new(IconName::Close)),
            );

        // ---- AI 模型 page ----
        let base_url_input = self.base_url_input.clone();
        let api_key_input = self.api_key_input.clone();
        let model_input = self.model_input.clone();
        let weak_save = cx.weak_entity();
        let notice = self.notice.clone();

        let save_row = SettingItem::render(move |_, _, _cx| {
            let weak_save = weak_save.clone();
            let notice_el: AnyElement = match &notice {
                Some((true, text)) => div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(text.clone())
                    .into_any_element(),
                Some((false, text)) => div()
                    .text_sm()
                    .text_color(theme.success)
                    .child(text.clone())
                    .into_any_element(),
                None => div().into_any_element(),
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(notice_el)
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
                        .bg(theme.primary)
                        .text_color(theme.popover)
                        .hover(|s| s.bg(theme.primary_hover))
                        .on_click(move |_, _, cx: &mut App| {
                            weak_save.update(cx, |this, cx| this.save(cx)).ok();
                        })
                        .child("保存"),
                )
                .into_any_element()
        });

        let ai_group = SettingGroup::new()
            .item(
                SettingItem::render(move |_, _, cx| {
                    field_row("Base URL", &base_url_input, cx).into_any_element()
                })
                .keywords(["Base URL", "接口地址"]),
            )
            .item(
                SettingItem::render(move |_, _, cx| {
                    field_row("API Key", &api_key_input, cx).into_any_element()
                })
                .keywords(["API Key", "密钥"]),
            )
            .item(
                SettingItem::render(move |_, _, cx| {
                    field_row("模型", &model_input, cx).into_any_element()
                })
                .keywords(["模型", "model"]),
            )
            .item(save_row.keywords(["保存", "save"]));

        let ai_page = KitSettingPage::new("AI 模型")
            .icon(IconName::Bot)
            .description("配置 OpenAI 兼容接口，AI无界通过它调用模型、在画布上直接创作。")
            .default_open(true)
            .group(ai_group);

        // ---- 工作区 page ----
        let root_text = self
            .board
            .upgrade()
            .map(|b| b.read(cx).workspace_root().display().to_string())
            .unwrap_or_default();
        let weak_ws = cx.weak_entity();

        let ws_group = SettingGroup::new().item(
            SettingItem::render(move |_, _, _cx| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("当前工作区"),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .bg(theme.background)
                                    .border_1()
                                    .border_color(theme.border)
                                    .rounded_md()
                                    .text_sm()
                                    .child(div().truncate().child(root_text.clone())),
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
                                    .bg(theme.primary)
                                    .text_color(theme.popover)
                                    .hover(|s| s.bg(theme.primary_hover))
                                    .on_click({
                                        let weak_ws = weak_ws.clone();
                                        move |_, _, cx: &mut App| {
                                            // ⚠️ The modal folder picker must run outside
                                            // any entity borrow: its message loop pumps
                                            // GPUI tasks (e.g. the board's autosave
                                            // ticker) that need the App borrow —
                                            // borrowing re-entrantly panics the process.
                                            let weak_pick = weak_ws.clone();
                                            cx.spawn(async move |cx| {
                                                if let Some(dir) = rfd::FileDialog::new()
                                                    .set_title("选择工作区目录")
                                                    .pick_folder()
                                                {
                                                    weak_pick
                                                        .update(cx, |this: &mut SettingsPage, cx| {
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
                                        }
                                    })
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
                                    .border_color(theme.border)
                                    .text_color(theme.foreground)
                                    .hover(|s| s.bg(theme.muted))
                                    .on_click({
                                        let weak_ws = weak_ws.clone();
                                        move |_, _, cx: &mut App| {
                                            let weak_ws = weak_ws.clone();
                                            weak_ws
                                                .update(cx, |this: &mut SettingsPage, cx| {
                                                    if let Some(board) = this.board.upgrade() {
                                                        board.update(cx, |board, cx| {
                                                            board.switch_workspace(
                                                                crate::workspace::Workspace::default_root(),
                                                                cx,
                                                            )
                                                        });
                                                    }
                                                })
                                                .ok();
                                        }
                                    })
                                    .child("恢复默认位置"),
                            ),
                    )
                    .into_any_element()
            })
            .keywords(["工作区", "目录", "workspace"]),
        );

        let ws_page = KitSettingPage::new("工作区")
            .icon(IconName::FolderOpen)
            .description(
                "白板文件保存在工作区目录下，对话记录等数据存放于 工作区/.boundless/。切换目录后即在新工作区中创作。",
            )
            .group(ws_group);

        // ---- 外观 page: light / dark / system theme pick. ----
        let theme_page = KitSettingPage::new("外观")
            .icon(IconName::Sun)
            .description("应用界面配色：浅色纸白 / 深色墨黑 / 跟随系统。")
            .group(
                SettingGroup::new().item(
                    SettingItem::render(move |_, _, _cx| {
                        let current = crate::theme::current_pref();
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .children(
                                [
                                    crate::theme::ThemePref::Light,
                                    crate::theme::ThemePref::Dark,
                                    crate::theme::ThemePref::System,
                                ]
                                .iter()
                                .enumerate()
                                .map(|(i, pref)| {
                                    let active = *pref == current;
                                    div()
                                        .id(("theme-mode", i))
                                        .h_7()
                                        .px_3()
                                        .rounded_md()
                                        .flex()
                                        .items_center()
                                        .text_sm()
                                        .cursor_pointer()
                                        .when(active, |d| {
                                            d.bg(theme.primary).text_color(theme.popover)
                                        })
                                        .when(!active, |d| {
                                            d.border_1()
                                                .border_color(theme.border)
                                                .text_color(theme.foreground)
                                                .hover(|s| s.bg(theme.muted))
                                        })
                                        .on_click(move |_, _, cx: &mut App| {
                                            crate::theme::set_pref(*pref, cx);
                                        })
                                        .child(pref.label())
                                }),
                            )
                            .into_any_element()
                    })
                    .keywords(["主题", "外观", "深色", "dark", "theme"]),
                ),
            );

        let settings = Settings::new("app-settings").pages([theme_page, ai_page, ws_page]);

        div()
            .id("settings-page")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(theme.popover)
            .child(header)
            .child(div().flex_1().min_h_0().child(settings))
    }
}

/// One labeled settings field: a small gray caption above the input.
fn field_row(label: &'static str, field: &Entity<InputState>, cx: &App) -> Div {
    let theme = crate::theme::chrome(cx);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(Input::new(field))
}
