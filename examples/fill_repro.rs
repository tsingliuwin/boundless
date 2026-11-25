//! Probe for gpui-pre's path rendering on Windows, and for what it costs per
//! frame. Same mechanics as `paint_world_geom` (PathBuilder + `paint_path`).
//!
//! Run with `cargo run --example fill_repro -- [mode] [--show] [--break]`.
//!
//! Nothing shows on the desktop. The probe opens a hidden window, draws into
//! it by hand, reads the pixels back from the GPU (`Window::render_to_image`)
//! and checks them itself: exit code 0 when every check passes, 1 when any
//! fails, 2 on a usage error, 3 when the window never became usable. It takes
//! a couple of seconds and needs no screenshot and no human eye.
//! `FILL_REPRO_DUMP=<file.png>` keeps the read-back frame, to look at a
//! failure. `--show` instead opens the window and leaves it open for a human,
//! with no checks and no exit code.
//!
//! Modes:
//!
//! - `plain` (default): the original repro — two EvenOdd-filled ellipses (one
//!   clockwise, one reversed) and a quad reference, in an 800x600 window.
//! - `matrix`: a 4x2 grid comparing fill rules, alpha, gradient and a stroke,
//!   each cell captioned:
//!
//!   ```text
//!     [quad]   EO cw      EO rev     NZ cw      NZ rev
//!              NZ a=0.5   NZ grad    EO a=0.5   stroke 5px
//!   ```
//!
//!   EO = FillRule::EvenOdd (PathBuilder::fill() default), NZ = FillRule::NonZero
//!   (what the pen ribbon and ink dot use).
//! - `wash`: the 彩 fill as the board paints it — through a `WashCache` and
//!   its worker threads, one frame at a time, until the washes are at hand.
//!   Read against a frame that shows the same elements' flat glazes instead,
//!   it checks that each wash is there, bleeds past the glazes' outline and
//!   fades in over that stretch, and has texture where they are flat; that a
//!   restyled element's wash follows; and that the replaced texture was
//!   dropped.
//! - `app`: the app itself — a hidden window whose root is the real
//!   `BoardView` (under `Root`, as in `src/main.rs`), on a throwaway home
//!   (`BOUNDLESS_HOME`, never the user's) and a board of ours with two 彩
//!   shapes at a camera that is off the origin and not 1:1. Checks that the
//!   rectangle's wash sits on its own outline — the outline is placed by the
//!   board's camera and canvas origin, the wash by its own mapping of them —
//!   to within its bleed and wobble (about 4 world units), with the sidebar
//!   closed and again with it open, which moves the canvas; and that both
//!   washes are there, with texture. `app --show` is that app for a human.
//! - `bench`: CPU time `paint_path` spends per frame for a few synthetic path
//!   counts/shapes; prints one line per case to stderr.
//! - `board <file.boundless>`: the same for a real board — every element's
//!   paint list at the saved camera, culled to a 1440x900 window like the
//!   app. Its 彩 washes are rasterized at once, where the board makes them in
//!   the background once their elements hold still.
//!
//! `--break` skips painting the first fill: a check that cannot fail proves
//! nothing, so this proves that these can (expected exit code: 1).
//!
//! The bench modes exist because gpui-pre 0.3.14 re-bins each painted path
//! into tiles on every `paint_path` call (`PaintedPath::new`), where 0.3.12
//! tessellated once at build time. The board keeps its paths in a render cache
//! and repaints them every frame, so that cost is paid per frame.
//!
//! # Why this file exists, and why it was wrong for months
//!
//! The board avoided `PathBuilder::fill()` for every fill style because this
//! probe "showed" filled paths never reach the screen on Windows (quad visible,
//! fills missing). That result was a bug in the probe: its canvas had no size,
//! and an unsized canvas collapses to zero height inside a block parent, so
//! every ellipse computed from the canvas bounds degenerated into a zero-area
//! line. With the canvas sized (2026-11-24, gpui-pre 0.3.14), every fill
//! renders. Hence the rules here: the canvas size is checked where the frame
//! is judged, nothing is trusted because it looked right — the checks read
//! real pixels — and `--break` shows they can fail.

use boundless::board::BoardView;
use boundless::camera::Camera;
use boundless::render::rough::{paint_world_geom, world_geometry, PaintOp, WorldGeom};
use boundless::render::wash;
use boundless::render::wash::WashDraw;
use boundless::scene::{
    Element, ElementKind, ElementStyle, FillStyle, Scene, SceneFile, WBounds, WPoint,
};
use gpui::{
    div, point, prelude::*, px, rgb, size, AnyWindowHandle, App, Background, Bounds, Corners,
    Entity, FillOptions, FillRule, IntoElement, Menu, Path, PathBuilder, PathStyle, Pixels, Point,
    RenderImage, Size, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Plain,
    Matrix,
    Bench,
    Board,
    Wash,
    App,
}

impl Mode {
    fn parse(arg: Option<&str>) -> Mode {
        match arg {
            None | Some("plain") => Mode::Plain,
            Some("matrix") => Mode::Matrix,
            Some("bench") => Mode::Bench,
            Some("board") => Mode::Board,
            Some("wash") => Mode::Wash,
            Some("app") => Mode::App,
            Some(other) => {
                eprintln!("unknown mode {other:?}; using plain");
                Mode::Plain
            }
        }
    }

    /// Window size for this mode.
    fn window(&self) -> Size<Pixels> {
        let (width, height) = match self {
            Mode::Plain | Mode::Wash => (800.0, 600.0),
            Mode::App => (1440.0, 900.0),
            Mode::Matrix | Mode::Bench => (1000.0, 700.0),
            // The app's window on this machine, so culling matches the app.
            Mode::Board => (BOARD_WINDOW.0, BOARD_WINDOW.1),
        };
        Size {
            width: px(width),
            height: px(height),
        }
    }
}

struct Options {
    mode: Mode,
    board: Option<String>,
    /// Show the window and leave it open for a human.
    show: bool,
    /// Skip painting the first fill, to prove the checks can fail.
    break_one: bool,
}

const USAGE: &str =
    "usage: fill_repro [plain|matrix|wash|app|bench|board <file.boundless>] [--show] [--break]";

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        mode: Mode::Plain,
        board: None,
        show: false,
        break_one: false,
    };
    let mut words: Vec<String> = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--show" => options.show = true,
            "--break" => options.break_one = true,
            _ => words.push(arg),
        }
    }
    options.mode = Mode::parse(words.first().map(String::as_str));
    if matches!(options.mode, Mode::Board) {
        options.board = Some(words.get(1).cloned().ok_or(USAGE.to_string())?);
    }
    Ok(options)
}

// --- drawing -----------------------------------------------------------------

fn ellipse_color() -> gpui::Hsla {
    gpui::hsla(0.35, 0.2, 0.35, 1.0)
}

/// How one ellipse is painted.
#[derive(Clone, Copy)]
enum Paint {
    EvenOdd,
    NonZero,
    /// NonZero, half transparent.
    NonZeroAlpha,
    /// NonZero, light at the top and dark at the bottom.
    NonZeroGradient,
    /// EvenOdd, half transparent.
    EvenOddAlpha,
    /// A 5px stroke (known-good reference) instead of a fill.
    Stroke,
}

impl Paint {
    fn fill_rule(self) -> FillRule {
        match self {
            Paint::EvenOdd | Paint::EvenOddAlpha => FillRule::EvenOdd,
            _ => FillRule::NonZero,
        }
    }

    fn background(self) -> Option<Background> {
        match self {
            Paint::EvenOdd | Paint::NonZero => Some(ellipse_color().into()),
            Paint::NonZeroAlpha | Paint::EvenOddAlpha => {
                Some(gpui::hsla(0.35, 0.2, 0.35, 0.5).into())
            }
            Paint::NonZeroGradient => Some(gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(rgb(0x89b7e6), 0.0),
                gpui::linear_color_stop(rgb(0x1a324c), 1.0),
            )),
            Paint::Stroke => None,
        }
    }
}

/// One matrix cell: an ellipse built from LINE segments and painted one way.
struct MatrixCell {
    cx: f32,
    cy: f32,
    /// Reverse the point order (flips the winding direction).
    reverse: bool,
    paint: Paint,
    label: &'static str,
}

const RX: f32 = 95.0;
const RY: f32 = 62.0;
const ROW_Y: [f32; 2] = [266.0, 532.0];
const COL_X: [f32; 4] = [140.0, 380.0, 620.0, 860.0];

fn cells() -> Vec<MatrixCell> {
    let [y0, y1] = ROW_Y;
    let [x0, x1, x2, x3] = COL_X;
    let cell = |cx, cy, reverse, paint, label| MatrixCell {
        cx,
        cy,
        reverse,
        paint,
        label,
    };
    vec![
        cell(x0, y0, false, Paint::EvenOdd, "EO cw"),
        cell(x1, y0, true, Paint::EvenOdd, "EO rev"),
        cell(x2, y0, false, Paint::NonZero, "NZ cw"),
        cell(x3, y0, true, Paint::NonZero, "NZ rev"),
        cell(x0, y1, false, Paint::NonZeroAlpha, "NZ a=0.5"),
        cell(x1, y1, false, Paint::NonZeroGradient, "NZ grad"),
        cell(x2, y1, false, Paint::EvenOddAlpha, "EO a=0.5"),
        cell(x3, y1, false, Paint::Stroke, "stroke 5px"),
    ]
}

fn ellipse_points(cx: f32, cy: f32, rx: f32, ry: f32, reverse: bool) -> Vec<Point<Pixels>> {
    let n = 64;
    let mut pts: Vec<Point<Pixels>> = Vec::new();
    for i in 0..=n {
        let t = i as f32 / n as f32 * std::f32::consts::TAU;
        pts.push(point(px(cx + rx * t.cos()), px(cy + ry * t.sin())));
    }
    if reverse {
        pts.reverse();
    }
    pts
}

fn trace(builder: &mut PathBuilder, pts: &[Point<Pixels>]) {
    for (i, p) in pts.iter().enumerate() {
        if i == 0 {
            builder.move_to(*p);
        } else {
            builder.line_to(*p);
        }
    }
    builder.close();
}

/// Build a filled path from `pts`.
fn fill_path(pts: &[Point<Pixels>], rule: FillRule) -> Result<Path, anyhow::Error> {
    let mut b = PathBuilder::fill().with_style(PathStyle::Fill(FillOptions { fill_rule: rule }));
    trace(&mut b, pts);
    b.build()
}

/// Build a stroked path from `pts`.
fn stroke_path(pts: &[Point<Pixels>], width: f32) -> Result<Path, anyhow::Error> {
    let mut b = PathBuilder::stroke(px(width));
    trace(&mut b, pts);
    b.build()
}

const PAPER: [i32; 3] = [245, 239, 220]; // rgb(0xf5efdc)
const QUAD_COLOR: [i32; 3] = [32, 79, 22]; // rgb(0x204f16)
const INK: [i32; 3] = [71, 107, 75]; // hsla(0.35, 0.2, 0.35, 1.0)

const QUAD: Bounds<Pixels> = Bounds {
    origin: point(px(40.0), px(40.0)),
    size: size(px(90.0), px(90.0)),
};

/// Quad reference: a solid color at a known place, which the checks use to
/// find where logical pixels land in the read-back.
fn paint_quad(window: &mut Window) {
    window.paint_quad(gpui::fill(QUAD, rgb(0x204f16)));
}

/// The two ellipses of the original repro, positioned relative to the canvas
/// (left = clockwise, right = REVERSED). Returns their centres and points.
fn plain_ellipses(bounds: &Bounds<Pixels>) -> Vec<([f32; 2], Vec<Point<Pixels>>)> {
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let rx = w * 0.28 * 0.6;
    let ry = h * 0.22 * 0.6;
    [(0.25, false), (0.80, true)]
        .into_iter()
        .map(|(fx, reverse)| {
            let centre = [ox + w * fx, oy + h * 0.4];
            let pts = ellipse_points(centre[0], centre[1], rx, ry, reverse);
            (centre, pts)
        })
        .collect()
}

fn paint_matrix(window: &mut Window, break_one: bool) {
    for (i, cell) in cells().into_iter().enumerate() {
        let pts = ellipse_points(cell.cx, cell.cy, RX, RY, cell.reverse);
        // Faint 1px stroke of the same contour, so a fill that never reaches
        // the screen shows as an empty ring.
        if let Ok(ring) = stroke_path(&pts, 1.0) {
            window.paint_path(ring, rgb(0xc9c2b0));
        }
        if break_one && i == 0 {
            continue;
        }
        let painted = match cell.paint.background() {
            Some(color) => fill_path(&pts, cell.paint.fill_rule()).map(|path| (path, color)),
            None => stroke_path(&pts, 5.0).map(|path| (path, Background::from(ellipse_color()))),
        };
        match painted {
            Ok((path, color)) => window.paint_path(path, color),
            Err(e) => eprintln!("{}: build failed: {e}", cell.label),
        }
    }
    paint_quad(window);
}

fn paint_plain(bounds: &Bounds<Pixels>, window: &mut Window, break_one: bool) {
    for (i, (_, pts)) in plain_ellipses(bounds).into_iter().enumerate() {
        if break_one && i == 0 {
            continue;
        }
        match fill_path(&pts, FillRule::EvenOdd) {
            Ok(path) => window.paint_path(path, ellipse_color()),
            Err(e) => eprintln!("fill build failed: {e}"),
        }
    }
    paint_quad(window);
}

// --- checking ----------------------------------------------------------------

/// Where a logical pixel lands in the read-back, from the reference quad.
struct Map {
    scale: f32,
    ox: f32,
    oy: f32,
}

impl Map {
    fn x(&self, x: f32) -> f32 {
        self.ox + x * self.scale
    }

    fn y(&self, y: f32) -> f32 {
        self.oy + y * self.scale
    }
}

fn pixel(img: &image::RgbaImage, x: i32, y: i32) -> Option<[i32; 3]> {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 {
        return None;
    }
    let p = img.get_pixel(x as u32, y as u32);
    Some([p.0[0] as i32, p.0[1] as i32, p.0[2] as i32])
}

fn distance(a: [i32; 3], b: [i32; 3]) -> i32 {
    (0..3).map(|i| (a[i] - b[i]).abs()).max().unwrap_or(0)
}

fn rounded(c: [f32; 3]) -> [i32; 3] {
    c.map(|v| v.round() as i32)
}

fn rgb_text(c: [i32; 3]) -> String {
    format!("rgb({},{},{})", c[0], c[1], c[2])
}

fn calibrate(img: &image::RgbaImage) -> Option<Map> {
    let (mut x0, mut y0) = (i32::MAX, i32::MAX);
    let (mut x1, mut y1) = (i32::MIN, i32::MIN);
    for y in 0..img.height() as i32 {
        for x in 0..img.width() as i32 {
            if pixel(img, x, y).is_some_and(|c| distance(c, QUAD_COLOR) <= 4) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 > x1 {
        return None;
    }
    let side = f32::from(QUAD.size.width);
    let scale = (x1 - x0 + 1) as f32 / side;
    Some(Map {
        scale,
        ox: x0 as f32 - f32::from(QUAD.origin.x) * scale,
        oy: y0 as f32 - f32::from(QUAD.origin.y) * scale,
    })
}

/// Every device pixel in the square of side `2 * r + 1` centred on a logical point.
fn square(img: &image::RgbaImage, map: &Map, x: f32, y: f32, r: i32) -> Vec<[i32; 3]> {
    let (cx, cy) = (map.x(x).round() as i32, map.y(y).round() as i32);
    (-r..=r)
        .flat_map(|dy| (-r..=r).filter_map(move |dx| pixel(img, cx + dx, cy + dy)))
        .collect()
}

fn mean(samples: &[[i32; 3]]) -> Option<[f32; 3]> {
    (!samples.is_empty()).then(|| {
        let mut sum = [0f32; 3];
        for c in samples {
            for i in 0..3 {
                sum[i] += c[i] as f32;
            }
        }
        sum.map(|s| s / samples.len() as f32)
    })
}

/// The sample closest to `target`: for features as thin as a stroke.
fn closest(samples: &[[i32; 3]], target: [i32; 3]) -> Option<[i32; 3]> {
    samples.iter().copied().min_by_key(|c| distance(*c, target))
}

struct Report {
    passed: usize,
    failed: usize,
    /// Count without printing: for attempts that may be retried.
    mute: bool,
}

impl Report {
    fn check(&mut self, label: &str, ok: bool, detail: String) {
        if self.mute {
            if ok {
                self.passed += 1;
            } else {
                self.failed += 1;
            }
            return;
        }
        if ok {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        eprintln!("{} {label}: {detail}", if ok { "PASS" } else { "FAIL" });
    }
}

/// One thing that must hold at one place on the canvas.
struct Probe {
    label: &'static str,
    cx: f32,
    cy: f32,
    paint: Paint,
}

impl Probe {
    /// `ry` is the ellipse's vertical radius; samples are small squares
    /// well inside it, clear of the contour, the ring and the caption.
    fn check(&self, img: &image::RgbaImage, map: &Map, ry: f32, report: &mut Report) {
        let at = |x: f32, y: f32| square(img, map, x, y, 3);
        match self.paint {
            Paint::EvenOdd | Paint::NonZero => {
                let got = mean(&at(self.cx, self.cy));
                let ok = got.is_some_and(|g| distance(rounded(g), INK) <= 12);
                report.check(
                    self.label,
                    ok,
                    format!(
                        "centre {} want {}",
                        got.map_or_else(|| "none".to_string(), |g| rgb_text(rounded(g))),
                        rgb_text(INK)
                    ),
                );
            }
            Paint::NonZeroAlpha | Paint::EvenOddAlpha => {
                // Half-transparent ink over paper: how far the colour travels
                // from ink toward paper depends on the blending colour space,
                // so only a range is asserted. A fill painted twice (overlapping
                // tiles) lands well below it.
                let got = mean(&at(self.cx, self.cy));
                let ok = got.is_some_and(|g| {
                    (0..3).all(|i| {
                        let t = (g[i] - INK[i] as f32) / (PAPER[i] - INK[i]) as f32;
                        (0.42..=0.72).contains(&t)
                    })
                });
                report.check(
                    self.label,
                    ok,
                    format!(
                        "centre {}",
                        got.map_or_else(|| "none".to_string(), |g| rgb_text(rounded(g)))
                    ),
                );
            }
            Paint::NonZeroGradient => {
                let top = mean(&at(self.cx, self.cy - ry * 0.6));
                let bottom = mean(&at(self.cx, self.cy + ry * 0.6));
                let ok = match (top, bottom) {
                    (Some(t), Some(b)) => {
                        t.iter().sum::<f32>() > b.iter().sum::<f32>() + 150.0
                            && b.iter().sum::<f32>() < PAPER.iter().sum::<i32>() as f32 - 150.0
                    }
                    _ => false,
                };
                report.check(self.label, ok, format!("top {top:?} bottom {bottom:?}"));
            }
            Paint::Stroke => {
                let centre = mean(&at(self.cx, self.cy));
                // The stroke is centred on the contour: the ellipse's leftmost point.
                let edge = closest(&square(img, map, self.cx - RX, self.cy, 3), INK);
                let ok = centre.is_some_and(|c| distance(rounded(c), PAPER) <= 12)
                    && edge.is_some_and(|e| distance(e, INK) <= 12);
                report.check(
                    self.label,
                    ok,
                    format!(
                        "centre {} edge {}",
                        centre.map_or_else(|| "none".to_string(), |c| rgb_text(rounded(c))),
                        edge.map_or_else(|| "none".to_string(), rgb_text)
                    ),
                );
            }
        }
    }
}

/// The probes of a `plain` or `matrix` frame. `None` if `plain` was never
/// painted (its ellipses sit relative to the canvas).
fn probes_for(mode: Mode, canvas: Option<&Bounds<Pixels>>) -> Option<Vec<Probe>> {
    match mode {
        Mode::Matrix => Some(
            cells()
                .into_iter()
                .map(|c| Probe {
                    label: c.label,
                    cx: c.cx,
                    cy: c.cy,
                    paint: c.paint,
                })
                .collect(),
        ),
        _ => {
            let Some(bounds) = canvas else {
                eprintln!("FAIL canvas: the probe canvas was never painted");
                return None;
            };
            // Checked where it is judged, not where it is painted: the frames
            // before the hidden window has its size do have an empty canvas.
            if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
                eprintln!(
                    "FAIL canvas: it has no size ({bounds:?}). An unsized canvas is what \
                     made this probe wrong for months; see the module docs."
                );
                return None;
            }
            Some(
                plain_ellipses(bounds)
                    .into_iter()
                    .zip(["EO cw", "EO rev"])
                    .map(|((centre, _), label)| Probe {
                        label,
                        cx: centre[0],
                        cy: centre[1],
                        paint: Paint::EvenOdd,
                    })
                    .collect(),
            )
        }
    }
}

/// `FILL_REPRO_DUMP=<file.png>` keeps a read-back frame, to look at it.
fn keep_dump(img: &image::RgbaImage) {
    if let Ok(path) = std::env::var("FILL_REPRO_DUMP") {
        match img.save(&path) {
            Ok(()) => eprintln!("read-back saved to {path}"),
            Err(e) => eprintln!("cannot save the read-back to {path}: {e}"),
        }
    }
}

/// Reads the rendered frame back and checks every probe. Returns
/// (passed, failed).
fn verify(window: &Window, probes: &[Probe]) -> (usize, usize) {
    let mut report = Report {
        passed: 0,
        failed: 0,
        mute: false,
    };
    let img = match window.render_to_image() {
        Ok(img) => img,
        Err(e) => {
            eprintln!("FAIL read-back: {e:#}");
            return (0, 1);
        }
    };
    eprintln!("read back a {}x{} image", img.width(), img.height());
    keep_dump(&img);
    let Some(map) = calibrate(&img) else {
        eprintln!("FAIL calibration: the reference quad is not in the read-back");
        return (0, 1);
    };
    eprintln!("scale factor {:.3}", map.scale);
    for probe in probes {
        probe.check(&img, &map, RY, &mut report);
    }
    // Somewhere nothing was painted must still be the canvas colour.
    let blank = mean(&square(&img, &map, 500.0, 500.0, 3));
    let ok = blank.is_some_and(|c| distance(rounded(c), PAPER) <= 12);
    report.check(
        "unpainted canvas",
        ok,
        format!(
            "at (500,500) {} want {}",
            blank.map_or_else(|| "none".to_string(), |c| rgb_text(rounded(c))),
            rgb_text(PAPER)
        ),
    );
    (report.passed, report.failed)
}

// --- bench -------------------------------------------------------------------

/// One synthetic workload. Shaped like what the board paints: dense hachure
/// lines (very many short strokes), pen strokes (few, long ribbons), and real
/// fills if the board switches back to them.
struct BenchCase {
    /// `true` = filled closed polygons, `false` = 2px strokes.
    fill: bool,
    count: usize,
    segments: usize,
}

const BENCH_CASES: [BenchCase; 5] = [
    BenchCase {
        fill: false,
        count: 500,
        segments: 2,
    },
    BenchCase {
        fill: false,
        count: 4000,
        segments: 2,
    },
    BenchCase {
        fill: false,
        count: 2000,
        segments: 8,
    },
    BenchCase {
        fill: false,
        count: 50,
        segments: 400,
    },
    BenchCase {
        fill: true,
        count: 500,
        segments: 64,
    },
];
const BENCH_FRAMES: u32 = 60;
const BOARD_WINDOW: (f32, f32) = (1440.0, 900.0);

/// What the board paints for an element's paint list: a vector path, or a
/// raster wash where one is at hand.
enum Painted {
    Path(Path, Background),
    Image(Bounds<Pixels>, Arc<RenderImage>),
}

impl Painted {
    fn paint(&self, window: &mut Window) {
        match self {
            Painted::Path(path, color) => window.paint_path(path.clone(), *color),
            Painted::Image(bounds, image) => {
                let _ = window.paint_image(
                    *bounds,
                    *bounds,
                    Corners::all(px(0.0)),
                    image.clone(),
                    0,
                    false,
                );
            }
        }
    }
}

#[derive(Default)]
struct Bench {
    case: usize,
    /// What the current case paints, built once like the board's render
    /// cache holds its `ReadyPath`s, then repainted every frame.
    paths: Vec<Painted>,
    /// Set for `board` mode: describes the one supplied case and makes it the
    /// only one.
    board: Option<String>,
    /// Set once there is nothing left to paint.
    finished: bool,
    frames: u32,
    paint: Duration,
    clone_only: Duration,
}

impl Bench {
    fn build(&mut self) {
        let BenchCase {
            fill,
            count,
            segments,
        } = BENCH_CASES[self.case];
        let color: Background = ellipse_color().into();
        // Spread the paths over a 20-column grid so each stays on screen.
        let (cols, rows) = (20.0, (count as f32 / 20.0).ceil());
        let (dx, dy) = (1000.0 / cols, 700.0 / rows);
        self.paths = (0..count)
            .map(|i| {
                let (x0, y0) = ((i % 20) as f32 * dx, (i / 20) as f32 * dy);
                let pts: Vec<Point<Pixels>> = (0..=segments)
                    .map(|s| {
                        // A gently zigzagging horizontal polyline.
                        let x = x0 + 4.0 + (s as f32 / segments as f32) * (dx - 8.0);
                        let y = y0 + dy * 0.5 + if s % 2 == 0 { 0.0 } else { 1.5 };
                        point(px(x), px(y))
                    })
                    .collect();
                let path = if fill {
                    fill_path(&pts, FillRule::EvenOdd)
                } else {
                    stroke_path(&pts, 2.0)
                }
                .expect("bench path builds");
                Painted::Path(path, color)
            })
            .collect();
        self.frames = 0;
        self.paint = Duration::ZERO;
        self.clone_only = Duration::ZERO;
    }

    fn done(&self) -> bool {
        self.case >= BENCH_CASES.len()
    }

    fn label(&self) -> String {
        match &self.board {
            Some(note) => format!("board {:7} painted ({note})", self.paths.len()),
            None => {
                let BenchCase {
                    fill,
                    count,
                    segments,
                } = BENCH_CASES[self.case];
                format!(
                    "{:6} count={count:4} segments={segments:3}",
                    if fill { "fill" } else { "stroke" }
                )
            }
        }
    }

    /// Paint one frame of the current case; returns false once finished.
    /// The last case's paths stay in place, so they can be painted again
    /// ([`Self::repaint`]) for a picture.
    fn frame(&mut self, window: &mut Window) -> bool {
        if self.finished {
            return false;
        }
        if self.paths.is_empty() {
            // A board's paths are loaded up front, so for it an empty list
            // means there is nothing to do.
            if self.board.is_some() || self.done() {
                self.finished = true;
                return false;
            }
            self.build();
        }
        let count = self.paths.len();
        let t = Instant::now();
        for painted in &self.paths {
            if let Painted::Path(path, _) = painted {
                std::hint::black_box(path.clone());
            }
        }
        self.clone_only += t.elapsed();

        let t = Instant::now();
        for painted in &self.paths {
            painted.paint(window);
        }
        self.paint += t.elapsed();
        self.frames += 1;

        if self.frames >= BENCH_FRAMES {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / self.frames as f64;
            eprintln!(
                "bench {}: paint_path {:.3} ms/frame ({:.2} us/path), clone-only {:.3} ms/frame",
                self.label(),
                ms(self.paint),
                self.paint.as_secs_f64() * 1e6 / (self.frames as usize * count) as f64,
                ms(self.clone_only),
            );
            self.case = if self.board.is_some() {
                BENCH_CASES.len()
            } else {
                self.case + 1
            };
            self.finished = self.done();
            if !self.finished {
                self.paths.clear();
            }
        }
        !self.finished
    }

    /// Paint the paths of the last frame again, untimed: keeps them on show
    /// after the measurement ends.
    fn repaint(&self, window: &mut Window) {
        for painted in &self.paths {
            painted.paint(window);
        }
    }
}

/// A saved board reduced to what the app would paint: every element's paint
/// list at the saved camera, culled by element bounds against the window.
struct BoardPaths {
    elements: usize,
    visible: usize,
    paths: Vec<Painted>,
    /// Of them raster washes (the 彩 fill).
    washes: usize,
    /// Rough-geometry segments per op-set type. The per-frame cost scales
    /// with these, so they show where it comes from.
    segments: BTreeMap<String, usize>,
    /// Cold cost of generating all those paths (roughr + build).
    build: Duration,
}

fn board_paths(file: &str) -> anyhow::Result<BoardPaths> {
    let file = boundless::scene::SceneFile::parse(&std::fs::read_to_string(file)?)?;
    let camera = file.camera;
    let origin = point(px(0.0), px(0.0));
    let (vw, vh) = BOARD_WINDOW;
    let mut out = BoardPaths {
        elements: file.elements.len(),
        visible: 0,
        paths: Vec::new(),
        washes: 0,
        segments: BTreeMap::new(),
        build: Duration::ZERO,
    };
    for el in &file.elements {
        let a = camera.world_to_screen(WPoint::new(el.bounds.x, el.bounds.y), origin);
        let b = camera.world_to_screen(
            WPoint::new(el.bounds.x + el.bounds.w, el.bounds.y + el.bounds.h),
            origin,
        );
        let (x0, y0) = (
            f32::from(a.x).min(f32::from(b.x)),
            f32::from(a.y).min(f32::from(b.y)),
        );
        let (x1, y1) = (
            f32::from(a.x).max(f32::from(b.x)),
            f32::from(a.y).max(f32::from(b.y)),
        );
        if x1 < 0.0 || y1 < 0.0 || x0 > vw || y0 > vh {
            continue;
        }
        out.visible += 1;
        let geom = world_geometry(el);
        if let WorldGeom::Rough(sets) = &geom {
            for set in sets {
                *out.segments
                    .entry(format!("{:?}", set.op_set_type))
                    .or_default() += set.ops.elements().len();
            }
        }
        let t = Instant::now();
        for op in paint_world_geom(&geom, el, &camera, origin) {
            match op {
                PaintOp::Path(rp) => out.paths.push(Painted::Path(rp.path, rp.color)),
                PaintOp::Wash(wash) => {
                    // The board resolves a wash through its cache once the
                    // element holds still; here it is rasterized at once, for
                    // a window of one device pixel per pixel.
                    let raster = wash::rasterize(
                        &geom,
                        el.style.background.unwrap_or(0),
                        el.seed,
                        wash::bucket_scale(camera.zoom),
                        (el.bounds.x, el.bounds.y),
                    );
                    match raster {
                        Some(raster) => {
                            let rect = raster.world_rect((el.bounds.x, el.bounds.y));
                            let corner =
                                |x: f64, y: f64| camera.world_to_screen(WPoint::new(x, y), origin);
                            let (tl, br) = (
                                corner(rect.x, rect.y),
                                corner(rect.x + rect.w, rect.y + rect.h),
                            );
                            out.paths.push(Painted::Image(
                                Bounds {
                                    origin: tl,
                                    size: size(br.x - tl.x, br.y - tl.y),
                                },
                                raster.image,
                            ));
                            out.washes += 1;
                        }
                        None => out.paths.extend(
                            wash.fallback
                                .into_iter()
                                .map(|rp| Painted::Path(rp.path, rp.color)),
                        ),
                    }
                }
            }
        }
        out.build += t.elapsed();
    }
    Ok(out)
}

// --- wash through the board's flow -------------------------------------------

/// The first fill colours of the two washes, and what the first is restyled
/// to. Each sits well off the paper once blended at any coverage.
const WASH_COLORS: [u32; 2] = [0x2f9e44, 0x1971c2];
const RESTYLED: u32 = 0xe8590c;

/// Two watercolor rectangles side by side, clear of the reference quad.
fn wash_bounds(i: usize) -> WBounds {
    WBounds::new(60.0 + 360.0 * i as f64, 200.0, 260.0, 180.0)
}

/// The centre of rectangle `i`, in logical pixels.
fn wash_centre(i: usize) -> (f64, f64) {
    let b = wash_bounds(i);
    (b.x + b.w / 2.0, b.y + b.h / 2.0)
}

/// Two watercolor rectangles, painted the way the board paints them: elements
/// of a scene, their washes through a `WashCache` with its worker threads,
/// one frame at a time.
struct WashProbe {
    scene: Scene,
    cache: wash::WashCache,
    /// Paint the flat glazes the board shows while a wash is unresolved,
    /// washes at hand or not: what to compare a wash against.
    stand_in: bool,
    /// Set by a frame that found every wash the current stage waits for at
    /// hand: the frame is then read back and judged.
    judge: Option<usize>,
    /// Textures handed to `drop_image` so far.
    dropped: usize,
    frames: usize,
}

impl WashProbe {
    fn new() -> Self {
        let shape = |i: usize| {
            let mut el = Element::new(
                ElementKind::Rectangle,
                wash_bounds(i),
                ElementStyle {
                    background: Some(WASH_COLORS[i]),
                    fill_style: FillStyle::Watercolor,
                    // No outline: the checks read the wash alone.
                    stroke_none: true,
                    ..Default::default()
                },
            );
            el.seed = 20261125;
            el
        };
        let mut scene = Scene::new();
        scene.add(shape(0));
        scene.add(shape(1));
        WashProbe {
            scene,
            cache: wash::WashCache::new(),
            stand_in: false,
            judge: None,
            dropped: 0,
            frames: 0,
        }
    }

    /// The stage the washes on screen belong to: 0 for the first colours, 1
    /// once the first rectangle was restyled.
    fn stage(&self) -> usize {
        usize::from(self.scene.elements[0].style.background == Some(RESTYLED))
    }
}

/// One frame, laid out as the board's `build_paint` and paint phase are.
fn wash_frame(probe: &Rc<RefCell<WashProbe>>, window: &mut Window) {
    let mut probe = probe.borrow_mut();
    // As `build_paint` does, in its order.
    probe.cache.retain_scene(&probe.scene);
    let washed = probe.cache.work(|el| Arc::new(world_geometry(el)));
    // Before any wash is resolved into this frame's list.
    let stale = probe.cache.take_stale();

    let (camera, origin) = (Camera::default(), point(px(0.0), px(0.0)));
    let mut ready = [false; 2];
    for (i, el) in probe.scene.elements.iter().enumerate() {
        for op in paint_world_geom(&world_geometry(el), el, &camera, origin) {
            match op {
                PaintOp::Path(rp) => window.paint_path(rp.path, rp.color),
                PaintOp::Wash(wash) => {
                    // What the board's `resolve_wash` does.
                    let drawn = match probe.cache.image(el, 1.0) {
                        WashDraw::Ready(image) => {
                            if let Some(r) = ready.get_mut(i) {
                                *r = true;
                            }
                            Some(image)
                        }
                        WashDraw::Pending(older) => older,
                    };
                    let image = if probe.stand_in { None } else { drawn };
                    match image {
                        Some(image) => {
                            let rect = image.world_rect((el.bounds.x, el.bounds.y));
                            let at =
                                |x: f64, y: f64| camera.world_to_screen(WPoint::new(x, y), origin);
                            let (tl, br) =
                                (at(rect.x, rect.y), at(rect.x + rect.w, rect.y + rect.h));
                            let bounds = Bounds {
                                origin: tl,
                                size: size(br.x - tl.x, br.y - tl.y),
                            };
                            let _ = window.paint_image(
                                bounds,
                                bounds,
                                Corners::all(px(0.0)),
                                image.image.clone(),
                                0,
                                false,
                            );
                        }
                        None => {
                            for rp in wash.fallback {
                                window.paint_path(rp.path, rp.color);
                            }
                        }
                    }
                }
            }
        }
    }
    for image in &stale {
        let _ = window.drop_image(image.clone());
    }
    paint_quad(window);
    probe.dropped += stale.len();
    probe.frames += 1;
    if probe.judge.is_none() && ready.iter().all(|r| *r) {
        probe.judge = Some(probe.stage());
    }
    if washed || probe.cache.busy() {
        window.request_animation_frame();
    }
}

/// Paints one frame — the washes, or with `stand_in` the glazes the board
/// shows until they are at hand — and reads it back from the GPU.
fn wash_read_back(
    probe: &Rc<RefCell<WashProbe>>,
    stand_in: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<(image::RgbaImage, Map)> {
    probe.borrow_mut().stand_in = stand_in;
    window.refresh();
    window.draw(cx).clear(cx);
    probe.borrow_mut().stand_in = false;
    let img = window.render_to_image().ok()?;
    let map = calibrate(&img)?;
    Some((img, map))
}

/// How far a pixel is from the paper toward a fully covered one, from its red
/// channel: 0 at the paper, 1 where the fill is at full strength.
fn coverage(pixel: [i32; 3], full: [i32; 3]) -> f32 {
    ((PAPER[0] - pixel[0]) as f32 / (PAPER[0] - full[0]).max(1) as f32).clamp(0.0, 1.0)
}

/// The first rectangle as read back: its strength at the centre, and along
/// nine rows of its left edge where the tint starts and how it builds up
/// over the device pixels after that.
struct WashRead<'a> {
    img: &'a image::RgbaImage,
    map: &'a Map,
    full: [i32; 3],
}

impl WashRead<'_> {
    fn new<'a>(img: &'a image::RgbaImage, map: &'a Map) -> WashRead<'a> {
        let (cx, cy) = wash_centre(0);
        let full = mean(&square(img, map, cx as f32, cy as f32, 2)).map_or(PAPER, rounded);
        WashRead { img, map, full }
    }

    /// The tint's strength at device pixel (`x`, `y`).
    fn strength(&self, x: i32, y: i32) -> f32 {
        coverage(pixel(self.img, x, y).unwrap_or(PAPER), self.full)
    }

    /// The tint's strength at device pixel (`x`, `y`) and in the device
    /// pixels right of it, up to `n`.
    fn run(&self, x: i32, y: i32, n: usize) -> Vec<f32> {
        (0..n as i32).map(|dx| self.strength(x + dx, y)).collect()
    }

    /// Per row (nine, spread over the rectangle's height): the device column
    /// where the tint starts on its left edge, and the run from there;
    /// `None` where the row holds no tint.
    fn left_edge(&self) -> Vec<Option<(i32, Vec<f32>)>> {
        let left = self.map.x(wash_bounds(0).x as f32).round() as i32;
        let (_, cy) = wash_centre(0);
        (-4..=4)
            .map(|row| {
                let y = self.map.y((cy + f64::from(row) * 15.0) as f32).round() as i32;
                let first = ((left - 40)..).find(|x| self.strength(*x, y) > 0.03);
                first.map(|x| (x, self.run(x, y, 12)))
            })
            .collect()
    }

    /// How far the red channel ranges over a 60 px square at the centre: a
    /// wash is pigment in water and varies pixel to pixel, a flat tint does
    /// not.
    fn red_spread(&self) -> i32 {
        let (cx, cy) = wash_centre(0);
        red_spread(
            self.img,
            self.map.x(cx as f32) as i32,
            self.map.y(cy as f32) as i32,
        )
    }
}

/// How far the red channel ranges over the 60 px square centred on device
/// pixel (`px`, `py`).
fn red_spread(img: &image::RgbaImage, px: i32, py: i32) -> i32 {
    let mut reds: Vec<i32> = (-30..30)
        .flat_map(|dy| (-30..30).map(move |dx| (px + dx, py + dy)))
        .filter_map(|(x, y)| pixel(img, x, y))
        .map(|c| c[0])
        .collect();
    reds.sort_unstable();
    reds.last().unwrap_or(&0) - reds.first().unwrap_or(&0)
}

/// The middle value; of nothing, NaN — which no threshold lets pass.
fn median(values: &[f32]) -> f32 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    sorted.get(sorted.len() / 2).copied().unwrap_or(f32::NAN)
}

fn pct(c: f32) -> String {
    format!("{}%", (c * 100.0).round() as u32)
}

fn run_text(run: &[f32]) -> String {
    run.iter().map(|c| pct(*c)).collect::<Vec<_>>().join(" ")
}

/// Both washes at hand after the first colours: compared with what the board
/// showed of the same elements until then.
fn washes_arrived(
    flat: &image::RgbaImage,
    flat_map: &Map,
    img: &image::RgbaImage,
    map: &Map,
    report: &mut Report,
) {
    for i in 0..2 {
        let (x, y) = wash_centre(i);
        let c = mean(&square(img, map, x as f32, y as f32, 2)).map_or(PAPER, rounded);
        let leads = match i {
            0 => c[1] - c[2],
            _ => c[2] - c[0],
        };
        report.check(
            if i == 0 { "first wash" } else { "second wash" },
            leads > 30 && c[0] < PAPER[0] - 60,
            format!("centre {}", rgb_text(c)),
        );
    }
    let (flat, wash) = (WashRead::new(flat, flat_map), WashRead::new(img, map));
    let (flat_edge, wash_edge) = (flat.left_edge(), wash.left_edge());
    // Per row: how far the wash's tint starts outside the glazes' outline,
    // and what the tint lacks of full strength over that stretch — the area
    // of its fade-in, which a hard-edged fringe has little of.
    let (mut out, mut fade) = (Vec::new(), Vec::new());
    for (flat, wash) in flat_edge.iter().zip(&wash_edge) {
        let (Some((flat_x, flat_run)), Some((wash_x, wash_run))) = (flat, wash) else {
            continue;
        };
        let beyond = flat_x - wash_x;
        let found = flat_run.iter().any(|c| *c > 0.5) && wash_run.iter().any(|c| *c > 0.5);
        if beyond <= 0 || beyond as usize >= wash_run.len() || !found {
            continue;
        }
        out.push(beyond as f32 / map.scale);
        fade.push(
            wash_run[..beyond as usize]
                .iter()
                .map(|c| 1.0 - c)
                .sum::<f32>()
                / map.scale,
        );
    }
    let rows = out.len();
    let (least, most) = (
        out.iter().cloned().fold(f32::MAX, f32::min),
        out.iter().cloned().fold(f32::MIN, f32::max),
    );
    let median_out = median(&out);
    let where_to = if out.is_empty() {
        "nowhere further out than the glazes' outline".to_string()
    } else {
        format!(
            "a median {median_out:.1} logical px further out than the glazes' outline ({least:.1} to {most:.1})"
        )
    };
    report.check(
        "bleeds past its outline",
        rows == 9 && median_out >= 2.0,
        format!("{rows} of 9 rows; {where_to}"),
    );
    let one_row = wash_edge[4].as_ref().map_or_else(
        || "no tint found".to_string(),
        |(_, run)| run_text(&run[..run.len().min(5)]),
    );
    let median_fade = median(&fade);
    let lacks = if fade.is_empty() {
        "nothing: no stretch to measure".to_string()
    } else {
        format!("{median_fade:.1} px")
    };
    report.check(
        "fades in at its edge",
        rows == 9 && median_fade >= 1.0,
        format!(
            concat!(
                "what it lacks of full strength over that stretch adds up to {lacks};",
                " one row, outermost first: {one_row}"
            ),
            lacks = lacks,
            one_row = one_row
        ),
    );
    // A wash is pigment in water: its interior varies pixel to pixel where
    // the flat glazes do not.
    let (flat_spread, wash_spread) = (flat.red_spread(), wash.red_spread());
    report.check(
        "wash has texture",
        wash_spread >= flat_spread + 6,
        format!("red channel spans {wash_spread} over a 60 px square, the glazes {flat_spread}"),
    );
}

/// Drives the wash probe's frames until both stages are judged.
fn run_wash(probe: &Rc<RefCell<WashProbe>>, window: &mut Window, cx: &mut App) -> u8 {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(60);
    let mut arrived = Duration::ZERO;
    let mut report = Report {
        passed: 0,
        failed: 0,
        mute: false,
    };
    loop {
        window.refresh();
        window.draw(cx).clear(cx);
        let judged = probe.borrow().judge;
        let Some(stage) = judged else {
            if Instant::now() > deadline {
                eprintln!("fill probe (Wash): the washes never arrived");
                return 3;
            }
            // The workers get on meanwhile.
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        if stage > 0 {
            match wash_read_back(probe, false, window, cx) {
                Some((img, map)) => {
                    let (cx, cy) = wash_centre(0);
                    let c =
                        mean(&square(&img, &map, cx as f32, cy as f32, 2)).map_or(PAPER, rounded);
                    report.check(
                        "restyled wash",
                        c[0] - c[2] > 90 && c[0] - c[1] > 30,
                        format!("centre {}", rgb_text(c)),
                    );
                }
                None => report.check("read-back", false, "no frame to judge".to_string()),
            }
            break;
        }
        arrived = started.elapsed();
        let frame = wash_read_back(probe, false, window, cx);
        let flat = wash_read_back(probe, true, window, cx);
        match (flat, frame) {
            (Some((flat, flat_map)), Some((img, map))) => {
                washes_arrived(&flat, &flat_map, &img, &map, &mut report)
            }
            _ => report.check("read-back", false, "no frame to judge".to_string()),
        }
        // Both washes are at hand: restyle the first, as a style change from
        // the inspector would, and wait for its wash to be made over.
        let mut probe = probe.borrow_mut();
        probe.scene.elements[0].style.background = Some(RESTYLED);
        probe.judge = None;
    }
    let (frames, dropped) = {
        let probe = probe.borrow();
        (probe.frames, probe.dropped)
    };
    // The first wash's old texture gave way to the restyled one.
    report.check(
        "replaced texture dropped",
        dropped >= 1,
        format!("{dropped} handed to drop_image"),
    );
    eprintln!(
        "fill probe (Wash): {} passed, {} failed; {frames} frames",
        report.passed, report.failed
    );
    eprintln!(
        "fill probe (Wash): washes at hand after {:.0} ms, restyled after {:.0} ms",
        arrived.as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1000.0
    );
    u8::from(report.failed > 0)
}

// --- the real board, hidden ---------------------------------------------------

/// The camera the `app` board is saved with: off the origin and not 1:1, so
/// that a wash placed or sized by anything but the camera shows.
const APP_CAMERA: Camera = Camera {
    x: -110.0,
    y: -10.0,
    zoom: 1.25,
};
/// The solid square the checks find first, to learn where world points
/// land: where in the world, and how wide.
const REF_AT: (f64, f64) = (420.0, 470.0);
const REF_SIDE: f64 = 90.0;

/// Where `app` keeps the app's home: a throwaway, so that the real
/// `BoardView` never sees — or writes — the user's boards, settings or logs.
fn app_home() -> PathBuf {
    std::env::temp_dir().join("boundless-fill-repro-home")
}

/// Points the app at a fresh throwaway home. Returns it.
fn sandbox_app() -> PathBuf {
    let home = app_home();
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("a throwaway home for the app");
    std::env::set_var("BOUNDLESS_HOME", &home);
    // Nor whatever API credentials the environment holds: the AI panel
    // opens on start and reads them.
    for key in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "BOUNDLESS_MODEL"] {
        std::env::remove_var(key);
    }
    home
}

/// The board `app` opens: the reference square and two 彩 shapes with
/// outlines, on the plain surface. Returns the file it was written to.
fn app_board(home: &std::path::Path) -> PathBuf {
    let mut scene = Scene::new();
    scene.add(Element::new(
        ElementKind::Rectangle,
        WBounds::new(REF_AT.0, REF_AT.1, REF_SIDE, REF_SIDE),
        ElementStyle {
            background: Some(0x204f16),
            fill_style: FillStyle::Solid,
            stroke_none: true,
            // No hand-drawn wobble: its outline is the exact square, so
            // its outline tells where the world is to the pixel.
            roughness: 0.0,
            ..Default::default()
        },
    ));
    for (i, kind) in [ElementKind::Rectangle, ElementKind::Ellipse]
        .into_iter()
        .enumerate()
    {
        let mut el = Element::new(
            kind,
            wash_bounds(i),
            ElementStyle {
                background: Some(WASH_COLORS[i]),
                fill_style: FillStyle::Watercolor,
                ..Default::default()
            },
        );
        el.seed = 20261125;
        scene.add(el);
    }
    let file = SceneFile::new(&scene, APP_CAMERA);
    let path = home.join("wash.boundless");
    std::fs::write(&path, serde_json::to_vec_pretty(&file).unwrap()).unwrap();
    path
}

/// What `src/main.rs` sets up before its window opens, minus what needs a
/// human around (crash logger, key bindings, update checks).
fn app_init(cx: &mut App) {
    gpui_component::init(cx);
    boundless::theme::init(cx);
    let embedded_fonts: Vec<std::borrow::Cow<'static, [u8]>> = vec![
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/Excalifont.ttf").as_slice()),
        std::borrow::Cow::Borrowed(
            include_bytes!("../assets/fonts/Xiaolai-Regular.ttf").as_slice(),
        ),
    ];
    if let Err(e) = cx.text_system().add_fonts(embedded_fonts) {
        eprintln!("failed to load embedded fonts: {e}");
    }
    gpui_component::GlobalState::global_mut(cx).set_app_menus(
        boundless::board::app_menus()
            .into_iter()
            .map(Menu::owned)
            .collect(),
    );
}

/// Where world points land in the app's read-back, from the reference square.
struct AppMap {
    /// Device pixels per world unit.
    per_unit: f32,
    /// The device pixel at the square's top-left corner.
    x0: f32,
    y0: f32,
}

impl AppMap {
    fn x(&self, world_x: f64) -> f32 {
        self.x0 + ((world_x - REF_AT.0) as f32) * self.per_unit
    }

    fn y(&self, world_y: f64) -> f32 {
        self.y0 + ((world_y - REF_AT.1) as f32) * self.per_unit
    }
}

/// Finds the reference square, whole: a square partly under a panel or a
/// toolbar would map every point wrong.
fn app_calibrate(img: &image::RgbaImage) -> Option<AppMap> {
    let (mut x0, mut y0) = (i32::MAX, i32::MAX);
    let (mut x1, mut y1) = (i32::MIN, i32::MIN);
    for y in 0..img.height() as i32 {
        for x in 0..img.width() as i32 {
            if pixel(img, x, y).is_some_and(|c| distance(c, QUAD_COLOR) <= 4) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    if w <= 0 || (w - h).abs() > 2 {
        eprintln!("fill probe (App): the reference square reads {w}x{h} px, not square");
        return None;
    }
    Some(AppMap {
        per_unit: w as f32 / REF_SIDE as f32,
        x0: x0 as f32,
        y0: y0 as f32,
    })
}

/// The mean colour of the 5x5 device pixels around device pixel (`x`, `y`),
/// rounded.
fn device_mean(img: &image::RgbaImage, x: f32, y: f32) -> [i32; 3] {
    let (cx, cy) = (x.round() as i32, y.round() as i32);
    let samples: Vec<[i32; 3]> = (-2..=2)
        .flat_map(|dy| (-2..=2).map(move |dx| (cx + dx, cy + dy)))
        .filter_map(|(x, y)| pixel(img, x, y))
        .collect();
    mean(&samples).map_or(PAPER, rounded)
}

#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
}

/// The app's frame as read back.
struct AppRead<'a> {
    img: &'a image::RgbaImage,
    map: &'a AppMap,
    /// The red channel of the first wash at its centre: fully covered.
    full: i32,
}

impl AppRead<'_> {
    fn new<'a>(img: &'a image::RgbaImage, map: &'a AppMap) -> AppRead<'a> {
        let (cx, cy) = wash_centre(0);
        AppRead {
            img,
            map,
            full: device_mean(img, map.x(cx), map.y(cy))[0],
        }
    }

    fn pixel(&self, axis: Axis, along: i32, across: i32) -> [i32; 3] {
        let (x, y) = match axis {
            Axis::X => (along, across),
            Axis::Y => (across, along),
        };
        pixel(self.img, x, y).unwrap_or(PAPER)
    }

    /// How far the first wash's tint reaches past its outline on one side,
    /// in world units (negative: it stops short of it), along the line
    /// `across`. `None` where the tint or the outline is not found.
    fn past_outline(&self, side: usize, across: i32) -> Option<f32> {
        let b = wash_bounds(0);
        let (axis, edge, centre, outward) = match side {
            0 => (Axis::X, b.x, b.x + b.w / 2.0, -1.0),
            1 => (Axis::X, b.x + b.w, b.x + b.w / 2.0, 1.0),
            2 => (Axis::Y, b.y, b.y + b.h / 2.0, -1.0),
            _ => (Axis::Y, b.y + b.h, b.y + b.h / 2.0, 1.0),
        };
        let at = |world: f64| match axis {
            Axis::X => self.map.x(world).round() as i32,
            Axis::Y => self.map.y(world).round() as i32,
        };
        // From 40 units outside the edge toward the centre of the shape.
        let (from, to) = (at(edge + outward * 40.0), at(centre));
        let step = if to > from { 1 } else { -1 };
        // The surface beside it, to read the tint against.
        let mut surface: Vec<i32> = (0..20)
            .map(|k| self.pixel(axis, from + step * k, across)[0])
            .collect();
        surface.sort_unstable();
        let surface = surface[surface.len() / 2];
        // Coloured, not merely darker: the outline's antialiased fringe is
        // grey, and so is the surface a wash leaves when it falls short of
        // its outline.
        let tinted = |c: [i32; 3]| {
            c.iter().max().unwrap_or(&0) - c.iter().min().unwrap_or(&0) >= 3
                && (surface - c[0]) as f32 > 0.03 * (surface - self.full).max(1) as f32
        };
        // Up to the outline: where the tint starts when it starts outside
        // it — the surface runs right up to the line when it does not.
        let mut p = from;
        let mut outside = None;
        let mut outline = Vec::new();
        while p != to {
            let c = self.pixel(axis, p, across);
            if c.iter().all(|v| *v < 100) {
                outline.push(p);
            } else if !outline.is_empty() {
                break;
            } else if outside.is_none() && tinted(c) {
                outside = Some(p as f32);
            }
            p += step;
        }
        let (first, last) = (*outline.first()?, *outline.last()?);
        let line = (first + last) as f32 / 2.0;
        let tint = match outside {
            Some(p) => p,
            None => {
                // Past the line: a wash that stops short of its outline
                // begins in there.
                let mut q = last + step;
                let mut inside = None;
                while q != to {
                    if tinted(self.pixel(axis, q, across)) {
                        inside = Some(q as f32);
                        break;
                    }
                    q += step;
                }
                inside?
            }
        };
        Some((line - tint) * step as f32 / self.map.per_unit)
    }
}

/// The app's 彩 shapes once their washes are at hand: each on its outline.
fn app_checks(img: &image::RgbaImage, map: &AppMap, report: &mut Report) {
    let read = AppRead::new(img, map);
    for i in 0..2 {
        let (cx, cy) = wash_centre(i);
        let c = device_mean(img, map.x(cx), map.y(cy));
        let leads = match i {
            0 => c[1] - c[2],
            _ => c[2] - c[0],
        };
        report.check(
            if i == 0 {
                "rectangle wash"
            } else {
                "ellipse wash"
            },
            leads > 30 && c[0] < PAPER[0] - 60,
            format!("centre {}", rgb_text(c)),
        );
    }
    // The wash and the outline are placed by two mappings of the same
    // camera: along three lines per side, how far the tint reaches past
    // the outline. It should be a few units out — the wash bleeds past the
    // line it was laid on — and about the same on every side.
    let (cx, cy) = wash_centre(0);
    let sides = ["left", "right", "top", "bottom"];
    let mut findings = Vec::new();
    for (side, name) in sides.iter().enumerate() {
        let found: Vec<f32> = [-50.0, 0.0, 50.0]
            .into_iter()
            .filter_map(|d| {
                read.past_outline(
                    side,
                    match side {
                        0 | 1 => map.y(cy + d).round() as i32,
                        _ => map.x(cx + d).round() as i32,
                    },
                )
            })
            .collect();
        findings.push((*name, median(&found), found.len()));
    }
    let text = findings
        .iter()
        .map(|(name, m, n)| format!("{name} {m:+.1} ({n}/3)"))
        .collect::<Vec<_>>()
        .join(", ");
    report.check(
        "wash sits on its outline",
        findings
            .iter()
            .all(|(_, m, n)| *n == 3 && (-4.0..=11.0).contains(m)),
        format!("tint past the outline, world units: {text}"),
    );
    // Pigment in water, not a flat tint: the interior varies pixel to pixel.
    let spread = red_spread(img, map.x(cx).round() as i32, map.y(cy).round() as i32);
    report.check(
        "wash has texture",
        spread >= 6,
        format!("red channel spans {spread} over a 60 px square"),
    );
}

/// Opens the board in the real `BoardView` and judges its 彩 shapes from the
/// pixels: with the sidebar closed, then open — which moves the canvas.
fn run_app(
    board: &Entity<BoardView>,
    file: &std::path::Path,
    window: &mut Window,
    cx: &mut App,
) -> u8 {
    let started = Instant::now();
    board.update(cx, |view, cx| {
        view.open_board(file.to_path_buf(), window, cx)
    });
    let mut failed = false;
    for sidebar_open in [false, true] {
        if sidebar_open {
            board.update(cx, |view, cx| view.toggle_explorer(cx));
        }
        let at = Instant::now();
        // Tried a few times: the washes and the sidebar's opening take a few
        // frames, and this window gets none from the platform. Only what the
        // passing attempt — or the last one — has to say gets printed.
        let mut attempt = Report {
            passed: 0,
            failed: 1,
            mute: false,
        };
        for trial in 0..4 {
            let wait_until = at + Duration::from_millis(1500 + 700 * trial);
            while Instant::now() < wait_until {
                window.refresh();
                window.draw(cx).clear(cx);
                std::thread::sleep(Duration::from_millis(10));
            }
            let Some(img) = window.render_to_image().ok() else {
                eprintln!("FAIL read-back: no frame to judge");
                continue;
            };
            let Some(map) = app_calibrate(&img) else {
                eprintln!("FAIL reference square: not found whole in the frame");
                keep_app_frame(&img, sidebar_open);
                continue;
            };
            let mut probe = Report {
                passed: 0,
                failed: 0,
                mute: true,
            };
            app_checks(&img, &map, &mut probe);
            if probe.failed == 0 || trial == 3 {
                attempt = Report {
                    passed: 0,
                    failed: 0,
                    mute: false,
                };
                app_checks(&img, &map, &mut attempt);
                keep_app_frame(&img, sidebar_open);
            }
            if probe.failed == 0 {
                break;
            }
        }
        eprintln!(
            "fill probe (App, sidebar {}): {} passed, {} failed; {:.0} ms",
            if sidebar_open { "open" } else { "closed" },
            attempt.passed,
            attempt.failed,
            (Instant::now() - started).as_secs_f64() * 1000.0
        );
        failed |= attempt.failed > 0;
    }
    u8::from(failed)
}

/// With `FILL_REPRO_DUMP` set, keeps the judged frame of the sidebar-closed
/// stage under that path and of the open stage beside it.
fn keep_app_frame(img: &image::RgbaImage, sidebar_open: bool) {
    let Some(dump) = std::env::var_os("FILL_REPRO_DUMP") else {
        return;
    };
    let mut path = PathBuf::from(dump);
    if sidebar_open {
        let stem = path
            .file_stem()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        path.set_file_name(format!("{}-open.png", stem.to_string_lossy()));
    }
    if let Err(e) = img.save(&path) {
        eprintln!("cannot keep the frame at {}: {e}", path.display());
    }
}

// --- app ---------------------------------------------------------------------

static EXIT: AtomicU8 = AtomicU8::new(0);

fn finish(code: u8, cx: &mut App) {
    EXIT.store(code, Ordering::SeqCst);
    cx.quit();
}

struct FillRepro {
    options: Options,
    bench: Rc<RefCell<Bench>>,
    wash: Rc<RefCell<WashProbe>>,
    /// The canvas bounds of the last painted frame (`plain` positions from them).
    canvas: Rc<RefCell<Option<Bounds<Pixels>>>>,
}

impl gpui::Render for FillRepro {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let _ = cx;
        let mode = self.options.mode;
        let (break_one, show) = (self.options.break_one, self.options.show);
        let bench = self.bench.clone();
        let wash = self.wash.clone();
        let canvas = self.canvas.clone();
        // `.size_full()` is load-bearing: see the module docs.
        let mut root = div().size_full().bg(rgb(0xf5efdc)).child(
            gpui::canvas(
                |_bounds, _window, _cx| (),
                move |bounds, _, window, _cx| match mode {
                    Mode::Bench | Mode::Board => {
                        let mut bench = bench.borrow_mut();
                        if bench.frame(window) {
                            // Hidden, the probe draws its frames itself
                            // (`run`); shown, it asks the platform for the next one.
                            if show {
                                window.request_animation_frame();
                            }
                        } else {
                            // Keep the last measured case on show.
                            bench.repaint(window);
                        }
                    }
                    Mode::Wash => wash_frame(&wash, window),
                    // Its window's root view is the board, not this view.
                    Mode::App => unreachable!("no FillRepro view in app mode"),
                    Mode::Matrix => paint_matrix(window, break_one),
                    Mode::Plain => {
                        *canvas.borrow_mut() = Some(bounds);
                        paint_plain(&bounds, window, break_one);
                    }
                },
            )
            .size_full(),
        );
        if matches!(mode, Mode::Matrix) {
            for cell in cells() {
                root = root.child(caption(cell.cx, cell.cy, cell.label));
            }
        }
        root
    }
}

/// Lays out and paints one frame by hand and reads it back from the GPU.
fn draw(window: &mut Window, cx: &mut App) -> Option<image::RgbaImage> {
    window.refresh();
    window.draw(cx).clear(cx);
    window.render_to_image().ok()
}

/// Drives the hidden window by hand — it gets no frames from the platform —
/// and returns the exit code: one frame to judge for `plain` and `matrix`, as
/// many as the bench needs (`wash` has its own driver, `run_wash`). The
/// window must already have its size.
fn run(
    mode: Mode,
    canvas: &RefCell<Option<Bounds<Pixels>>>,
    bench: &RefCell<Bench>,
    window: &mut Window,
    cx: &mut App,
) -> u8 {
    loop {
        window.refresh();
        window.draw(cx).clear(cx);
        match mode {
            Mode::Plain | Mode::Matrix => break,
            Mode::Wash => unreachable!("run_wash drives the wash probe"),
            Mode::App => unreachable!("run_app drives the real board"),
            Mode::Bench | Mode::Board => {
                if bench.borrow().finished {
                    eprintln!("bench done");
                    break;
                }
            }
        }
    }
    if matches!(mode, Mode::Bench | Mode::Board) {
        if std::env::var_os("FILL_REPRO_DUMP").is_some() {
            // The view repaints the last measured frame; keep a copy to look at.
            if let Some(img) = draw(window, cx) {
                keep_dump(&img);
            }
        }
        return 0;
    }
    // `probes_for` says why it has nothing to check.
    let Some(probes) = probes_for(mode, canvas.borrow().as_ref()) else {
        return 1;
    };
    let (passed, failed) = verify(window, &probes);
    eprintln!("fill probe ({mode:?}): {passed} passed, {failed} failed");
    u8::from(failed > 0)
}

/// A small centered caption above the ellipse centered at (`cx`, `cy`).
fn caption(cx: f32, cy: f32, label: &'static str) -> gpui::Div {
    div()
        .absolute()
        .left(px(cx - 60.0))
        .top(px(cy - RY - 28.0))
        .w(px(120.0))
        .text_xs()
        .text_color(rgb(0x444444))
        .text_center()
        .child(label)
}

fn main() {
    let options = parse_args().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let mut bench = Bench::default();
    if let Some(file) = &options.board {
        match board_paths(file) {
            Ok(board) => {
                let segments: Vec<String> = board
                    .segments
                    .iter()
                    .map(|(kind, n)| format!("{kind}={n}"))
                    .collect();
                bench.board = Some(format!(
                    "{} elements, {} on screen, {} washes, cold path generation {:.1} ms, segments [{}]",
                    board.elements,
                    board.visible,
                    board.washes,
                    board.build.as_secs_f64() * 1000.0,
                    segments.join(", ")
                ));
                bench.paths = board.paths;
            }
            Err(e) => {
                eprintln!("cannot load {file}: {e:#}");
                std::process::exit(2);
            }
        }
    }
    let (mode, show) = (options.mode, options.show);
    // The real board gets a home of its own, never the user's.
    let app_board = matches!(mode, Mode::App).then(|| app_board(&sandbox_app()));
    // The 0.7+ gpui ecosystem dropped `Application::new()`; desktop apps boot
    // through the platform crate's factory (same entry point as src/main.rs).
    gpui_platform::application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx: &mut App| {
            if app_board.is_some() {
                app_init(cx);
            }
            let bounds = Bounds::centered(None, mode.window(), cx);
            let bench = Rc::new(RefCell::new(bench));
            let wash = Rc::new(RefCell::new(WashProbe::new()));
            let canvas: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
            // Through an `AnyWindowHandle` on purpose: `WindowHandle::update`
            // holds the root view for its closure, and `draw` needs it too.
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Hidden unless a human asked to look: the checks read the frame
                // back from the GPU, so it never has to be on screen.
                show,
                focus: show,
                ..Default::default()
            };
            let the_board: Rc<RefCell<Option<Entity<BoardView>>>> = Rc::default();
            let window: AnyWindowHandle = if app_board.is_some() {
                let slot = the_board.clone();
                cx.open_window(
                    WindowOptions {
                        titlebar: Some(TitlebarOptions {
                            title: Some("Boundless".into()),
                            appears_transparent: true,
                            ..Default::default()
                        }),
                        ..window_options
                    },
                    |window, cx| {
                        let board = cx.new(|cx| BoardView::new(window, cx));
                        *slot.borrow_mut() = Some(board.clone());
                        if show {
                            // Shown for a human: on our board, not an empty one.
                            let file = app_board.clone().unwrap();
                            board.update(cx, |view, cx| view.open_board(file, window, cx));
                        }
                        let view: gpui::AnyView = board.into();
                        cx.new(|cx| gpui_component::Root::new(view, window, cx))
                    },
                )
                .unwrap()
                .into()
            } else {
                cx.open_window(window_options, |_window, cx| {
                    cx.new(|_| FillRepro {
                        options,
                        bench: bench.clone(),
                        wash: wash.clone(),
                        canvas: canvas.clone(),
                    })
                })
                .unwrap()
                .into()
            };
            if show {
                cx.activate(true);
                return;
            }
            // A hidden window is never placed, so it keeps its creation size — a
            // 1x1 render target, which would cull every path — and it gets no
            // frames. Give it the probe's size, wait for that to land, then draw
            // by hand.
            cx.spawn(async move |cx| {
                cx.update(|cx| {
                    window
                        .update(cx, |_, window, _| window.resize(mode.window()))
                        .unwrap();
                });
                let mut ready = false;
                for _ in 0..100 {
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                    ready = cx.update(|cx| {
                        window
                            .update(cx, |_, window, cx| {
                                draw(window, cx)
                                    .is_some_and(|img| img.width() > 1 && img.height() > 1)
                            })
                            .unwrap()
                    });
                    if ready {
                        break;
                    }
                }
                let code = if ready {
                    cx.update(|cx| {
                        window
                            .update(cx, |_, window, cx| match mode {
                                Mode::Wash => run_wash(&wash, window, cx),
                                Mode::App => run_app(
                                    the_board.borrow().as_ref().unwrap(),
                                    app_board.as_deref().unwrap(),
                                    window,
                                    cx,
                                ),
                                _ => run(mode, &canvas, &bench, window, cx),
                            })
                            .unwrap()
                    })
                } else {
                    eprintln!("fill probe ({mode:?}): the hidden window never took its size");
                    3
                };
                cx.update(|cx| finish(code, cx));
            })
            .detach();
        });
    std::process::exit(i32::from(EXIT.load(Ordering::SeqCst)));
}
