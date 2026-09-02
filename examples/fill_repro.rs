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

                    // 1. ellipse via LINE segments, alpha 0.5 (the exact
                    //    mechanics of paint_world_geom's fill: flatten → fill).
                    let mut b = PathBuilder::fill();
                    let n = 64;
                    for i in 0..=n {
                        let t = i as f32 / n as f32 * std::f32::consts::TAU;
                        let p: Point<Pixels> =
                            point(px(cx0 + rx * t.cos()), px(cy0 + ry * t.sin()));
                        if i == 0 {
                            b.move_to(p);
                        } else {
                            b.line_to(p);
                        }
                    }
                    b.close();
                    match b.build() {
                        Ok(path) => {
                            window.paint_path(path, gpui::hsla(0.35, 0.2, 0.35, 0.5));
                        }
                        Err(e) => eprintln!("line-seg ellipse build failed: {e}"),
                    }

                    // 2. ellipse via CURVE segments, alpha 0.8.
                    let cy1 = oy + h * 0.72;
                    let mut b = PathBuilder::fill();
                    b.move_to(point(px(cx0 - rx), px(cy1)));
                    let n = 8;
                    for i in 1..=n {
                        let t0 = (i - 1) as f32 / n as f32 * std::f32::consts::TAU;
                        let t1 = i as f32 / n as f32 * std::f32::consts::TAU;
                        let tm = (t0 + t1) / 2.0;
                        b.curve_to(
                            point(px(cx0 + rx * tm.cos()), px(cy1 + ry * tm.sin())),
                            point(px(cx0 + rx * t1.cos()), px(cy1 + ry * t1.sin())),
                        );
                    }
                    b.close();
                    match b.build() {
                        Ok(path) => {
                            window.paint_path(path, gpui::hsla(0.0, 0.6, 0.45, 0.8));
                        }
                        Err(e) => eprintln!("curve ellipse build failed: {e}"),
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
