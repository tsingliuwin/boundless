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
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

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
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
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

/// Toggle the window between maximized and restored.
///
/// GPUI's `Window::zoom_window()` on Windows only ever calls
/// `ShowWindow(SW_MAXIMIZE)` - it doesn't toggle, so a second click on a
/// "restore" button would just re-maximize. This does a real toggle via the
/// HWND: `IsZoomed` -> `SW_RESTORE`, else `SW_MAXIMIZE`. No-op on non-Windows
/// (the in-app menu bar that calls this is Windows-only anyway).
pub fn toggle_maximize(window: &Window) {
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = toggle_maximize_windows(window) {
            eprintln!("toggle_maximize failed: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn toggle_maximize_windows(window: &Window) -> Result<(), String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindowAsync, SW_MAXIMIZE, SW_RESTORE};

    // gpui::Window has its own inherent `window_handle()` (returning an
    // AnyWindowHandle) that shadows the `HasWindowHandle` trait method, so call
    // the trait method via UFCS to get the raw window handle.
    let raw = HasWindowHandle::window_handle(window)
        .map_err(|e| format!("window_handle: {e}"))?
        .as_raw();
    let hwnd = match raw {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut core::ffi::c_void),
        _ => return Err("not a Win32 window".into()),
    };
    // `window.is_maximized()` is a live `IsZoomed` query (and also rules out
    // fullscreen), so it matches the maximize/restore icon we render. Toggle:
    // maximized -> restore, otherwise -> maximize. ShowWindowAsync returns the
    // previous visibility (BOOL); we don't need it.
    let cmd = if window.is_maximized() {
        SW_RESTORE
    } else {
        SW_MAXIMIZE
    };
    unsafe {
        let _ = ShowWindowAsync(hwnd, cmd);
    }
    Ok(())
}

/// Start a native window drag from a client-area mouse-down.
///
/// GPUI's `start_window_move` is a no-op on Windows, and `window_control_area(
/// Drag)` only drags when `WM_NCHITTEST` returns `HTCAPTION` - which needs a
/// current `mouse_hit_test`, but that's often stale by one frame at click time,
/// so the hit falls back to `HTCLIENT` and the drag never starts. Instead we
/// release the mouse capture and `SendMessage(WM_NCLBUTTONDOWN, HTCAPTION)`,
/// which makes Windows enter its caption drag modal loop directly. This is the
/// standard trick for custom-titlebar windows and doesn't depend on hit-testing.
pub fn start_window_drag(window: &Window) {
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = start_window_drag_windows(window) {
            eprintln!("start_window_drag failed: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn start_window_drag_windows(window: &Window) -> Result<(), String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, PostMessageW, HTCAPTION, WM_NCLBUTTONDOWN,
    };

    let raw = HasWindowHandle::window_handle(window)
        .map_err(|e| format!("window_handle: {e}"))?
        .as_raw();
    let hwnd = match raw {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut core::ffi::c_void),
        _ => return Err("not a Win32 window".into()),
    };
    unsafe {
        // ReleaseCapture so the in-flight button-down doesn't hold the mouse,
        // then synthesize a caption (HTCAPTION) non-client button-down. Windows
        // runs its drag modal loop inside that message; the GPUI input
        // callback is already taken by the outer client mouse-down, so the
        // WM_NCLBUTTONDOWN skips element dispatch and hits DefWindowProcW,
        // which starts the drag.
        //
        // POST, don't SEND: SendMessageW would enter the modal move loop
        // synchronously *inside this mouse-down handler*, so agent-stream
        // updates (a main-thread refresh every ~50ms while the AI is
        // generating) execute re-entrantly mid-modal-loop — which crashed the
        // app. Posting defers the modal loop until this handler has returned.
        let _ = ReleaseCapture();
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // lparam = screen-space cursor (low word = x, high word = y).
        let lparam = LPARAM((((pt.y as u32) << 16) | (pt.x as u32 & 0xFFFF)) as isize);
        let _ = PostMessageW(
            Some(hwnd),
            WM_NCLBUTTONDOWN,
            WPARAM(HTCAPTION as usize),
            lparam,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stylus pressure (Windows Ink / WM_POINTER)
//
// GPUI 0.2.2 has no pressure in its event model and its Windows backend only
// handles legacy WM_MOUSE* messages — when a stylus draws, the digitizer's
// WM_POINTER packets are simply ignored and everything arrives as synthetic
// mouse input with the pressure thrown away.
//
// This hook recovers that data without touching gpui: the gpui window is
// *subclassed* and the pointer messages are only observed (pressure stashed
// into lock-free atomics), never consumed. Every packet is forwarded to the
// original proc, whose DefWindowProc translates pointer input into the legacy
// mouse messages gpui's event loop is built on — so gpui's input flow is
// byte-for-byte unchanged and the ink collector (crate::ink) reads the
// stashed pressure as a side channel while it captures mouse samples.
//
// The classifier and freshness logic are plain functions over primitive
// values so they unit-test on every platform; only the message pump itself
// is Windows-only.
// ---------------------------------------------------------------------------

/// One stylus sample recovered from the WM_POINTER stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenSample {
    /// Digitizer pressure normalized to `0..=1` (raw packets are 0..1024).
    pub pressure: f32,
    /// True when the eraser end of the stylus is in use.
    pub eraser: bool,
}

/// Raw `POINTER_INPUT_TYPE` for a stylus. Mirrors the windows crate's
/// `PT_PEN`; a plain constant keeps the classifier testable on all platforms.
const PT_PEN_RAW: i32 = 3;
/// Raw `PEN_FLAG_ERASER` bit, mirroring the windows crate's constant.
const PEN_FLAG_ERASER_RAW: u32 = 4;
/// Windows defines stylus pressure as 0..1024 (full press = 1024).
const PEN_PRESSURE_MAX: f32 = 1024.0;
/// Max age of a pen packet for it to count as current input. While a stylus
/// draws, WM_POINTERUPDATE packets stream continuously so the slot stays
/// fresh; a mouse-only session never produces packets, so samples age out
/// and strokes fall back to velocity-simulated pressure.
const PEN_FRESH_MS: u64 = 200;

/// Last observed stylus packet, as lock-free side-channel state written by
/// the WM_POINTER subclass proc (UI thread) and read by the ink collector.
static PEN_PRESSURE_BITS: AtomicU32 = AtomicU32::new(0.0f32.to_bits());
static PEN_FLAGS: AtomicU32 = AtomicU32::new(0);
static PEN_PRESENT: AtomicU8 = AtomicU8::new(0);
static PEN_LAST_SEEN_MS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock milliseconds for packet freshness stamps.
fn now_ms() -> u64 {
    // Wall-clock (not a lazy process-start anchor): tests must be able to
    // construct a timestamp older than the freshness window even when the
    // process has just started.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Map one raw WM_POINTER pen packet to a normalized [`PenSample`]. Returns
/// `None` for non-pen pointers (mouse/touch packets carry no pen pressure).
fn pen_sample_from(pointer_type: i32, pressure_raw: u32, pen_flags: u32) -> Option<PenSample> {
    if pointer_type != PT_PEN_RAW {
        return None;
    }
    Some(PenSample {
        pressure: (pressure_raw as f32 / PEN_PRESSURE_MAX).clamp(0.0, 1.0),
        eraser: pen_flags & PEN_FLAG_ERASER_RAW != 0,
    })
}

/// Latest stylus sample, when a pen has produced packets recently. The
/// caller (the ink collector, via board.rs) uses this per pointer event to
/// switch a stroke from velocity-simulated to hardware pressure.
pub fn latest_pen_sample() -> Option<PenSample> {
    if PEN_PRESENT.load(Ordering::Relaxed) != 1 {
        return None;
    }
    let age = now_ms().saturating_sub(PEN_LAST_SEEN_MS.load(Ordering::Relaxed));
    if age > PEN_FRESH_MS {
        return None;
    }
    Some(PenSample {
        pressure: f32::from_bits(PEN_PRESSURE_BITS.load(Ordering::Relaxed)),
        eraser: PEN_FLAGS.load(Ordering::Relaxed) & PEN_FLAG_ERASER_RAW != 0,
    })
}

/// Install the WM_POINTER observation hook on the gpui window. Idempotent;
/// no-op on non-Windows. Must run on the window's thread (BoardView::new
/// qualifies).
pub fn init_pen_input(_window: &Window) {
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = init_pen_input_windows(_window) {
            eprintln!("init_pen_input failed: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn init_pen_input_windows(window: &Window) -> Result<(), String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;

    static INSTALLED: AtomicU8 = AtomicU8::new(0);
    if INSTALLED.load(Ordering::SeqCst) == 1 {
        return Ok(());
    }

    // Same HWND extraction as toggle_maximize (gpui::Window's inherent
    // window_handle() shadows the trait method, hence the UFCS).
    let raw = HasWindowHandle::window_handle(window)
        .map_err(|e| format!("window_handle: {e}"))?
        .as_raw();
    let hwnd = match raw {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut core::ffi::c_void),
        _ => return Err("not a Win32 window".into()),
    };
    unsafe {
        // Must be called from the window's thread — we are (main/UI thread).
        SetWindowSubclass(hwnd, Some(pen_subclass_proc), PEN_SUBCLASS_ID, 0)
            .ok()
            .map_err(|e| format!("SetWindowSubclass failed: {e}"))?;
    }
    INSTALLED.store(1, Ordering::SeqCst);
    Ok(())
}

/// Arbitrary non-zero subclass id (namespace: this window has no other
/// subclasses).
#[cfg(target_os = "windows")]
const PEN_SUBCLASS_ID: usize = 0xB0E55;

/// Observe-and-forward: record pen pressure from pointer packets, then hand
/// EVERY message to the next proc in the chain. Forwarding is the load-bearing
/// part — if we returned without it (or consumed WM_POINTER*), DefWindowProc
/// would never translate pointer input into the legacy mouse messages gpui
/// dispatches on, and all input would go dead.
#[cfg(target_os = "windows")]
unsafe extern "system" fn pen_subclass_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _uid_subclass: usize,
    _ref_data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Input::Pointer::{GetPointerInfo, GetPointerPenInfo, POINTER_INFO};
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{
        PT_PEN, WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE,
    };

    if matches!(msg, WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP) {
        // GET_POINTERID_WPARAM: pointer id lives in the low word of wParam.
        let pointer_id = (wparam.0 & 0xFFFF) as u32;
        let mut info = POINTER_INFO::default();
        if unsafe { GetPointerInfo(pointer_id, &mut info) }.is_ok() && info.pointerType == PT_PEN {
            let mut pen = windows::Win32::UI::Input::Pointer::POINTER_PEN_INFO::default();
            if unsafe { GetPointerPenInfo(pointer_id, &mut pen) }.is_ok() {
                if let Some(s) = pen_sample_from(info.pointerType.0, pen.pressure, pen.penFlags) {
                    PEN_PRESSURE_BITS.store(s.pressure.to_bits(), Ordering::Relaxed);
                    PEN_FLAGS.store(pen.penFlags, Ordering::Relaxed);
                    PEN_PRESENT.store(1, Ordering::Relaxed);
                    PEN_LAST_SEEN_MS.store(now_ms(), Ordering::Relaxed);
                }
            }
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pen_classifier_rejects_non_pen_pointers() {
        // PT_MOUSE = 4, PT_TOUCH = 2 — no pen pressure on those packets.
        assert!(pen_sample_from(4, 512, 0).is_none());
        assert!(pen_sample_from(2, 512, 0).is_none());
    }

    #[test]
    fn pen_classifier_normalizes_pressure() {
        let full = pen_sample_from(PT_PEN_RAW, 1024, 0).unwrap();
        assert!((full.pressure - 1.0).abs() < 1e-6);
        assert!(!full.eraser);

        let half = pen_sample_from(PT_PEN_RAW, 512, 0).unwrap();
        assert!((half.pressure - 0.5).abs() < 1e-6);

        let zero = pen_sample_from(PT_PEN_RAW, 0, 0).unwrap();
        assert!((zero.pressure).abs() < 1e-6);
    }

    #[test]
    fn pen_classifier_clamps_out_of_range_pressure() {
        let s = pen_sample_from(PT_PEN_RAW, 999_999, 0).unwrap();
        assert!((s.pressure - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pen_classifier_detects_eraser_tip() {
        // PEN_FLAG_ERASER (raw 4) set → eraser; other flags (barrel = 1) don't.
        assert!(
            pen_sample_from(PT_PEN_RAW, 100, PEN_FLAG_ERASER_RAW)
                .unwrap()
                .eraser
        );
        assert!(!pen_sample_from(PT_PEN_RAW, 100, 1).unwrap().eraser);
    }

    #[test]
    fn pen_slot_never_reports_without_packets() {
        // Fresh timestamp but the "packets flowing" gate off → None.
        PEN_PRESENT.store(0, Ordering::Relaxed);
        PEN_LAST_SEEN_MS.store(now_ms(), Ordering::Relaxed);
        assert!(latest_pen_sample().is_none());
    }

    #[test]
    fn pen_slot_expires_stale_packets() {
        PEN_PRESENT.store(1, Ordering::Relaxed);
        PEN_LAST_SEEN_MS.store(now_ms().saturating_sub(PEN_FRESH_MS + 1), Ordering::Relaxed);
        assert!(latest_pen_sample().is_none());

        // Back in date → reported again.
        PEN_LAST_SEEN_MS.store(now_ms(), Ordering::Relaxed);
        let s = latest_pen_sample().unwrap();
        assert!((0.0..=1.0).contains(&s.pressure));
        PEN_PRESENT.store(0, Ordering::Relaxed);
    }
}
