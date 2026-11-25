//! Raster watercolor for the 彩 fill style.
//!
//! A wash is nothing a vector path can draw: its edge is soft and bleeds
//! past the outline, the pigment pools darker along that edge and settles
//! unevenly, and gpui fills a path with a flat colour or a linear gradient at
//! most (layering true fills only draws concentric steps). So a wash is
//! rasterized on the CPU and painted as one image under the vector outline.
//!
//! Per pixel, the signed distance to a glaze's outline decides how much the
//! glaze covers it (a soft edge inside the outline, a bleed outside), and how
//! much pigment pools there (deeper toward the edge). Two slow noise fields
//! make the wet edge wander and the pigment settle unevenly. The glazes are
//! the element's own true-fill op sets, each with its alpha
//! ([`crate::render::rough::WorldGeom`]).
//!
//! Rasterizing is not free, so images live in [`WashCache`], keyed by element
//! and by resolution bucket:
//!
//! * an image is laid out in its element's own coordinates: panning, and
//!   moving the element, only move it;
//! * zoom re-rasterizes across resolution buckets only, and until the new
//!   image is at hand the old one is stretched;
//! * a wash is made only once its element has held still for a frame, and,
//!   for want of a new resolution, once zooming has stopped: dragging an
//!   element, or zooming, rasterizes nothing;
//! * the board's cache makes washes on worker threads, so what a wash costs
//!   never comes out of a frame;
//! * until an image is at hand the board paints the glazes as flat fills;
//! * replaced images go stale, and the paint phase drops them from the atlas.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::{mpsc, Arc, LazyLock, Mutex};

use gpui::RenderImage;
use roughr::core::OpSetType;

use crate::render::cache::fingerprint;
use crate::render::rough::WorldGeom;
use crate::scene::{Element, ElementId, Scene, WBounds};

/// How far (world units) the wash reaches past an outline, thinning out.
const BLEED: f32 = 3.5;
/// How far (world units) inside an outline coverage turns from partial to
/// full.
const SOFT_EDGE: f32 = 1.4;
/// Amplitude (world units) of the noise that makes the wet edge wander.
const WANDER: f32 = 2.2;
/// Half of it, past which the outline cannot wander.
const WANDER_HALF: f32 = WANDER / 2.0;
/// Wavelength (world units) of that noise.
const WANDER_CELL: f32 = 14.0;
/// Depth (world units) over which pigment pooled at the edge thins out
/// inward, and how much it adds there.
const POOLING_WIDTH: f32 = 7.0;
const POOLING_STRENGTH: f32 = 1.1;
/// How far the pooled pigment's colour goes toward the pooled shade.
const POOLING_TINT: f32 = 0.8;
/// The pooled shade, as a factor of the fill colour.
const POOLED_SHADE: f32 = 0.62;
/// Range of the slow unevenness: a glaze settles this transparent at its
/// thinnest, and its wavelength.
const UNEVEN_LOW: f32 = 0.72;
const UNEVEN_CELL: f32 = 62.0;
/// Range of the per-pixel grain (the paper).
const GRAIN_LOW: f32 = 0.93;
const GRAIN_SPAN: f32 = 0.14;
/// Margin around the glazes' bounding box: everything the wash can reach.
const PAD: f64 = ((BLEED + WANDER / 2.0 + SOFT_EDGE) as f64).max(1.0);
/// Curves are flattened to a polygon this close (world units).
const FLATTEN_TOLERANCE: f64 = 0.25;
/// The wash is computed on a grid this fine (steps per world unit).
const SNAP: f64 = 1_000_000.0;
/// Each pixel's coverage is counted on a grid of this many samples per side.
const SUPERSAMPLE: usize = 3;
/// Pixels per screen pixel a wash is rasterized at, at least.
const PIXELS_PER_SCREEN_PIXEL: f64 = 1.0;
/// Caps on one image: past either the wash is rasterized coarser — a wash
/// only gets softer — which bounds what one costs to make and to keep.
pub const MAX_SIDE: u32 = 2048;
const MAX_PIXELS: f64 = 1_000_000.0;

/// Resolution to rasterize at for a screen scale of `scale` device pixels
/// per world unit: the smaller of 1.5·2^k and 2^(k+1) that is at least
/// [`PIXELS_PER_SCREEN_PIXEL`] times it. A wash is never stretched, and
/// zooming re-rasterizes only across buckets.
pub fn bucket_scale(scale: f64) -> f64 {
    let scale = scale * PIXELS_PER_SCREEN_PIXEL;
    let base = 2f64.powf(scale.max(1e-6).log2().floor());
    if scale <= base * 1.5 {
        base * 1.5
    } else {
        base * 2.0
    }
}

/// A rendered wash: a straight-alpha BGRA image (the byte order of the raster
/// canvases), covering `rect` in its element's own coordinates — the origin
/// being the element's bounds origin.
pub struct WashImage {
    pub image: Arc<RenderImage>,
    pub rect: WBounds,
    pub px_per_world: f64,
}

impl WashImage {
    /// What the wash covers on the board, for an element whose bounds origin
    /// is `at`.
    pub fn world_rect(&self, at: (f64, f64)) -> WBounds {
        WBounds::new(
            at.0 + self.rect.x,
            at.1 + self.rect.y,
            self.rect.w,
            self.rect.h,
        )
    }
}

/// Rasterize the 彩 wash of `geom` in the colour `color`, at `px_per_world`
/// pixels per world unit. `origin` is the element's bounds origin: the image
/// is laid out in the element's own coordinates, so moving the element moves
/// the image and nothing is rasterized again. `None` when `geom` holds no
/// glazes (the element is not painted with a wash).
pub fn rasterize(
    geom: &WorldGeom,
    color: u32,
    seed: u64,
    px_per_world: f64,
    origin: (f64, f64),
) -> Option<WashImage> {
    let (pixels, rect, px_per_world) = wash_pixels(geom, color, seed, px_per_world, origin)?;
    Some(WashImage {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
        rect,
        px_per_world,
    })
}

/// [`rasterize`] down to the pixels: (the image, what it covers in the
/// element's own coordinates, the resolution it ended up at).
fn wash_pixels(
    geom: &WorldGeom,
    color: u32,
    seed: u64,
    px_per_world: f64,
    origin: (f64, f64),
) -> Option<(image::RgbaImage, WBounds, f64)> {
    let WorldGeom::Rough(sets) = geom else {
        return None;
    };
    // The glazes: each outline flattened to a polygon in the element's own
    // coordinates, and the alpha it is painted at.
    let mut glazes: Vec<(Vec<(f64, f64)>, f32)> = Vec::new();
    for set in sets {
        if set.op_set_type != OpSetType::FillPath {
            continue;
        }
        let Some(alpha) = set.paint.as_ref().and_then(|p| p.color).map(|c| c.a) else {
            continue;
        };
        let mut pts = Vec::new();
        kurbo::flatten(
            set.ops.elements().iter().copied(),
            FLATTEN_TOLERANCE,
            |el| {
                if let kurbo::PathEl::MoveTo(p) | kurbo::PathEl::LineTo(p) = el {
                    // Snapped to a millionth of a unit. Subtracting a far-off
                    // origin loses the last bits, and the element's own
                    // coordinates must be the same numbers wherever it sits:
                    // that is what lets its wash be reused when it moves.
                    pts.push((
                        ((p.x - origin.0) * SNAP).round() / SNAP,
                        ((p.y - origin.1) * SNAP).round() / SNAP,
                    ));
                }
            },
        );
        if pts.len() >= 3 {
            glazes.push((pts, alpha));
        }
    }
    if glazes.is_empty() {
        return None;
    }

    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (pts, _) in &glazes {
        for &(x, y) in pts {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    let rect = WBounds::new(x0 - PAD, y0 - PAD, x1 - x0 + 2.0 * PAD, y1 - y0 + 2.0 * PAD);
    let q = px_per_world
        .min(MAX_SIDE as f64 / rect.w.max(1e-6))
        .min(MAX_SIDE as f64 / rect.h.max(1e-6))
        .min((MAX_PIXELS / (rect.w * rect.h).max(1e-6)).sqrt())
        .max(1e-3);
    let (pw, ph) = (
        ((rect.w * q).round() as u32).clamp(1, MAX_SIDE),
        ((rect.h * q).round() as u32).clamp(1, MAX_SIDE),
    );
    // The image is exactly `q` pixels per unit: fit what it covers to whole
    // pixels (a fraction of a pixel either way).
    let rect = WBounds::new(rect.x, rect.y, pw as f64 / q, ph as f64 / q);
    let (w, h) = (pw as usize, ph as usize);
    let n = w * h;
    let samples = (SUPERSAMPLE * SUPERSAMPLE) as u8;

    let mut count = vec![0u8; n];
    let mut edge = vec![0u8; n];
    let mut dist = vec![0f32; n];
    // The glazes composited so far, straight alpha.
    let (mut red, mut green, mut blue) = (vec![0f32; n], vec![0f32; n], vec![0f32; n]);
    let mut acc_a = vec![0f32; n];
    let paper: Vec<f32> = (0..n)
        .map(|i| GRAIN_LOW + GRAIN_SPAN * hash((i % w) as i32, (i / w) as i32, seed))
        .collect();
    let base = [
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
    ];
    // What pooling adds to each channel, by distance to the outline.
    let tint: [Vec<f32>; 3] = base.map(|b| {
        POOLING
            .iter()
            .map(|pooled| (b * POOLED_SHADE - b) * POOLING_TINT * pooled)
            .collect()
    });
    let qf = q as f32;
    let inv_q = 1.0 / qf;

    for (li, (pts, glaze_a)) in glazes.iter().enumerate() {
        count.fill(0);
        fill_polygon(pts, rect.x, rect.y, q as f64, w, h, &mut count);
        mark_edges(&count, w, h, samples, &mut edge);
        chamfer(&edge, w, h, &mut dist);
        let wander = Lattice::new((WANDER_CELL * qf).max(2.0), w, h, seed + li as u64 * 13);
        let uneven = Lattice::new((UNEVEN_CELL * qf).max(2.0), w, h, seed + 100 + li as u64);

        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                // Signed distance to the outline, in pixels: positive inside.
                let c = count[i];
                let d_px = if edge[i] != 0 {
                    match c {
                        0 => -0.5,
                        c if c == samples => 0.5,
                        c => c as f32 / samples as f32 - 0.5,
                    }
                } else if c * 2 >= samples {
                    dist[i] - 0.5
                } else {
                    0.5 - dist[i]
                };
                let clean = d_px * inv_q;
                // Only near the outline does the wandering matter: far
                // outside nothing is painted, deep inside everything is.
                let t = if clean > WANDER_HALF {
                    table_index(clean)
                } else if clean < -BLEED - WANDER_HALF {
                    continue;
                } else {
                    table_index(clean + (wander.at(x, y) - 0.5) * WANDER)
                };
                let cover = COVERAGE[t];
                if cover == 0.0 {
                    continue;
                }
                let settled = UNEVEN_LOW + (1.0 - UNEVEN_LOW) * uneven.at(x, y);
                let a =
                    (*glaze_a * cover * settled * paper[i] * (1.0 + POOLING_STRENGTH * POOLING[t]))
                        .min(1.0);
                // Source-over: what is already there shows through.
                let kept = acc_a[i] * (1.0 - a);
                let inv = 1.0 / (a + kept);
                red[i] = (base[0] + tint[0][t]) * a * inv + red[i] * kept * inv;
                green[i] = (base[1] + tint[1][t]) * a * inv + green[i] * kept * inv;
                blue[i] = (base[2] + tint[2][t]) * a * inv + blue[i] * kept * inv;
                acc_a[i] = a + kept;
            }
        }
    }

    let mut buf = Vec::with_capacity(n * 4);
    for i in 0..n {
        buf.extend_from_slice(&[
            (blue[i] * 255.0 + 0.5) as u8,
            (green[i] * 255.0 + 0.5) as u8,
            (red[i] * 255.0 + 0.5) as u8,
            (acc_a[i] * 255.0 + 0.5) as u8,
        ]);
    }
    Some((image::RgbaImage::from_raw(pw, ph, buf)?, rect, q as f64))
}

/// The shading reads coverage and pooling from tables indexed by the distance
/// to the outline, in this many steps per world unit, from this distance (the
/// most negative) up to where pooling has faded.
const TABLE_STEPS: f32 = 16.0;
const TABLE_FIRST: f32 = -8.0;
const TABLE_LEN: usize = ((32.0 - TABLE_FIRST) * TABLE_STEPS) as usize;

static COVERAGE: LazyLock<[f32; TABLE_LEN]> = LazyLock::new(|| {
    let mut table = [0f32; TABLE_LEN];
    for (i, cover) in table.iter_mut().enumerate() {
        *cover = smoothstep(-BLEED, SOFT_EDGE, TABLE_FIRST + i as f32 / TABLE_STEPS);
    }
    table
});

static POOLING: LazyLock<[f32; TABLE_LEN]> = LazyLock::new(|| {
    let mut table = [0f32; TABLE_LEN];
    for (i, pooled) in table.iter_mut().enumerate() {
        let d = (TABLE_FIRST + i as f32 / TABLE_STEPS).max(0.0);
        *pooled = (-d / POOLING_WIDTH).exp();
    }
    table
});

#[inline]
fn table_index(d: f32) -> usize {
    ((d - TABLE_FIRST) * TABLE_STEPS + 0.5).clamp(0.0, TABLE_LEN as f32 - 1.0) as usize
}

/// Count, per pixel, how many of its samples fall inside the polygon. The
/// polygon is filled by the non-zero winding rule, like the board's vector
/// fills: a roughr outline may wind around twice (an ellipse's does), and the
/// even-odd rule would leave its middle out. Coordinates are relative to
/// (`ox`, `oy`).
fn fill_polygon(
    pts: &[(f64, f64)],
    ox: f64,
    oy: f64,
    q: f64,
    w: usize,
    h: usize,
    count: &mut [u8],
) {
    let ss = SUPERSAMPLE;
    let scale = q * ss as f64;
    let (cols, rows) = (w * ss, h * ss);
    // Crossings of one sample row with the polygon's edges: where, and
    // whether the edge goes down (+1) or up there.
    let mut crossings: Vec<(f64, i32)> = Vec::with_capacity(32);
    for row in 0..rows {
        let y = row as f64 + 0.5;
        crossings.clear();
        for (i, &(ax, ay)) in pts.iter().enumerate() {
            let (bx, by) = pts[(i + 1) % pts.len()];
            let (ya, yb) = ((ay - oy) * scale, (by - oy) * scale);
            if (ya > y) != (yb > y) {
                let (xa, xb) = ((ax - ox) * scale, (bx - ox) * scale);
                crossings.push((
                    xa + (y - ya) / (yb - ya) * (xb - xa),
                    if yb > ya { 1 } else { -1 },
                ));
            }
        }
        if crossings.len() < 2 {
            continue;
        }
        crossings.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let line = &mut count[(row / ss) * w..][..w];
        // A span runs from where the winding leaves zero to where it returns.
        let (mut winding, mut open) = (0i32, 0f64);
        for &(x, direction) in &crossings {
            let outside = winding == 0;
            winding += direction;
            if outside {
                open = x;
            } else if winding == 0 {
                fill_span(open, x, ss, cols, line);
            }
        }
    }
}

/// Count one sample-row span (`from`, `to`), in sample columns, on `line`.
fn fill_span(from: f64, to: f64, ss: usize, cols: usize, line: &mut [u8]) {
    let from = ((from - 0.5).ceil().max(0.0) as usize).min(cols);
    let to = ((to - 0.5).ceil().max(0.0) as usize).min(cols);
    // A pixel takes all of its samples inside the span at once.
    for p in from / ss..=to.saturating_sub(1) / ss {
        let inside = to.min((p + 1) * ss) - from.max(p * ss);
        line[p] += inside as u8;
    }
}

/// Mark the pixels either side of a flip in coverage (covered = at least
/// half the samples inside): to the nearest pixel, they are the outline.
fn mark_edges(count: &[u8], w: usize, h: usize, samples: u8, edge: &mut [u8]) {
    // `count * 2 >= samples`, for an odd number of samples.
    let half = samples / 2 + 1;
    let covered = |c: u8| c >= half;
    edge.fill(0);
    // Side by side.
    for (row, marks) in count.chunks_exact(w).zip(edge.chunks_exact_mut(w)) {
        let mut left = covered(row[0]);
        let mut flipped = false;
        for (&c, mark) in row.iter().zip(marks) {
            let mine = covered(c);
            let flip = mine != left;
            *mark = u8::from(flip || flipped);
            flipped = flip;
            left = mine;
        }
    }
    // One above the other.
    for y in 1..h {
        let (above, below) = (&count[(y - 1) * w..][..w], &count[y * w..][..w]);
        let (upper, lower) = edge.split_at_mut(y * w);
        for ((&a, &b), (mark_a, mark_b)) in above
            .iter()
            .zip(below)
            .zip(upper[(y - 1) * w..].iter_mut().zip(lower))
        {
            if covered(a) != covered(b) {
                *mark_a = 1;
                *mark_b = 1;
            }
        }
    }
}

/// Distance of a pixel that has no pixel of the set anywhere near.
const FAR: f32 = 1.0e9;
/// Chamfer cost of a diagonal step (an orthogonal one is 1).
const DIAGONAL: f32 = std::f32::consts::SQRT_2;

/// Chamfer distance, in pixels, from every pixel to the nearest pixel of `to`
/// (0 on those): two sweeps, each pixel taking the distances of the four
/// neighbours behind it in scan order as already improved. Within a few
/// percent of the true distance, which is all a soft wash needs.
fn chamfer(to: &[u8], w: usize, h: usize, dist: &mut [f32]) {
    for (d, &t) in dist.iter_mut().zip(to) {
        *d = if t != 0 { 0.0 } else { FAR };
    }
    // Down and to the right.
    for y in 0..h {
        let above = y > 0;
        for x in 0..w {
            let i = y * w + x;
            let mut d = dist[i];
            if x > 0 {
                d = d.min(dist[i - 1] + 1.0);
                if above {
                    d = d.min(dist[i - w - 1] + DIAGONAL);
                }
            }
            if above {
                d = d.min(dist[i - w] + 1.0);
                if x + 1 < w {
                    d = d.min(dist[i - w + 1] + DIAGONAL);
                }
            }
            dist[i] = d;
        }
    }
    // Up and to the left.
    for y in (0..h).rev() {
        let below = y + 1 < h;
        for x in (0..w).rev() {
            let i = y * w + x;
            let mut d = dist[i];
            if x + 1 < w {
                d = d.min(dist[i + 1] + 1.0);
                if below {
                    d = d.min(dist[i + w + 1] + DIAGONAL);
                }
            }
            if below {
                d = d.min(dist[i + w] + 1.0);
                if x > 0 {
                    d = d.min(dist[i + w - 1] + DIAGONAL);
                }
            }
            dist[i] = d;
        }
    }
}

/// Smooth value noise on a square lattice of `cell` pixels, bilinear between
/// its points. The lattice coordinates are worked out per column and per row
/// once: the shading loop only interpolates.
struct Lattice {
    w: usize,
    data: Vec<f32>,
    col: Axes,
    row: Axes,
}

struct Axes {
    index: Vec<u32>,
    between: Vec<f32>,
}

impl Lattice {
    fn new(cell: f32, w: usize, h: usize, seed: u64) -> Self {
        let axes = |n: usize| -> (Vec<u32>, Vec<f32>) {
            let (mut index, mut between) = (Vec::with_capacity(n), Vec::with_capacity(n));
            for p in 0..n {
                let f = p as f32 / cell;
                let i = f as usize;
                let t = f - i as f32;
                index.push(i as u32);
                between.push(t * t * (3.0 - 2.0 * t));
            }
            (index, between)
        };
        let (cols, rows) = (axes(w), axes(h));
        let (lw, lh) = (
            *cols.0.last().unwrap() as usize + 3,
            *rows.0.last().unwrap() as usize + 3,
        );
        Lattice {
            w: lw,
            data: (0..lw * lh)
                .map(|i| hash((i % lw) as i32, (i / lw) as i32, seed))
                .collect(),
            col: Axes {
                index: cols.0,
                between: cols.1,
            },
            row: Axes {
                index: rows.0,
                between: rows.1,
            },
        }
    }

    #[inline]
    fn at(&self, x: usize, y: usize) -> f32 {
        let i = self.row.index[y] as usize * self.w + self.col.index[x] as usize;
        let (a, b) = (self.data[i], self.data[i + 1]);
        let (c, d) = (self.data[i + self.w], self.data[i + self.w + 1]);
        let tx = self.col.between[x];
        let ty = self.row.between[y];
        (a + (b - a) * tx) * (1.0 - ty) + (c + (d - c) * tx) * ty
    }
}

fn hash(x: i32, y: i32, seed: u64) -> f32 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ seed;
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    (h >> 40) as f32 / 16_777_216.0
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
// --- cache -------------------------------------------------------------------

/// Washes remembered; past this they are all dropped and remade on sight.
const MAX_ENTRIES: usize = 512;
/// Threads rasterizing washes at once, for the board's cache.
const WORKERS: usize = 3;

/// What to paint for an element's wash on a given frame.
#[derive(Clone)]
pub enum WashDraw {
    /// The wash at this resolution.
    Ready(Arc<WashImage>),
    /// A wash is being made. `older` is the wash of another resolution, if
    /// there is one, to stretch over the meantime.
    Pending(Option<Arc<WashImage>>),
}

struct Entry {
    /// The fingerprint this element had on the previous frame.
    seen: u64,
    /// The screen scale of the previous frame, to a ten-thousandth of a pixel
    /// per unit: zooming changes it every frame.
    seen_scale: Option<i64>,
    image: Option<Arc<WashImage>>,
    /// What `image` was rasterized for: the element's fingerprint (see
    /// `wash_fingerprint`) and the resolution bucket.
    image_fp: u64,
    image_scale: f64,
    /// A wash is queued for this element, or being made.
    queued: bool,
}

/// A wash to make: the element as it was queued, and what for.
struct Job {
    el: Element,
    fp: u64,
    scale: f64,
}

/// A job as a worker gets it, with its element's geometry — made on the
/// main thread, where the render cache lives.
struct Task {
    el: Element,
    fp: u64,
    scale: f64,
    geom: Arc<WorldGeom>,
}

/// What a worker hands back.
struct Done {
    id: ElementId,
    fp: u64,
    scale: f64,
    wash: Option<WashImage>,
}

fn raster(task: &Task) -> Done {
    Done {
        id: task.el.id,
        fp: task.fp,
        scale: task.scale,
        wash: rasterize(
            &task.geom,
            task.el.style.background.unwrap_or(0),
            task.el.seed,
            task.scale,
            (task.el.bounds.x, task.el.bounds.y),
        ),
    }
}

/// Per-element wash images, owned by the board view (interior mutability:
/// `build_paint` runs with `&self`).
///
/// The board's cache ([`WashCache::new`]) rasterizes on worker threads:
/// [`WashCache::work`] hands jobs over and takes back what has finished, and
/// costs the frame it runs in almost nothing. [`WashCache::inline`] makes
/// them on the calling thread instead. Both keep to the same rules:
///
/// * a wash is made only once its element has held still for a frame, and —
///   for want of a new resolution — once zooming has stopped: dragging an
///   element, or zooming, rasterizes nothing;
/// * an image is laid out in its element's own coordinates: panning, and
///   moving the element, only move it;
/// * zoom re-rasterizes across resolution buckets only, and until the new
///   image is at hand the old one is stretched;
/// * until an image is at hand the board paints the glazes as flat fills;
/// * replaced images go stale, and the paint phase drops them from the atlas.
pub struct WashCache {
    entries: RefCell<HashMap<ElementId, Entry>>,
    /// Queued by [`Self::image`], to be handed over by the next [`Self::work`].
    queued: RefCell<VecDeque<Job>>,
    /// With a worker.
    outstanding: Cell<usize>,
    /// The worker pool; `None` rasterizes inline.
    pool: RefCell<Option<(mpsc::Sender<Task>, mpsc::Receiver<Done>)>>,
    /// Images replaced or dropped since the paint phase last looked; it
    /// drains this into `window.drop_image` so the atlas frees them.
    stale: RefCell<Vec<Arc<RenderImage>>>,
}

/// `fingerprint` with the element moved to the origin: a wash is rasterized
/// in its element's own coordinates, so moving the element must not make it
/// over.
fn wash_fingerprint(el: &Element) -> u64 {
    let mut moved = el.clone();
    moved.bounds.x = 0.0;
    moved.bounds.y = 0.0;
    fingerprint(&moved)
}

impl Default for WashCache {
    fn default() -> Self {
        Self::new()
    }
}

impl WashCache {
    /// A cache that rasterizes on worker threads; what the board paints with.
    pub fn new() -> Self {
        Self::with_pool(Self::start_pool())
    }

    /// A cache whose [`Self::work`] rasterizes on the calling thread.
    pub fn inline() -> Self {
        Self::with_pool(None)
    }

    fn with_pool(pool: Option<(mpsc::Sender<Task>, mpsc::Receiver<Done>)>) -> Self {
        WashCache {
            entries: RefCell::new(HashMap::new()),
            queued: RefCell::new(VecDeque::new()),
            outstanding: Cell::new(0),
            pool: RefCell::new(pool),
            stale: RefCell::new(Vec::new()),
        }
    }

    /// The worker pool: `WORKERS` threads over one queue. They end with the
    /// cache, when its sending end of the queue is dropped.
    fn start_pool() -> Option<(mpsc::Sender<Task>, mpsc::Receiver<Done>)> {
        let (tasks, receiver) = mpsc::channel::<Task>();
        let (results, finished) = mpsc::channel::<Done>();
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..WORKERS {
            let (receiver, results) = (Arc::clone(&receiver), results.clone());
            // A worker that fails — it cannot be spawned, or its task panics
            // — costs its wash, not the others, and never the board.
            let running = std::thread::Builder::new()
                .name("boundless-wash".into())
                .spawn(move || loop {
                    let task = {
                        let queue = receiver.lock().unwrap_or_else(|e| e.into_inner());
                        queue.recv()
                    };
                    let Ok(task) = task else {
                        return;
                    };
                    let done =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| raster(&task)))
                            .unwrap_or_else(|_| Done {
                                id: task.el.id,
                                fp: task.fp,
                                scale: task.scale,
                                wash: None,
                            });
                    if results.send(done).is_err() {
                        return;
                    }
                });
            if running.is_err() {
                return None;
            }
        }
        Some((tasks, finished))
    }

    fn retire(&self, image: &Option<Arc<WashImage>>) {
        if let Some(wash) = image {
            self.stale.borrow_mut().push(wash.image.clone());
        }
    }

    /// The wash of `el` at a screen scale of `scale` device pixels per world
    /// unit. Call it once per frame per element painted: a wash is queued only
    /// when the element's fingerprint, and the scale, are those of the
    /// previous call — an element mid-drag, or a zoom in progress,
    /// rasterizes nothing.
    pub fn image(&self, el: &Element, scale: f64) -> WashDraw {
        let fp = wash_fingerprint(el);
        let raw = (scale * 10_000.0).round() as i64;
        let scale = bucket_scale(scale);
        let mut entries = self.entries.borrow_mut();
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(&el.id) {
            for ent in entries.values() {
                self.retire(&ent.image);
            }
            entries.clear();
            self.queued.borrow_mut().clear();
        }
        let ent = entries.entry(el.id).or_insert_with(|| Entry {
            seen: 0,
            seen_scale: None,
            image: None,
            image_fp: 0,
            image_scale: 0.0,
            queued: false,
        });
        let still = ent.seen == fp && ent.seen_scale == Some(raw);
        ent.seen = fp;
        ent.seen_scale = Some(raw);
        if ent.image_fp == fp && ent.image_scale == scale {
            if let Some(image) = &ent.image {
                return WashDraw::Ready(image.clone());
            }
        }
        // Changed since the previous frame, or zooming: whatever is queued, or
        // being made, is for the earlier state and will be dropped when it
        // comes back.
        if !still {
            ent.queued = false;
        }
        if still && !ent.queued {
            self.queued.borrow_mut().push_back(Job {
                el: el.clone(),
                fp,
                scale,
            });
            ent.queued = true;
        }
        WashDraw::Pending(ent.image.clone())
    }

    /// Whether `el` still waits for the wash `job` was queued for.
    fn wants(&self, job: &Job) -> bool {
        self.entries
            .borrow()
            .get(&job.el.id)
            .is_some_and(|e| e.queued && e.seen == job.fp)
    }

    /// Hand queued washes over to be rasterized, and take back the ones
    /// finished since. Call it once per frame, before the paint list is
    /// built, so that what it retires is not painted again; each element's
    /// geometry comes from `geom_of`, on the calling thread. Returns whether
    /// anything was handed over.
    pub fn work(&self, geom_of: impl Fn(&Element) -> Arc<WorldGeom>) -> bool {
        self.collect();
        // The workers' inbox; `None` when rasterizing inline.
        let inbox = self.pool.borrow().as_ref().map(|(tasks, _)| tasks.clone());
        let mut handed = false;
        while let Some(job) = self.queued.borrow_mut().pop_front() {
            if !self.wants(&job) {
                continue;
            }
            let geom = geom_of(&job.el);
            let task = Task {
                el: job.el,
                fp: job.fp,
                scale: job.scale,
                geom,
            };
            match &inbox {
                Some(tasks) => {
                    self.outstanding.set(self.outstanding.get() + 1);
                    if let Err(unsent) = tasks.send(task) {
                        // The workers are gone: this one is made inline after
                        // all, and so is everything from here on.
                        *self.pool.borrow_mut() = None;
                        self.outstanding.set(self.outstanding.get() - 1);
                        self.raster_inline(unsent.0);
                    }
                }
                None => self.raster_inline(task),
            }
            handed = true;
        }
        handed
    }

    fn raster_inline(&self, task: Task) {
        self.deliver(raster(&task));
    }

    /// Take back what the workers finished since the last call.
    fn collect(&self) {
        let pool = self.pool.borrow();
        let Some((_, finished)) = pool.as_ref() else {
            return;
        };
        while let Ok(done) = finished.try_recv() {
            self.outstanding
                .set(self.outstanding.get().saturating_sub(1));
            self.deliver(done);
        }
    }

    /// Keep a finished wash if its element still waits for it.
    fn deliver(&self, done: Done) {
        let mut entries = self.entries.borrow_mut();
        let Some(ent) = entries.get_mut(&done.id) else {
            return;
        };
        if !ent.queued || ent.seen != done.fp {
            // Made for an earlier state of the element (restyled meanwhile, or
            // removed and added again). It was never uploaded: dropping it is
            // all it takes.
            return;
        }
        ent.queued = false;
        self.retire(&ent.image);
        ent.image = done.wash.map(Arc::new);
        ent.image_fp = done.fp;
        ent.image_scale = done.scale;
    }

    /// Whether any wash is queued or being made: the board keeps asking for
    /// frames until none is.
    pub fn busy(&self) -> bool {
        !self.queued.borrow().is_empty() || self.outstanding.get() > 0
    }

    /// Drop the washes of elements that no longer exist in the scene (deleted,
    /// or swapped out by undo/redo). Runs each frame before painting.
    pub fn retain_scene(&self, scene: &Scene) {
        self.entries.borrow_mut().retain(|id, ent| {
            let alive = scene.get(*id).is_some();
            if !alive {
                self.retire(&ent.image);
            }
            alive
        });
        self.queued
            .borrow_mut()
            .retain(|job| scene.get(job.el.id).is_some());
    }

    /// Retired images pending atlas eviction; drained by the paint phase.
    pub fn take_stale(&self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut *self.stale.borrow_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::cache::RenderCache;
    use crate::render::rough::world_geometry;
    use crate::scene::{ElementKind, ElementStyle, FillStyle};
    use std::time::{Duration, Instant};

    fn watercolor(bounds: WBounds, color: u32, opacity: f32) -> Element {
        let mut el = Element::new(
            ElementKind::Rectangle,
            bounds,
            ElementStyle {
                background: Some(color),
                fill_style: FillStyle::Watercolor,
                opacity,
                ..Default::default()
            },
        );
        el.seed = 20261125;
        el
    }

    /// Pixels of `el`'s wash, with its rect and resolution.
    fn pixels(el: &Element, px_per_world: f64) -> (image::RgbaImage, WBounds, f64) {
        wash_pixels(
            &world_geometry(el),
            el.style.background.unwrap(),
            el.seed,
            px_per_world,
            (el.bounds.x, el.bounds.y),
        )
        .expect("a watercolor element has a wash")
    }

    /// The pixel holding a point of the element's own coordinates.
    fn pixel_at(
        img: &image::RgbaImage,
        rect: &WBounds,
        px_per_world: f64,
        x: f64,
        y: f64,
    ) -> [u8; 4] {
        let (px, py) = (
            ((x - rect.x) * px_per_world).floor() as u32,
            ((y - rect.y) * px_per_world).floor() as u32,
        );
        img.get_pixel(px.min(img.width() - 1), py.min(img.height() - 1))
            .0
    }

    /// Mean alpha over a patch of the element's own coordinates.
    fn mean_alpha(
        img: &image::RgbaImage,
        rect: &WBounds,
        px_per_world: f64,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    ) -> f32 {
        let (mut sum, mut n) = (0f32, 0u32);
        let mut y = y0;
        while y < y1 {
            let mut x = x0;
            while x < x1 {
                sum += pixel_at(img, rect, px_per_world, x, y)[3] as f32;
                n += 1;
                x += 0.5;
            }
            y += 0.5;
        }
        sum / n as f32
    }

    #[test]
    fn a_bucket_is_at_least_the_scale_and_not_half_again_more() {
        for scale in [0.1, 0.3, 0.5, 0.74, 0.75, 0.9, 1.0, 1.3, 1.5, 2.7, 8.0] {
            let bucket = bucket_scale(scale);
            let least = scale * PIXELS_PER_SCREEN_PIXEL;
            assert!(bucket >= least - 1e-9, "{scale} -> {bucket}");
            assert!(bucket <= least * 1.5 + 1e-9, "{scale} -> {bucket}");
        }
        // Zooming inside a bucket re-rasterizes nothing.
        assert_eq!(bucket_scale(1.0), bucket_scale(1.3));
        assert_ne!(bucket_scale(1.0), bucket_scale(2.0));
    }

    #[test]
    fn a_wash_covers_the_shape_and_fades_out_past_its_outline() {
        let el = watercolor(WBounds::new(0.0, 0.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let (img, rect, q) = pixels(&el, 2.0);
        assert_eq!(q, 2.0);
        assert!(
            img.width() > 400 && img.height() > 200,
            "{:?}",
            img.dimensions()
        );
        // The middle is well covered (two glazes settle at 53-68%), in the
        // fill colour.
        let [b, g, r, a] = pixel_at(&img, &rect, q, 100.0, 50.0);
        assert!((120..=180).contains(&a), "middle alpha {a}");
        for (got, want) in [(r, 0x2f), (g, 0x9e), (b, 0x44)] {
            assert!(got.abs_diff(want) < 12, "middle colour rgb({r},{g},{b})");
        }
        // Far outside nothing is painted.
        assert_eq!(pixel_at(&img, &rect, q, -PAD + 0.5, -PAD + 0.5)[3], 0);
        assert_eq!(pixel_at(&img, &rect, q, 250.0, 50.0)[3], 0);
        // Right at the outline it is about half covered, and it thins out
        // from inside to outside.
        let at_edge = mean_alpha(&img, &rect, q, -0.5, 40.0, 0.5, 60.0);
        assert!(
            (40.0..=215.0).contains(&at_edge),
            "alpha at the outline {at_edge}"
        );
        let inside = mean_alpha(&img, &rect, q, 4.0, 40.0, 6.0, 60.0);
        let outside = mean_alpha(&img, &rect, q, -6.0, 40.0, -4.0, 60.0);
        assert!(
            at_edge < inside && outside < at_edge,
            "{inside} {at_edge} {outside}"
        );
        // No step at the outline: crossing it, the alpha takes many values.
        let crossing: Vec<u8> = (50..52)
            .flat_map(|y| (-5..10).map(move |x| (x, y)))
            .map(|(x, y)| pixel_at(&img, &rect, q, x as f64, y as f64)[3])
            .collect();
        let mut levels = crossing.clone();
        levels.sort_unstable();
        levels.dedup();
        assert!(levels.len() >= 8, "the edge steps: {levels:?}");
        assert!(
            crossing.iter().all(|a| *a <= 250),
            "the wash stays translucent"
        );
    }

    #[test]
    fn a_wash_fills_a_shape_whose_outline_winds_around_twice() {
        // roughr's ellipse does; under the even-odd rule its middle would
        // count as outside and only a ring would be painted.
        let mut el = watercolor(WBounds::new(0.0, 0.0, 200.0, 120.0), 0x2f9e44, 1.0);
        el.kind = ElementKind::Ellipse;
        let (img, rect, q) = pixels(&el, 2.0);
        let middle = mean_alpha(&img, &rect, q, 70.0, 40.0, 130.0, 80.0);
        assert!(middle >= 120.0, "middle alpha {middle}");
    }

    #[test]
    fn pigment_pools_darker_along_the_edge() {
        let el = watercolor(WBounds::new(0.0, 0.0, 200.0, 100.0), 0xffd8a8, 1.0);
        let (img, rect, q) = pixels(&el, 2.0);
        let middle = pixel_at(&img, &rect, q, 100.0, 50.0);
        let near_edge = pixel_at(&img, &rect, q, 2.5, 50.0);
        for c in 0..3 {
            assert!(
                near_edge[c] < middle[c] - 15,
                "channel {c}: near the edge {}, in the middle {}",
                near_edge[c],
                middle[c]
            );
        }
        assert!(
            near_edge[3] > 200,
            "near the edge the wash is still covered"
        );
    }

    #[test]
    fn a_wash_is_deterministic_and_follows_opacity() {
        let opaque = watercolor(WBounds::new(0.0, 0.0, 200.0, 100.0), 0x1971c2, 1.0);
        let (first, rect, q) = pixels(&opaque, 2.0);
        let (again, _, _) = pixels(&opaque, 2.0);
        assert_eq!(first, again);

        let faint = watercolor(WBounds::new(0.0, 0.0, 200.0, 100.0), 0x1971c2, 0.4);
        let (faint, faint_rect, _) = pixels(&faint, 2.0);
        let (thin, thick) = (
            pixel_at(&faint, &faint_rect, q, 100.0, 50.0)[3],
            pixel_at(&first, &rect, q, 100.0, 50.0)[3],
        );
        assert!(thin < thick - 60, "faint {thin}, opaque {thick}");
    }

    #[test]
    fn a_wash_is_laid_out_in_its_own_coordinates() {
        let here = watercolor(WBounds::new(0.0, 0.0, 200.0, 100.0), 0xe8590c, 1.0);
        let mut there = here.clone();
        there.bounds = WBounds::new(5000.0, -3000.0, 200.0, 100.0);
        let (a, rect_a, q_a) = pixels(&here, 1.0);
        let (b, rect_b, q_b) = pixels(&there, 1.0);
        // The same pixels, the same place on the element.
        assert_eq!(a, b);
        assert_eq!(rect_a.x, rect_b.x);
        assert_eq!(rect_a.y, rect_b.y);
        assert_eq!((rect_a.w, rect_a.h), (rect_b.w, rect_b.h));
        assert_eq!(q_a, q_b);
        // And at exactly the resolution asked for.
        assert_eq!(q_a, 1.0);
    }

    #[test]
    fn a_huge_wash_stays_within_the_caps() {
        let el = watercolor(WBounds::new(0.0, 0.0, 20_000.0, 10_000.0), 0x2f9e44, 1.0);
        let (img, _, q) = pixels(&el, 4.0);
        assert!(img.width() <= MAX_SIDE && img.height() <= MAX_SIDE);
        assert!(img.width() as f64 * img.height() as f64 <= MAX_PIXELS * 1.1);
        assert!(q < 1.0, "a huge wash is rasterized coarser: {q}");
    }

    #[test]
    fn only_watercolor_geometry_makes_a_wash() {
        let mut plain = watercolor(WBounds::new(0.0, 0.0, 100.0, 80.0), 0x2f9e44, 1.0);
        plain.style.fill_style = FillStyle::Solid;
        assert!(wash_pixels(&world_geometry(&plain), 0x2f9e44, 1, 2.0, (0.0, 0.0)).is_none());
        assert!(wash_pixels(&WorldGeom::Empty, 0x2f9e44, 1, 2.0, (0.0, 0.0)).is_none());
    }

    #[test]
    fn a_wash_costs_tens_of_milliseconds_not_seconds() {
        let el = watercolor(WBounds::new(0.0, 0.0, 440.0, 200.0), 0x2f9e44, 1.0);
        let geom = world_geometry(&el);
        // As the board asks for it at zoom 1 (generous bound: the workers
        // have all the time a wash takes), and the worst case, a wash large
        // enough to hit the pixel cap.
        for (px_per_world, bound) in [(bucket_scale(1.0), 250), (bucket_scale(8.0), 1500)] {
            // Warm up, then time.
            let _ = rasterize(&geom, 0x2f9e44, el.seed, px_per_world, (0.0, 0.0));
            let start = Instant::now();
            let wash = rasterize(&geom, 0x2f9e44, el.seed, px_per_world, (0.0, 0.0)).unwrap();
            let took = start.elapsed();
            eprintln!(
                "wash 440x200 at {px_per_world} px/unit: {:?}x{:?} px in {took:?}",
                wash.image.size(0).width,
                wash.image.size(0).height
            );
            assert!(
                took < Duration::from_millis(bound),
                "rasterizing took {took:?}"
            );
        }
    }

    fn which(draw: &WashDraw) -> &'static str {
        match draw {
            WashDraw::Ready(_) => "Ready",
            WashDraw::Pending(None) => "Pending(None)",
            WashDraw::Pending(Some(_)) => "Pending(Some(_))",
        }
    }

    /// Let the cache make whatever is queued (inline, so at once).
    fn work(cache: &WashCache) -> bool {
        let render_cache = RenderCache::new();
        cache.work(|el| render_cache.geometry(el))
    }

    /// Bring `el`'s wash to `Ready` and return it: the cache has to see the
    /// element hold still for a frame, then the queued wash is made.
    fn settle(cache: &WashCache, el: &Element, scale: f64) -> Arc<WashImage> {
        assert!(matches!(cache.image(el, scale), WashDraw::Pending(_)));
        assert!(matches!(cache.image(el, scale), WashDraw::Pending(_)));
        assert!(cache.busy());
        assert!(work(cache));
        assert!(!cache.busy());
        match cache.image(el, scale) {
            WashDraw::Ready(image) => image,
            other => panic!("the wash should be at hand, got {}", which(&other)),
        }
    }

    #[test]
    fn a_wash_is_made_once_its_element_holds_still_and_kept_afterwards() {
        let cache = WashCache::inline();
        let el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        // Not on the first frame: the element may be mid-drag.
        assert!(matches!(cache.image(&el, 1.0), WashDraw::Pending(None)));
        assert!(!cache.busy());
        assert!(!work(&cache));

        let image = settle(&cache, &el, 1.0);
        assert_eq!(image.px_per_world, bucket_scale(1.0));
        for _ in 0..3 {
            match cache.image(&el, 1.0) {
                WashDraw::Ready(again) => assert!(Arc::ptr_eq(&image, &again)),
                other => panic!("{}", which(&other)),
            }
        }
    }

    #[test]
    fn moving_an_element_keeps_its_wash() {
        let cache = WashCache::inline();
        let mut el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let image = settle(&cache, &el, 1.0);
        el.bounds = WBounds::new(-4000.0, 9000.0, 200.0, 100.0);
        match cache.image(&el, 1.0) {
            WashDraw::Ready(again) => assert!(Arc::ptr_eq(&image, &again)),
            other => panic!("{}", which(&other)),
        }
        assert!(!cache.busy());
    }

    #[test]
    fn a_restyled_element_gets_a_new_wash_and_the_old_one_is_retired() {
        let cache = WashCache::inline();
        let mut el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let old = settle(&cache, &el, 1.0);
        assert!(cache.take_stale().is_empty());

        el.style.background = Some(0xe8590c);
        // The old wash covers while the new one is made.
        assert!(matches!(cache.image(&el, 1.0), WashDraw::Pending(Some(_))));
        let fresh = settle(&cache, &el, 1.0);
        assert!(!Arc::ptr_eq(&old, &fresh));
        let stale = cache.take_stale();
        assert!(stale.iter().any(|img| Arc::ptr_eq(&old.image, img)));
    }

    #[test]
    fn another_zoom_bucket_shows_the_old_wash_until_the_new_one_is_at_hand() {
        let cache = WashCache::inline();
        let el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let zoomed_out = settle(&cache, &el, 1.0);
        // Zooming inside the bucket changes nothing.
        match cache.image(&el, 1.3) {
            WashDraw::Ready(again) => assert!(Arc::ptr_eq(&zoomed_out, &again)),
            other => panic!("{}", which(&other)),
        }
        assert!(!cache.busy());

        // Across buckets the old wash is stretched meanwhile.
        assert!(matches!(cache.image(&el, 4.0), WashDraw::Pending(Some(_))));
        let zoomed_in = settle(&cache, &el, 4.0);
        assert_eq!(zoomed_in.px_per_world, bucket_scale(4.0));
        assert!(zoomed_in.image.size(0).width > zoomed_out.image.size(0).width);
        assert!(!cache.take_stale().is_empty());
    }

    #[test]
    fn zooming_that_goes_on_rasterizes_nothing() {
        let cache = WashCache::inline();
        let el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let old = settle(&cache, &el, 1.0);
        for step in 0..20 {
            let scale = 1.0 + step as f64 * 0.1;
            let draw = cache.image(&el, scale);
            // The wash of the bucket already at hand is shown as is, another
            // bucket's stretched — and either way nothing is rasterized.
            match bucket_scale(scale) == old.px_per_world {
                true => assert!(
                    matches!(draw, WashDraw::Ready(_)),
                    "{which}",
                    which = which(&draw)
                ),
                false => assert!(
                    matches!(draw, WashDraw::Pending(Some(_))),
                    "{which}",
                    which = which(&draw)
                ),
            }
            assert!(!cache.busy(), "rasterized mid-zoom at {scale}");
            assert!(!work(&cache));
        }
        // Stopped zooming: now it is made, once.
        let stopped = 1.0 + 19.0 * 0.1;
        let fresh = settle(&cache, &el, stopped);
        assert!(!Arc::ptr_eq(&old, &fresh));
    }

    #[test]
    fn an_element_that_keeps_changing_is_never_rasterized() {
        let cache = WashCache::inline();
        let mut el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        for i in 0..20 {
            el.bounds.w = 200.0 + i as f64;
            assert!(matches!(cache.image(&el, 1.0), WashDraw::Pending(None)));
            assert!(!cache.busy());
            assert!(!work(&cache));
        }
    }

    #[test]
    fn a_queued_wash_of_a_changed_element_is_dropped() {
        let cache = WashCache::inline();
        let mut el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let _ = cache.image(&el, 1.0);
        let _ = cache.image(&el, 1.0);
        assert!(cache.busy());
        el.bounds.w = 260.0;
        let _ = cache.image(&el, 1.0);
        assert!(!work(&cache));
        assert!(!cache.busy());
        assert!(matches!(cache.image(&el, 1.0), WashDraw::Pending(None)));
        // It is made once the element holds still again.
        let _ = settle(&cache, &el, 1.0);
    }

    #[test]
    fn a_wash_made_for_an_earlier_state_of_its_element_is_dropped() {
        let cache = WashCache::inline();
        let mut el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let _ = cache.image(&el, 1.0);
        let _ = cache.image(&el, 1.0);
        // The worker was handed the job, and gets on with it...
        let render_cache = RenderCache::new();
        let task = Task {
            el: el.clone(),
            fp: wash_fingerprint(&el),
            scale: bucket_scale(1.0),
            geom: render_cache.geometry(&el),
        };
        // ... but the element was restyled before it finished.
        el.style.background = Some(0xe8590c);
        let _ = cache.image(&el, 1.0);
        cache.deliver(raster(&task));
        assert!(matches!(cache.image(&el, 1.0), WashDraw::Pending(None)));
        assert!(cache.take_stale().is_empty());
    }

    #[test]
    fn washes_of_deleted_elements_are_retired() {
        let cache = WashCache::inline();
        let el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let image = settle(&cache, &el, 1.0);
        let mut scene = Scene::new();
        scene.add(el.clone());
        cache.retain_scene(&scene);
        assert!(cache.take_stale().is_empty());
        match cache.image(&el, 1.0) {
            WashDraw::Ready(again) => assert!(Arc::ptr_eq(&image, &again)),
            other => panic!("{}", which(&other)),
        }
        scene.remove(el.id);
        cache.retain_scene(&scene);
        let stale = cache.take_stale();
        assert!(stale.iter().any(|img| Arc::ptr_eq(&image.image, img)));
        assert!(matches!(cache.image(&el, 1.0), WashDraw::Pending(None)));
    }

    #[test]
    fn a_background_wash_arrives_without_the_calling_thread_making_it() {
        let cache = WashCache::new();
        let el = watercolor(WBounds::new(10.0, 20.0, 200.0, 100.0), 0x2f9e44, 1.0);
        let _ = cache.image(&el, 1.0);
        let _ = cache.image(&el, 1.0);
        let render_cache = RenderCache::new();
        // Handed over, not made: it is in flight, and nothing is at hand.
        assert!(cache.work(|el| render_cache.geometry(el)));
        assert!(cache.busy());
        assert!(matches!(cache.image(&el, 1.0), WashDraw::Pending(None)));

        let deadline = Instant::now() + Duration::from_secs(60);
        let image = loop {
            cache.work(|el| render_cache.geometry(el));
            if let WashDraw::Ready(image) = cache.image(&el, 1.0) {
                break image;
            }
            assert!(
                Instant::now() < deadline,
                "the background wash never arrived"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(image.px_per_world, bucket_scale(1.0));
        assert!(!cache.busy());
    }
}
