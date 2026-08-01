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
