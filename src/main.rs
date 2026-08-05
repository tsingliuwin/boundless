//! boundless — an Excalidraw-style infinite whiteboard on GPUI,
//! with OpenAI-compatible AI text generation.

mod ai;
mod board;
mod camera;
mod history;
mod icons;
mod platform;
mod render;
mod scene;
mod text;
mod tools;

use board::{
    ArrowTool, BoardView, BringForward, BringToFront, CancelOp, DeleteSelection, DiamondTool,
    EllipseTool, EraserTool, HandTool, LineTool, OpenScene, PenTool, RectTool, Redo, SaveScene,
    SelectTool, SendBackward, SendToBack, TextTool, ToggleAi, Undo, ZoomIn, ZoomOut, ZoomReset,
};
use gpui::*;
use gpui_component::Root;

fn main() {
    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(|cx: &mut App| {
        // Initialize the gpui-component theme and global state. Must be called
        // early so Input/Button/etc. have their styling ready before the first
        // render.
        gpui_component::init(cx);

        // Register the embedded Excalifont handwriting font (Excalidraw's
        // default) so text elements use a hand-drawn look for Latin glyphs;
        // CJK falls back to the system KaiTi font via FontFallbacks.
        let font_bytes: &'static [u8] = include_bytes!("../assets/fonts/Excalifont.ttf");
        if let Err(e) = cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(font_bytes)]) {
            eprintln!("failed to load Excalifont: {e}");
        }

        // Tool bindings are single letters, so they must be disabled while an
        // input field (AI panel) has focus. gpui-component's Input uses the
        // "Input" key context, so "!Input" prevents tool shortcuts from firing
        // while typing.
        const CANVAS: &str = "Board && !Input";
        cx.bind_keys([
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
