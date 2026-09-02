//! boundless — an Excalidraw-style infinite whiteboard on GPUI,
//! with OpenAI-compatible AI text generation.

// Hide the console window in release builds. The binary is a console-subsystem
// app by default, so double-clicking the exe (or the Start Menu shortcut the
// NSIS installer creates) opens a black terminal alongside the window. Switching
// to the windows subsystem suppresses it. Kept on in debug builds so `cargo run`
// still shows `eprintln!` diagnostics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use boundless::board::{
    ArrowTool, BoardView, BringForward, BringToFront, CancelOp, CheckForUpdates, DeleteSelection,
    DiamondTool, EllipseTool, EraserTool, HandTool, LineTool, OpenScene, PenTool, Quit, RectTool,
    Redo, SaveScene, SelectTool, SendBackward, SendToBack, TextTool, ToggleAi, Undo, ZoomIn,
    ZoomOut, ZoomReset,
};
use gpui::*;
use gpui_component::Root;

/// Install a process-wide panic hook that appends the panic message,
/// location, and backtrace to `~/.boundless/panic.log`. Panics inside GPUI's
/// event loop previously just made the window vanish (debug console closed /
/// release build), leaving nothing to diagnose.
fn install_panic_hook() {
    let log_path = boundless::ai::store::data_dir().join("panic.log");
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".to_string()
        };
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let text = format!(
            "=== panic @ {} (thread: {thread}) ===
payload: {payload}
location: {location}
backtrace:
{}
",
            ts,
            std::backtrace::Backtrace::force_capture()
        );
        eprintln!("{text}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            use std::io::Write;
            let _ = f.write_all(text.as_bytes());
        }
    }));
}

fn main() {
    // Crash diagnostics: any panic on any thread appends to
    // ~/.boundless/panic.log (and stderr) so a "window just vanished" report
    // comes with a location and backtrace instead of nothing.
    install_panic_hook();

    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(|cx: &mut App| {
            // Initialize the gpui-component theme and global state. Must be called
            // early so Input/Button/etc. have their styling ready before the first
            // render.
            gpui_component::init(cx);

            // Set the Dock/application icon at runtime (macOS). No-op on other
            // platforms; Windows embeds its icon via build.rs.
            boundless::platform::set_app_icon();

            // Register the embedded Excalifont handwriting font (Excalidraw's
            // default) so text elements use a hand-drawn look for Latin glyphs;
            // CJK falls back to the system KaiTi font via FontFallbacks.
            let font_bytes: &'static [u8] = include_bytes!("../assets/fonts/Excalifont.ttf");
            if let Err(e) = cx
                .text_system()
                .add_fonts(vec![std::borrow::Cow::Borrowed(font_bytes)])
            {
                eprintln!("failed to load Excalifont: {e}");
            }

            // Tool bindings are single letters, so they must be disabled while an
            // input field (AI panel) has focus. gpui-component's Input uses the
            // "Input" key context, so "!Input" prevents tool shortcuts from firing
            // while typing.
            const CANVAS: &str = "Board && !Input";
            cx.bind_keys([
                // Cmd-Q quits from anywhere (no context), matching the macOS app
                // convention. The menu item "退出 Boundless" picks this binding up
                // automatically for its key-equivalent display.
                KeyBinding::new("cmd-q", Quit, None),
                // Ctrl-Q is the Windows equivalent; the in-app menu bar's "退出"
                // item shows this as its shortcut. Harmless on macOS (which also
                // has cmd-q).
                KeyBinding::new("ctrl-q", Quit, None),
                KeyBinding::new("ctrl-z", Undo, Some("Board")),
                KeyBinding::new("ctrl-shift-z", Redo, Some("Board")),
                KeyBinding::new("ctrl-y", Redo, Some("Board")),
                KeyBinding::new("ctrl-s", SaveScene, Some("Board")),
                KeyBinding::new("ctrl-o", OpenScene, Some("Board")),
                KeyBinding::new("delete", DeleteSelection, Some(CANVAS)),
                KeyBinding::new("backspace", DeleteSelection, Some(CANVAS)),
                KeyBinding::new("escape", CancelOp, Some(CANVAS)),
                KeyBinding::new("ctrl-shift-]", BringToFront, Some(CANVAS)),
                KeyBinding::new("ctrl-shift-[", SendToBack, Some(CANVAS)),
                KeyBinding::new("ctrl-]", BringForward, Some(CANVAS)),
                KeyBinding::new("ctrl-[", SendBackward, Some(CANVAS)),
                KeyBinding::new("ctrl-=", ZoomIn, Some("Board")),
                KeyBinding::new("ctrl--", ZoomOut, Some("Board")),
                KeyBinding::new("ctrl-0", ZoomReset, Some("Board")),
                KeyBinding::new("ctrl-b", ToggleAi, Some("Board")),
                KeyBinding::new("v", SelectTool, Some(CANVAS)),
                KeyBinding::new("h", HandTool, Some(CANVAS)),
                KeyBinding::new("r", RectTool, Some(CANVAS)),
                KeyBinding::new("d", DiamondTool, Some(CANVAS)),
                KeyBinding::new("o", EllipseTool, Some(CANVAS)),
                KeyBinding::new("a", ArrowTool, Some(CANVAS)),
                KeyBinding::new("l", LineTool, Some(CANVAS)),
                KeyBinding::new("p", PenTool, Some(CANVAS)),
                KeyBinding::new("t", TextTool, Some(CANVAS)),
                KeyBinding::new("e", EraserTool, Some(CANVAS)),
            ]);

            // Application main menu. Besides being the conventional place for
            // File/Edit/View commands, an app with an actual menu bar makes macOS
            // slide the menu bar back in when the cursor hits the top of the screen
            // in native fullscreen — without it the menu bar stays hidden (GPUI's
            // platform layer sets no presentation options, so we rely on having
            // menu items for the system's auto-show to kick in).
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.set_menus(vec![
                // First menu is the application menu; macOS renames it to the app
                // name and bolds it. GPUI does not auto-inject About/Quit, so we
                // provide the entries we want here. (No "About" item: GPUI exposes
                // no standard about-panel API.)
                Menu {
                    name: "Boundless".into(),
                    items: vec![MenuItem::action("退出 Boundless", Quit)],
                },
                Menu {
                    name: "文件".into(),
                    items: vec![
                        MenuItem::action("打开场景…", OpenScene),
                        MenuItem::action("保存场景", SaveScene),
                    ],
                },
                Menu {
                    name: "帮助".into(),
                    items: vec![MenuItem::action("检查更新…", CheckForUpdates)],
                },
            ]);

            // Remove leftover `.old` files/dirs from a previous in-place update
            // (applied on the last run before restart). Best-effort, before any
            // window opens.
            boundless::updater::cleanup_old();

            let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Boundless".into()),
                        // Transparent titlebar + full-size content view: the canvas
                        // extends to the top of the window and the traffic lights
                        // float over it, so there's no separate title strip cutting
                        // the toolbar off from the top edge.
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    // boundless is a single-window app, but macOS leaves the
                    // process running in the Dock after the last window closes
                    // (GPUI doesn't implement
                    // applicationShouldTerminateAfterLastWindowClosed). Quit when
                    // the window is about to close so the red traffic light
                    // actually exits the app.
                    window.on_window_should_close(cx, |_, cx| {
                        cx.quit();
                        true
                    });
                    // Wrap the board in a gpui-component Root so the AI panel's
                    // Input/Button components have theme, focus management, and
                    // overlay layers (notifications/dialogs) available.
                    let view: AnyView = cx.new(|cx| BoardView::new(window, cx)).into();
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}
