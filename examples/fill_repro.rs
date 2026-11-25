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
//! - `bench`: CPU time `paint_path` spends per frame for a few synthetic path
//!   counts/shapes; prints one line per case to stderr.
//! - `board <file.boundless>`: the same for a real board — every path of that
//!   file at its saved camera, culled to a 1440x900 window like the app.
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

use boundless::render::rough::WorldGeom;
use boundless::scene::WPoint;
use gpui::{
    div, point, prelude::*, px, rgb, size, AnyWindowHandle, App, Background, Bounds, FillOptions,
    FillRule, IntoElement, Path, PathBuilder, PathStyle, Pixels, Point, Size, Window, WindowBounds,
    WindowOptions,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Plain,
    Matrix,
    Bench,
    Board,
}

impl Mode {
    fn parse(arg: Option<&str>) -> Mode {
        match arg {
            None | Some("plain") => Mode::Plain,
            Some("matrix") => Mode::Matrix,
            Some("bench") => Mode::Bench,
            Some("board") => Mode::Board,
            Some(other) => {
                eprintln!("unknown mode {other:?}; using plain");
                Mode::Plain
            }
        }
    }

    /// Window size for this mode.
    fn window(&self) -> Size<Pixels> {
        let (width, height) = match self {
            Mode::Plain => (800.0, 600.0),
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
    "usage: fill_repro [plain|matrix|bench|board <file.boundless>] [--show] [--break]";

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
}

impl Report {
    fn check(&mut self, label: &str, ok: bool, detail: String) {
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

#[derive(Default)]
struct Bench {
    case: usize,
    /// The paths of the current case, built once like the board's render
    /// cache holds its `ReadyPath`s, then repainted every frame.
    paths: Vec<(Path, Background)>,
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
                (path, color)
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
            Some(note) => format!("board {:7} paths ({note})", self.paths.len()),
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
        for (path, _) in &self.paths {
            std::hint::black_box(path.clone());
        }
        self.clone_only += t.elapsed();

        let t = Instant::now();
        for (path, color) in &self.paths {
            window.paint_path(path.clone(), *color);
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
        for (path, color) in &self.paths {
            window.paint_path(path.clone(), *color);
        }
    }
}

/// A saved board reduced to what the app would paint: every path at the
/// saved camera, culled by element bounds against the window.
struct BoardPaths {
    elements: usize,
    visible: usize,
    paths: Vec<(Path, Background)>,
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
        if let WorldGeom::Rough(sets) = boundless::render::rough::world_geometry(el) {
            for set in sets {
                *out.segments
                    .entry(format!("{:?}", set.op_set_type))
                    .or_default() += set.ops.elements().len();
            }
        }
        let t = Instant::now();
        out.paths.extend(
            boundless::render::rough::paths_for_element(el, &camera, origin)
                .into_iter()
                .map(|rp| (rp.path, rp.color)),
        );
        out.build += t.elapsed();
    }
    Ok(out)
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
    /// The canvas bounds of the last painted frame (`plain` positions from them).
    canvas: Rc<RefCell<Option<Bounds<Pixels>>>>,
}

impl gpui::Render for FillRepro {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let _ = cx;
        let mode = self.options.mode;
        let (break_one, show) = (self.options.break_one, self.options.show);
        let bench = self.bench.clone();
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
/// many as the bench needs. The window must already have its size.
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
                    "{} elements, {} on screen, cold path generation {:.1} ms, segments [{}]",
                    board.elements,
                    board.visible,
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
    // The 0.7+ gpui ecosystem dropped `Application::new()`; desktop apps boot
    // through the platform crate's factory (same entry point as src/main.rs).
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, mode.window(), cx);
        let bench = Rc::new(RefCell::new(bench));
        let canvas: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
        // Through an `AnyWindowHandle` on purpose: `WindowHandle::update`
        // holds the root view for its closure, and `draw` needs it too.
        let window: AnyWindowHandle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // Hidden unless a human asked to look: the checks read the
                    // frame back from the GPU, so it never has to be on screen.
                    show,
                    focus: show,
                    ..Default::default()
                },
                |_window, cx| {
                    cx.new(|_| FillRepro {
                        options,
                        bench: bench.clone(),
                        canvas: canvas.clone(),
                    })
                },
            )
            .unwrap()
            .into();
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
                            draw(window, cx).is_some_and(|img| img.width() > 1 && img.height() > 1)
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
                        .update(cx, |_, window, cx| run(mode, &canvas, &bench, window, cx))
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
