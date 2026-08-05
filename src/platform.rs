//! Platform interop helpers.
//!
//! The cross-cutting problem this module solves: GPUI updates the *cached*
//! cursor style whenever the UI re-renders (e.g. when a modifier is pressed),
//! but on Windows the actual on-screen cursor is only re-applied when the OS
//! sends `WM_SETCURSOR`, which normally happens on mouse move. So pressing
//! Ctrl/Shift changes which cursor *should* show, yet the visible cursor lags
//! until the mouse next moves.
//!
//! `refresh_cursor` sets the system cursor directly from a computed
//! [`gpui::CursorStyle`], so it changes instantly and stays in sync with the
//! toolbar highlight (both are derived from the same value in board.rs).

use gpui::{CursorStyle, Window};

/// Apply `style` to the system cursor immediately.
///
/// On Windows this loads the matching standard cursor and calls `SetCursor`.
/// On other platforms it's a no-op (those backends already refresh the cursor
/// on modifier change).
pub fn refresh_cursor(_window: &mut Window, _style: CursorStyle) {
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = set_cursor_windows(_style) {
            eprintln!("refresh_cursor failed: {e}");
        }
    }
}

/// Set the application/Dock icon at runtime.
///
/// On macOS, GPUI doesn't expose an app-icon API and the dev binary (`cargo
/// run`) has no `.app` bundle, so the Dock shows a generic icon. This loads
/// the embedded `logo.png` into an `NSImage` and calls
/// `[NSApp setApplicationIconImage:]`, which makes the Dock show the real icon
/// immediately - for both `cargo run` and the shipped bare binary. No-op on
/// other platforms (Windows embeds its icon via `build.rs`).
pub fn set_app_icon() {
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = set_app_icon_macos() {
            eprintln!("set_app_icon failed: {e}");
        }
    }
}

#[cfg(target_os = "macos")]
fn set_app_icon_macos() -> Result<(), String> {
    use objc::{class, msg_send, sel, sel_impl};
    use objc::runtime::Object;
    use std::ffi::c_void;

    // Embedded at compile time so the binary is self-contained (no external
    // file path needed at runtime).
    const ICON_BYTES: &[u8] = include_bytes!("../logo.png");

    unsafe {
        // NSData wrapping the PNG bytes (autoreleased).
        let data: *mut Object = msg_send![
            class!(NSData),
            dataWithBytes: ICON_BYTES.as_ptr() as *const c_void
            length: ICON_BYTES.len()
        ];
        if data.is_null() {
            return Err("NSData::dataWithBytes returned nil".into());
        }

        // NSImage from the PNG data. NSImage decodes PNG automatically.
        let image: *mut Object = msg_send![class!(NSImage), alloc];
        let image: *mut Object = msg_send![image, initWithData: data];
        if image.is_null() {
            return Err("NSImage::initWithData returned nil".into());
        }

        // [NSApp setApplicationIconImage: image]
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];

        // The app retains the icon image, so release our reference.
        let _: () = msg_send![image, release];
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn set_cursor_windows(style: CursorStyle) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadCursorW, SetCursor, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_SIZENS, IDC_SIZEWE,
    };

    // Map the cursor styles this app actually uses to their Win32 standard
    // cursors. Mirrors GPUI's own load_cursor() mapping (see gpui util.rs) so
    // there's no visible flicker between GPUI's cached value and ours.
    let idc = match style {
        CursorStyle::IBeam => IDC_IBEAM,
        CursorStyle::Crosshair => IDC_CROSS,
        CursorStyle::PointingHand | CursorStyle::DragLink => IDC_HAND,
        CursorStyle::ResizeLeft
        | CursorStyle::ResizeRight
        | CursorStyle::ResizeLeftRight
        | CursorStyle::ResizeColumn => IDC_SIZEWE,
        CursorStyle::ResizeUp
        | CursorStyle::ResizeDown
        | CursorStyle::ResizeUpDown
        | CursorStyle::ResizeRow => IDC_SIZENS,
        // Anything unmapped (Arrow, None, …) falls back to the default arrow.
        _ => IDC_ARROW,
    };

    unsafe {
        let hcursor = LoadCursorW(None, idc).map_err(|e| format!("LoadCursorW failed: {e}"))?;
        SetCursor(Some(hcursor));
    }
    Ok(())
}
