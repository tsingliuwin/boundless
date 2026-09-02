//! The ink pipeline: handwriting-quality freehand strokes.
//!
//! Everything in this module is pure data + math with no rendering or
//! platform dependency, so each piece is independently unit-testable and
//! the feel of the pen can be tuned by editing constants in
//! [`pressure`] without touching capture or rendering code.
//!
//! Layering (capture → geometry):
//!
//! * [`collector::InkCollector`] — the capture loop: decimation, EMA
//!   position smoothing, and per-sample pressure — *hardware* stylus
//!   pressure (Windows Ink WM_POINTER hook, `crate::platform`) when a pen
//!   is live, else a *simulated* pressure derived from pointer velocity
//!   (slow = thick, fast = thin — works with a plain mouse). Its output
//!   feeds both the live draft preview and the committed element, so what
//!   you see while drawing is exactly what you get.
//! * [`outline::ribbon_outline`] — the fillable outline around a
//!   variable-width centerline, consumed by the renderer.
//! * [`pressure`] — width models: the velocity model and the hardware
//!   model ([`pressure::width_from_hardware_pressure`], fed by
//!   `crate::platform::latest_pen_sample` through the board).
//!
//! Widths travel through the pipeline as *ratios* of the stroke's base
//! width (`style.stroke_width`): restyling the base width later rescales
//! the whole stroke, and the serialized form stays compact.

pub mod collector;
pub mod outline;
pub mod pressure;
pub mod smooth;

pub use collector::InkCollector;
pub use outline::{dot_outline, ribbon_outline};
