//! Camera: world <-> screen coordinate transforms with pan & zoom.

use gpui::{point, px, Pixels, Point};
use serde::{Deserialize, Serialize};

use crate::scene::WPoint;

pub const MIN_ZOOM: f64 = 0.1;
pub const MAX_ZOOM: f64 = 8.0;

/// The camera describes which world region is visible:
/// `x`/`y` are the world coordinates shown at the canvas element's top-left
/// corner, `zoom` is the scale factor (screen px per world unit).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Camera {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

impl Camera {
    pub fn world_to_screen(&self, p: WPoint, canvas_origin: Point<Pixels>) -> Point<Pixels> {
        point(
            canvas_origin.x + px(((p.x - self.x) * self.zoom) as f32),
            canvas_origin.y + px(((p.y - self.y) * self.zoom) as f32),
        )
    }

    pub fn screen_to_world(&self, p: Point<Pixels>, canvas_origin: Point<Pixels>) -> WPoint {
        WPoint {
            x: (p.x - canvas_origin.x).to_f64() / self.zoom + self.x,
            y: (p.y - canvas_origin.y).to_f64() / self.zoom + self.y,
        }
    }

    /// Scale a world-space length to screen pixels.
    pub fn scale(&self, len: f64) -> Pixels {
        px((len * self.zoom) as f32)
    }

    /// Pan so that the given screen-space delta is applied to the view.
    pub fn pan_by_screen(&mut self, dx: Pixels, dy: Pixels) {
        self.x -= dx.to_f64() / self.zoom;
        self.y -= dy.to_f64() / self.zoom;
    }

    /// Zoom by `factor`, keeping the world point under `screen_focus` fixed.
    /// `screen_focus` is relative to the canvas origin.
    pub fn zoom_at(
        &mut self,
        factor: f64,
        screen_focus: Point<Pixels>,
        canvas_origin: Point<Pixels>,
    ) {
        let focus_world = self.screen_to_world(screen_focus, canvas_origin);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        // Re-anchor so focus_world stays under the cursor.
        self.x = focus_world.x - (screen_focus.x - canvas_origin.x).to_f64() / self.zoom;
        self.y = focus_world.y - (screen_focus.y - canvas_origin.y).to_f64() / self.zoom;
    }

    /// Center the given world bounds in a viewport of `viewport` screen size.
    pub fn zoom_to_fit(&mut self, bounds: crate::scene::WBounds, viewport: gpui::Size<Pixels>) {
        if bounds.w <= 0.0 || bounds.h <= 0.0 {
            return;
        }
        let margin = 1.15;
        let zx = viewport.width.to_f64() / (bounds.w * margin);
        let zy = viewport.height.to_f64() / (bounds.h * margin);
        self.zoom = zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM);
        self.x = bounds.x + bounds.w / 2.0 - viewport.width.to_f64() / self.zoom / 2.0;
        self.y = bounds.y + bounds.h / 2.0 - viewport.height.to_f64() / self.zoom / 2.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_world_screen() {
        let cam = Camera {
            x: 120.0,
            y: -40.0,
            zoom: 2.5,
        };
        let origin = point(px(10.0), px(20.0));
        let w = WPoint { x: 333.3, y: 77.7 };
        let s = cam.world_to_screen(w, origin);
        let back = cam.screen_to_world(s, origin);
        assert!((back.x - w.x).abs() < 1e-3);
        assert!((back.y - w.y).abs() < 1e-3);
    }

    #[test]
    fn zoom_keeps_focus_fixed() {
        let mut cam = Camera {
            x: 50.0,
            y: 50.0,
            zoom: 1.0,
        };
        let origin = point(px(0.0), px(0.0));
        let focus = point(px(200.0), px(150.0));
        let before = cam.screen_to_world(focus, origin);
        cam.zoom_at(1.5, focus, origin);
        let after = cam.screen_to_world(focus, origin);
        assert!((before.x - after.x).abs() < 1e-3);
        assert!((before.y - after.y).abs() < 1e-3);
        assert!((cam.zoom - 1.5).abs() < 1e-9);
    }

    #[test]
    fn zoom_clamped() {
        let mut cam = Camera::default();
        let origin = point(px(0.0), px(0.0));
        for _ in 0..50 {
            cam.zoom_at(2.0, point(px(1.0), px(1.0)), origin);
        }
        assert!(cam.zoom <= MAX_ZOOM);
        for _ in 0..50 {
            cam.zoom_at(0.5, point(px(1.0), px(1.0)), origin);
        }
        assert!(cam.zoom >= MIN_ZOOM);
    }
}
