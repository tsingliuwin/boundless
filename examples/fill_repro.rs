//! Minimal repro: does gpui 0.2.2's `PathBuilder::fill()` + `paint_path`
//! render a semi-transparent filled path on Windows? Same mechanics as
//! `paint_world_geom`'s FillPath branch.

use gpui::{
    div, fill, hsla, point, prelude::*, px, rgb, size, App, Application, Bounds, IntoElement,
    PathBuilder, Pixels, Point, Size, Window, WindowBounds, WindowOptions,
};

struct FillRepro;

impl gpui::Render for FillRepro {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let _ = cx;
        div()
            .size_full()
            .bg(gpui::rgb(0xf5efdc))
            .child(gpui::canvas(
                |_bounds, _window, _cx| (),
                move |bounds, _, window, _cx| {
                    let ox = f32::from(bounds.origin.x);
                    let oy = f32::from(bounds.origin.y);
                    let w = f32::from(bounds.size.width);
                    let h = f32::from(bounds.size.height);
                    let cx0 = ox + w * 0.5;
                    let cy0 = oy + h * 0.35;
                    let rx = w * 0.28;
                    let ry = h * 0.22;

                    // 1. ellipse via LINE segments, alpha 1.0 — drawn TWICE:
                    //    left = clockwise, right = REVERSED. If the GPU culls
                    //    one winding, only one will show.
                    let n = 64;
                    for (half, reverse) in [(0.0, false), (0.55, true)] {
                        let cc = ox + w * (0.25 + half);
                        let ccy = oy + h * 0.4;
                        let rrx = rx * 0.6;
                        let rry = ry * 0.6;
                        let mut pts: Vec<Point<Pixels>> = Vec::new();
                        for i in 0..=n {
                            let t = i as f32 / n as f32 * std::f32::consts::TAU;
                            pts.push(point(
                                px(cc + rrx * t.cos()),
                                px(ccy + rry * t.sin()),
                            ));
                        }
                        if reverse {
                            pts.reverse();
                        }
                        let mut b = PathBuilder::fill();
                        for (i, p) in pts.iter().enumerate() {
                            if i == 0 {
                                b.move_to(*p);
                            } else {
                                b.line_to(*p);
                            }
                        }
                        b.close();
                        match b.build() {
                            Ok(path) => {
                                window.paint_path(path, gpui::hsla(0.35, 0.2, 0.35, 1.0));
                            }
                            Err(e) => eprintln!("fill build failed: {e}"),
                        }
                    }

                    // 3. quad reference (known-good rendering path).
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: point(px(ox + 40.0), px(oy + 40.0)),
                            size: size(px(90.0), px(90.0)),
                        },
                        gpui::rgb(0x204f16),
                    ));
                },
            ))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(
            None,
            Size {
                width: px(800.0),
                height: px(600.0),
            },
            cx,
        );
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| FillRepro),
        )
        .unwrap();
        cx.activate(true);
    });
}
