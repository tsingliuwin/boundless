//! boundless — an Excalidraw-style infinite whiteboard on GPUI,
//! with OpenAI-compatible AI text generation.

mod ai;
mod board;
mod camera;
mod history;
mod icons;
mod render;
mod scene;
mod text;
mod tools;

use board::{
    ArrowTool, BoardView, CancelOp, DeleteSelection, DiamondTool, EllipseTool, EraserTool,
    HandTool, LineTool, OpenScene, PenTool, RectTool, Redo, SaveScene, SelectTool, TextTool,
    ToggleAi, Undo, ZoomIn, ZoomOut, ZoomReset,
};
use gpui::*;

fn main() {
    Application::new().run(|cx: &mut App| {
        // Register the embedded Patrick Hand handwriting font so text elements
        // can use a hand-drawn look (Latin glyphs; CJK falls back to system UI).
        let font_bytes: &'static [u8] = include_bytes!("../assets/fonts/PatrickHand.ttf");
        if let Err(e) = cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(font_bytes)]) {
            eprintln!("failed to load Patrick Hand font: {e}");
        }

        // Tool bindings are single letters, so they must be disabled while a
        // text field (AI panel) has focus: the "Board && !TextInput"
        // predicate evaluates against the whole dispatch path.
        const CANVAS: &str = "Board && !TextInput";
        cx.bind_keys([
            KeyBinding::new("ctrl-z", Undo, Some("Board")),
            KeyBinding::new("ctrl-shift-z", Redo, Some("Board")),
            KeyBinding::new("ctrl-y", Redo, Some("Board")),
            KeyBinding::new("ctrl-s", SaveScene, Some("Board")),
            KeyBinding::new("ctrl-o", OpenScene, Some("Board")),
            KeyBinding::new("delete", DeleteSelection, Some(CANVAS)),
            KeyBinding::new("backspace", DeleteSelection, Some(CANVAS)),
            KeyBinding::new("escape", CancelOp, Some(CANVAS)),
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
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| BoardView::new(window, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
