//! CPU rasterizer for canvas elements: the pixel-brush half of the board.
//!
//! A canvas element replays its committed strokes into a backing pixel
//! buffer every time its fingerprint changes (new stroke, resize, restyle).
//! This is the "web canvas" rendering model — effects are per-pixel math
//! (translucent layered washes, edge pooling, seeded stipple) that the
//! tessellated-path pipeline cannot express.
//!
//! The buffer is BGRA byte order (this platform's `RenderImage` convention —
//! mirrors `paper_tile`), straight alpha, and is wrapped in an
//! `Arc<RenderImage>` per version: gpui caches the GPU texture in the sprite
//! atlas keyed by the image id, so a stable fingerprint paints with zero
//! re-upload, and a changed fingerprint costs exactly one full upload.
//! [`CanvasCache`] owns that mapping and collects replaced images in
//! `stale` so the board's paint phase can free their atlas tiles
//! (`window.drop_image`) — without that, every stroke would leak a texture.

use std::{cell::RefCell, collections::HashMap, sync::Arc};

use gpui::RenderImage;

use crate::scene::{CanvasBrush, CanvasStroke, Element, ElementId, ElementKind, Scene};

/// Backing resolution: device pixels per world unit for a fresh canvas.
/// 2× buys headroom for modest zoom-in before pixels read as pixels.
pub const PX_PER_WORLD: f64 = 2.0;
/// Cap on backing pixels per side: bounds the texture upload and the CPU
/// raster cost. A canvas wider than MAX_SIDE_PX/PX_PER_WORLD world units
/// simply rasterizes at a lower backing resolution (slightly softer).
pub const MAX_SIDE_PX: u32 = 4096;

/// Backing buffer size for a canvas element's bounds (never zero, never
/// above MAX_SIDE_PX per side).
pub fn canvas_pixel_size(el: &Element) -> (u32, u32) {
    let w = ((el.bounds.w.max(0.5) * PX_PER_WORLD).round() as u32).clamp(1, MAX_SIDE_PX);
    let h = ((el.bounds.h.max(0.5) * PX_PER_WORLD).round() as u32).clamp(1, MAX_SIDE_PX);
    (w, h)
}

/// How long a committed watercolor stroke stays "wet" (blooming outward,
/// diluting, its pooled edges settling) after the pen lifts. Live-only
/// nicety: loaded scenes render settled.
pub const WET_MS: u64 = 2500;

/// Ease-out cubic for the wet→settled transition: fast initial bloom, long
/// gentle settle.
pub fn wet_ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Per-stroke settling factors (0 = just committed, 1 = settled) for a
/// canvas's strokes, from their commit instants. The vec is parallel to
/// `strokes` (missing/None entries count as settled — undo/redo restoring
/// older stroke counts degrades gracefully, and strokes loaded from disk
/// never animate).
pub fn wet_profile(committed: &[Option<std::time::Instant>], now: std::time::Instant) -> Vec<f32> {
    committed
        .iter()
        .map(|c| match c {
            Some(t) => wet_ease(now.duration_since(*t).as_millis() as f32 / WET_MS as f32),
            None => 1.0,
        })
        .collect()
}

/// Rasterize a canvas element: surface fill + every stroke in order.
/// Deterministic in the element's seed (jittered brushes are seeded per
/// stroke), so the same element always rasterizes to the same pixels.
pub fn rasterize(el: &Element) -> image::RgbaImage {
    rasterize_with(el, &[])
}

/// [`rasterize`] with per-stroke wet factors (`wet[i] < 1` makes stroke i
/// render mid-bloom: narrower but more concentrated, edges uneven). An
/// empty/short profile renders every stroke settled.
pub fn rasterize_with(el: &Element, wet: &[f32]) -> image::RgbaImage {
    let (pw, ph) = canvas_pixel_size(el);
    let mut buf = vec![0u8; pw as usize * ph as usize * 4];
    if let Some(bg) = el.style.background {
        let (r, g, b) = rgb(bg);
        for px in buf.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 0xff;
        }
    }
    let strokes = match &el.kind {
        ElementKind::Canvas { strokes } => strokes,
        _ => return image::RgbaImage::from_raw(pw, ph, buf).expect("canvas buffer size"),
    };
    // World→pixel scale: each axis maps through its own bound so oversized
    // canvases (clamped at MAX_SIDE_PX) stay aligned; stroke widths use the
    // average scale so non-uniform resizes keep round pens round-ish.
    let sx = pw as f32 / el.bounds.w.max(0.5) as f32;
    let sy = ph as f32 / el.bounds.h.max(0.5) as f32;
    let wscale = (sx + sy) * 0.5;
    for (i, s) in strokes.iter().enumerate() {
        // Per-stroke seed: mix the element seed with the stroke index so
        // adjacent strokes never share a jitter pattern.
        let seed = el
            .seed
            .wrapping_add((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
            ^ ((s.points.len() as u64) << 3);
        let settle = wet.get(i).copied().unwrap_or(1.0);
        draw_stroke(&mut buf, pw, ph, s, sx, sy, wscale, seed, settle);
    }
    image::RgbaImage::from_raw(pw, ph, buf).expect("canvas buffer size")
}

fn rgb(color: u32) -> (u8, u8, u8) {
    (
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
    )
}

/// Deterministic per-stroke RNG: LCG taking the top bits (visual jitter
/// only — no cryptographic ambitions).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Splitmix-style avalanche so close seeds diverge.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng(z ^ (z >> 31))
    }
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32) / (1u64 << 31) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + self.unit() * (hi - lo)
    }
    fn signed(&mut self, m: f32) -> f32 {
        self.range(-m, m)
    }
}

/// Source-over blend of a straight-alpha color into the BGRA buffer.
/// Out-of-range pixels are dropped — the buffer edge is the canvas clip.
fn blend(buf: &mut [u8], pw: u32, ph: u32, x: i32, y: i32, r: u8, g: u8, b: u8, alpha: f32) {
    if !(0.004..=1.0).contains(&alpha) || x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as u32, y as u32);
    if x >= pw || y >= ph {
        return;
    }
    let i = ((y * pw + x) * 4) as usize;
    let dst_a = buf[i + 3] as f32 / 255.0;
    let out_a = alpha + dst_a * (1.0 - alpha);
    let mix = |src: u8, dst: u8| {
        ((src as f32 * alpha + dst as f32 * dst_a * (1.0 - alpha)) / out_a).round() as u8
    };
    buf[i] = mix(b, buf[i]); // B
    buf[i + 1] = mix(g, buf[i + 1]); // G
    buf[i + 2] = mix(r, buf[i + 2]); // R
    buf[i + 3] = (out_a * 255.0).round() as u8;
}

fn draw_stroke(
    buf: &mut [u8],
    pw: u32,
    ph: u32,
    s: &CanvasStroke,
    sx: f32,
    sy: f32,
    wscale: f32,
    seed: u64,
    settle: f32,
) {
    if s.points.is_empty() {
        return;
    }
    let pts: Vec<(f32, f32)> = s
        .points
        .iter()
        .map(|p| (p.x as f32 * sx, p.y as f32 * sy))
        .collect();
    // Half-width per point in px: base width × ratio (uniform when `widths`
    // is empty), clamped so hairlines still stamp and spikes stay sane.
    let base_half = (s.width as f32 * wscale * 0.5).max(0.6);
    let half: Vec<f32> = (0..pts.len())
        .map(|i| {
            let ratio = s.widths.get(i).copied().unwrap_or(1.0) as f32;
            (base_half * ratio.clamp(0.05, 8.0)).max(0.5)
        })
        .collect();
    let (r, g, b) = rgb(s.color);
    // Wash-family brushes draw through a smoothed, densified centerline:
    // raw AI/hand polylines carry hard zigzag corners that read as deco
    // pattern on a wide translucent band (目验 62 分轮的板条感根因之一).
    // Ink keeps the crisp vertex-to-vertex geometry.
    let (pts, half) = match s.brush {
        CanvasBrush::Ink => (pts, half),
        _ => smooth_centerline(&pts, &half),
    };
    match s.brush {
        CanvasBrush::Ink => {
            // Distance-field anti-aliasing: alpha ramps over the 1px band
            // straddling the stroke edge. One pass, crisp result.
            for_each_near_path(&pts, &half, (0.0, 0.0), 1.0, |x, y, d, h| {
                let a = (h + 0.5 - d).clamp(0.0, 1.0) * s.opacity;
                blend(buf, pw, ph, x, y, r, g, b, a);
            });
        }
        CanvasBrush::Watercolor => {
            // Layered wash: 4 translucent passes. Each layer displaces the
            // whole centerline by a *low-frequency* wander (per-point, smoothed
            // noise along the path — not a uniform shift: a shifted straight
            // band is still a straight band, which is exactly the slat
            // artifact) and modulates its width along the path, so layer
            // rims cross instead of stacking into parallel stripes. A pooling
            // band at each layer's rim darkens edges; its width scales with
            // the stroke (a fixed 2.5px band on a 20px wash reads as an
            // outline). A final wide halo bleeds the wet edge outward.
            //
            // Wet strokes (settle < 1, the 落纸晕开 bloom after the pen
            // lifts) start narrower but more concentrated, with stronger
            // uneven spread; over WET_MS they bloom to full width while
            // diluting to the settled wash.
            let bloom = 0.72 + 0.28 * settle; // width: blooms outward
            let dilute = 1.18 - 0.18 * settle; // alpha: lightens as it spreads
            let wander = 1.6 - 0.6 * settle; // wet spread is uneven
            let mut rng = Rng::new(seed);
            // Wet-on-wet: where this wash meets pigment already on the
            // paper, pigment migrates across the contact instead of
            // pooling at its own rim — stacked bands blend at the seam
            // instead of showing double rims with a pale gap between.
            let base_alpha: Vec<u8> = buf.chunks_exact(4).map(|c| c[3]).collect();
            for _ in 0..4 {
                let layer_pts = wander_path(&pts, &half, wander, &mut rng);
                let layer_half: Vec<f32> = half
                    .iter()
                    .map(|h| h * bloom * rng.range(0.88, 1.12))
                    .collect();
                let base = rng.range(0.08, 0.13) * dilute;
                let pool_w = half.iter().copied().fold(1.0, f32::max).max(2.5);
                for_each_near_path(&layer_pts, &layer_half, (0.0, 0.0), 1.0, |x, y, d, h| {
                    let rim = (h + 1.0 - d).clamp(0.0, 1.0);
                    let pool0 = 1.3 * ((d - (h - pool_w)) / pool_w).clamp(0.0, 1.0);
                    let ba = base_alpha
                        .get((y as u32 * pw + x as u32) as usize)
                        .copied()
                        .unwrap_or(0) as f32
                        / 255.0;
                    let wet = (ba / 0.35).min(1.0);
                    let pool = 1.0 + pool0 * (1.0 - 0.7 * wet);
                    let a = base * pool * rim * s.opacity * (1.0 + 0.35 * wet);
                    blend(buf, pw, ph, x, y, r, g, b, a);
                });
            }
            let halo = wander_path(&pts, &half, 0.5 * wander, &mut rng);
            let halo_half: Vec<f32> = half.iter().map(|h| h * 1.5 * bloom).collect();
            for_each_near_path(&halo, &halo_half, (0.0, 0.0), 1.0, |x, y, d, h| {
                let rim = (h + 1.0 - d).clamp(0.0, 1.0);
                blend(buf, pw, ph, x, y, r, g, b, 0.04 * dilute * rim * s.opacity);
            });
        }
        CanvasBrush::DryBrush => {
            // 飞白：a faint continuous core so the stroke reads connected,
            // then seeded stipple — small AA discs scattered along the path,
            // denser near the centerline, streaky toward the edges.
            for_each_near_path(&pts, &half, (0.0, 0.0), 0.6, |x, y, d, h| {
                let a = (h + 0.5 - d).clamp(0.0, 1.0) * 0.12 * s.opacity;
                blend(buf, pw, ph, x, y, r, g, b, a);
            });
            let mut rng = Rng::new(seed);
            // Scratch/deposit decisions must look at the canvas as it was
            // BEFORE this stroke: sampling the live buffer would make the
            // stroke's own overlapping discs scratch each other away (the
            // 74-round regression where glints vanished).
            let base_alpha: Vec<u8> = buf.chunks_exact(4).map(|c| c[3]).collect();
            dry_stipple(
                buf, pw, ph, &pts, &half, r, g, b, s.opacity, &mut rng, &base_alpha, pw,
            );
        }
    }
}

/// Corner-rounding + arc-length densification of a stroke centerline for
/// wash-family brushes. Two stages: one Chaikin corner-cutting pass pulls
/// hard vertices inward (a Catmull-Rom spline alone would interpolate the
/// corner exactly and keep the deco zigzag), then the Catmull-Rom spline
/// (same convention as the vector curve renderer: tightness 0, cubic Bézier
/// control points at 1/6 of the neighbor span, endpoints duplicated) adds
/// the intermediate vertices the per-point wander needs to breathe. Widths
/// interpolate linearly through both stages.
fn smooth_centerline(pts: &[(f32, f32)], half: &[f32]) -> (Vec<(f32, f32)>, Vec<f32>) {
    let n = pts.len();
    if n < 2 {
        return (pts.to_vec(), half.to_vec());
    }
    if n == 2 {
        // A straight stroke has no corners to cut, but the washes' wander
        // needs interior points to undulate through — sample the chord.
        let (p, q) = (pts[0], pts[1]);
        let (hp, hq) = (half[0], half[1]);
        let len = (q.0 - p.0).hypot(q.1 - p.1);
        let k = ((len / 6.0).ceil() as usize).clamp(2, 64);
        let mut out_pts = Vec::with_capacity(k + 1);
        let mut out_half = Vec::with_capacity(k + 1);
        for j in 0..=k {
            let t = j as f32 / k as f32;
            out_pts.push((p.0 + (q.0 - p.0) * t, p.1 + (q.1 - p.1) * t));
            out_half.push(hp + (hq - hp) * t);
        }
        return (out_pts, out_half);
    }
    // Chaikin: replace each interior vertex by the pair of points 1/4 in
    // from it along each incident segment (R of the incoming segment, Q of
    // the outgoing one) — the classic corner cut that pulls sharp zigzags
    // into curves while endpoints stay put.
    let mut cut_pts: Vec<(f32, f32)> = Vec::with_capacity(n * 2);
    let mut cut_half: Vec<f32> = Vec::with_capacity(n * 2);
    cut_pts.push(pts[0]);
    cut_half.push(half[0]);
    for i in 1..n - 1 {
        let (a, b, c) = (pts[i - 1], pts[i], pts[i + 1]);
        let (ha, hb, hc) = (half[i - 1], half[i], half[i + 1]);
        cut_pts.push((a.0 * 0.25 + b.0 * 0.75, a.1 * 0.25 + b.1 * 0.75));
        cut_half.push(ha * 0.25 + hb * 0.75);
        cut_pts.push((b.0 * 0.75 + c.0 * 0.25, b.1 * 0.75 + c.1 * 0.25));
        cut_half.push(hb * 0.75 + hc * 0.25);
    }
    cut_pts.push(pts[n - 1]);
    cut_half.push(half[n - 1]);

    let m = cut_pts.len();
    let last = m - 1;
    let mut out_pts = Vec::with_capacity(m * 6);
    let mut out_half = Vec::with_capacity(m * 6);
    out_pts.push(cut_pts[0]);
    out_half.push(cut_half[0]);
    for seg in 0..last {
        let p0 = cut_pts[seg.saturating_sub(1)];
        let p1 = cut_pts[seg];
        let p2 = cut_pts[seg + 1];
        let p3 = cut_pts[(seg + 2).min(last)];
        let h1 = cut_half[seg];
        let h2 = cut_half[seg + 1];
        let c1 = (p1.0 + (p2.0 - p0.0) / 6.0, p1.1 + (p2.1 - p0.1) / 6.0);
        let c2 = (p2.0 - (p3.0 - p1.0) / 6.0, p2.1 + (p3.1 - p1.1) / 6.0);
        let seg_len = (p2.0 - p1.0).hypot(p2.1 - p1.1);
        let k = ((seg_len / 6.0).ceil() as usize).clamp(2, 24);
        for j in 1..=k {
            let t = j as f32 / k as f32;
            let u = 1.0 - t;
            let x = u * u * u * p1.0 + 3.0 * u * u * t * c1.0 + 3.0 * u * t * t * c2.0 + t * t * t * p2.0;
            let y = u * u * u * p1.1 + 3.0 * u * u * t * c1.1 + 3.0 * u * t * t * c2.1 + t * t * t * p2.1;
            out_pts.push((x, y));
            out_half.push(h1 + (h2 - h1) * t);
        }
    }
    (out_pts, out_half)
}

/// Low-frequency wander: displace each centerline point along the local
/// normal by smoothed (two box passes) seeded noise scaled to the local
/// half-width and `strength`. The result is an organic edge line instead of
/// a rigidly shifted copy of the path.
fn wander_path(
    pts: &[(f32, f32)],
    half: &[f32],
    strength: f32,
    rng: &mut Rng,
) -> Vec<(f32, f32)> {
    let n = pts.len();
    if n < 2 {
        return pts.to_vec();
    }
    // Arc-length parameter so noise frequencies scale with the stroke:
    // point-count-based smoothing gave every stroke the same ~20px scallop
    // period (the caterpillar rim on long wave bands).
    let mut arc = vec![0.0f32; n];
    for i in 1..n {
        arc[i] = arc[i - 1] + (pts[i].0 - pts[i - 1].0).hypot(pts[i].1 - pts[i - 1].1);
    }
    let total = arc[n - 1].max(1.0);
    // Three incommensurate sine components (long swell + medium + texture)
    // with seeded phases/frequencies, wavelengths as fractions of the
    // stroke length…
    let comps: [(f32, f32, f32); 3] = [
        (rng.range(0.45, 0.75), rng.range(0.0, 6.283), 1.0),
        (rng.range(0.20, 0.33), rng.range(0.0, 6.283), 0.55),
        (rng.range(0.09, 0.15), rng.range(0.0, 6.283), 0.28),
    ];
    let norm: f32 = comps.iter().map(|c| c.2).sum();
    let mut off: Vec<f32> = (0..n)
        .map(|i| {
            let s = arc[i] / total;
            let v: f32 = comps
                .iter()
                .map(|(wl, ph, a)| a * (6.283 * s / wl + ph).sin())
                .sum();
            v / norm
        })
        .collect();
    // …plus a touch of per-point grain so the edge never looks machined.
    for i in 0..n {
        off[i] = (off[i] + 0.18 * rng.signed(1.0)).clamp(-1.0, 1.0);
    }
    (0..n)
        .map(|i| {
            let (a, b) = (pts[i.saturating_sub(1)], pts[(i + 1).min(n - 1)]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = dx.hypot(dy);
            let (nx, ny) = if len > 1e-3 { (-dy / len, dx / len) } else { (0.0, 1.0) };
            // Amplitude grows super-linearly with half-width: a 60px-tall
            // sky band needs decimetre-scale undulation to stop reading as
            // a ruler-straight slat, while a 4px line stays controlled.
            let amp = half[i] * (0.45 + half[i] * 0.02).min(1.1) * strength * off[i];
            (pts[i].0 + nx * amp, pts[i].1 + ny * amp)
        })
        .collect()
}

/// Seeded 飞白 stipple: AA discs along each segment, scattered across the
/// stroke width (dense at the center, streaky at the rim).
fn dry_stipple(
    buf: &mut [u8],
    pw: u32,
    ph: u32,
    pts: &[(f32, f32)],
    half: &[f32],
    r: u8,
    g: u8,
    b: u8,
    opacity: f32,
    rng: &mut Rng,
    base_alpha: &[u8],
    base_pw: u32,
) {
    let stamp_disc = |buf: &mut [u8], cx: f32, cy: f32, rad: f32, a0: f32| {
        let x0 = (cx - rad - 1.0).floor() as i32;
        let x1 = (cx + rad + 1.0).ceil() as i32;
        let y0 = (cy - rad - 1.0).floor() as i32;
        let y1 = (cy + rad + 1.0).ceil() as i32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = ((x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy) - rad).max(0.0);
                let a = (1.0 - d).clamp(0.0, 1.0) * a0;
                blend(buf, pw, ph, x, y, r, g, b, a);
            }
        }
    };
    // Destination-out disc: scale the existing alpha down so the paper shows
    // through — a dry brush dragging over a wet wash lifts pigment instead
    // of adding it (真飞白). Color is left in place; only coverage drops.
    let scratch_disc = |buf: &mut [u8], cx: f32, cy: f32, rad: f32, k0: f32| {
        let x0 = (cx - rad - 1.0).floor() as i32;
        let x1 = (cx + rad + 1.0).ceil() as i32;
        let y0 = (cy - rad - 1.0).floor() as i32;
        let y1 = (cy + rad + 1.0).ceil() as i32;
        for y in y0.max(0)..=y1.min(ph as i32 - 1) {
            for x in x0.max(0)..=x1.min(pw as i32 - 1) {
                let d = ((x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy) - rad).max(0.0);
                let k = (1.0 - d).clamp(0.0, 1.0) * k0;
                let i = ((y as u32 * pw + x as u32) * 4 + 3) as usize;
                buf[i] = (buf[i] as f32 * (1.0 - k)).round() as u8;
            }
        }
    };
    let dest_alpha = |x: f32, y: f32| -> f32 {
        let (xi, yi) = (x as i32, y as i32);
        if xi < 0 || yi < 0 || xi >= base_pw as i32 || yi >= ph as i32 {
            return 0.0;
        }
        base_alpha[(yi as u32 * base_pw + xi as u32) as usize] as f32 / 255.0
    };
    if pts.len() == 1 {
        stamp_disc(buf, pts[0].0, pts[0].1, half[0], 0.8 * opacity);
        return;
    }
    for seg in 0..pts.len() - 1 {
        let (ax, ay) = pts[seg];
        let (bx, by) = pts[seg + 1];
        let ha = half[seg];
        let hb = half[seg + 1];
        let (dx, dy) = (bx - ax, by - ay);
        let len = dx.hypot(dy);
        let count = ((len / 2.2).ceil() as usize).max(2);
        let (nx, ny) = if len > 1e-3 {
            (-dy / len, dx / len)
        } else {
            (1.0, 0.0)
        };
        for k in 0..count {
            let t = (k as f32 + rng.unit()) / count as f32;
            let h = ha + (hb - ha) * t;
            let spread = rng.signed(0.75) * h;
            let cx = ax + dx * t + nx * spread;
            let cy = ay + dy * t + ny * spread;
            let rad = h * rng.range(0.14, 0.48);
            // Adaptive: over existing coverage the dry brush scratches
            // (streaks of paper); on bare canvas it deposits bold grains so
            // the stroke never dissolves into an underlying wash.
            if dest_alpha(cx, cy) > 0.18 {
                scratch_disc(buf, cx, cy, rad, rng.range(0.35, 0.7) * opacity);
            } else {
                let a0 = rng.range(0.45, 0.9) * opacity;
                stamp_disc(buf, cx, cy, rad, a0);
            }
        }
    }
}

#[inline]
fn seg_dist(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let len2 = abx * abx + aby * aby;
    if len2 < 1e-6 {
        return ((px - a.0).hypot(py - a.1), 0.0);
    }
    let t = (((px - a.0) * abx + (py - a.1) * aby) / len2).clamp(0.0, 1.0);
    ((px - (a.0 + abx * t)).hypot(py - (a.1 + aby * t)), t)
}

/// Coarse uniform grid bucketing segments so each pixel only tests the few
/// that could plausibly be near. Segments are registered in every cell
/// their bbox inflated by `reach` overlaps, which makes a single-cell query
/// per pixel sufficient (any segment within `reach` of the pixel must cover
/// the pixel's own cell).
struct SegGrid {
    cell: f32,
    ox: f32,
    oy: f32,
    cols: i32,
    rows: i32,
    cells: Vec<Vec<u32>>,
}

impl SegGrid {
    fn build(pts: &[(f32, f32)], reach: f32) -> Self {
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for p in pts {
            min_x = min_x.min(p.0);
            min_y = min_y.min(p.1);
            max_x = max_x.max(p.0);
            max_y = max_y.max(p.1);
        }
        // Cell size: comfortably larger than the query reach, and large
        // enough that the grid never explodes for huge bounding boxes.
        let area = (max_x - min_x + 1.0).max(1.0) * (max_y - min_y + 1.0).max(1.0);
        let mut cell = (reach * 2.0).max(16.0);
        if cell * cell < area / 262_144.0 {
            cell = (area / 262_144.0).sqrt().ceil();
        }
        // Origin sits `reach` outside the point bbox: pixels within reach of
        // the geometry (the only ones that can plot) must map to a valid
        // cell — an origin ON the bbox would send them to negative rows.
        let ox = min_x - reach;
        let oy = min_y - reach;
        let cols = (((max_x + reach - ox) / cell).ceil() as i32 + 1).max(1);
        let rows = (((max_y + reach - oy) / cell).ceil() as i32 + 1).max(1);
        let mut grid = SegGrid {
            cell,
            ox,
            oy,
            cols,
            rows,
            cells: vec![Vec::new(); (cols as usize) * (rows as usize)],
        };
        for (i, w) in pts.windows(2).enumerate() {
            let (ax, ay) = w[0];
            let (bx, by) = w[1];
            let (cx0, cx1) = (ax.min(bx) - reach, ax.max(bx) + reach);
            let (cy0, cy1) = (ay.min(by) - reach, ay.max(by) + reach);
            let c0 = (((cx0 - grid.ox) / cell).floor() as i32).max(0);
            let c1 = (((cx1 - grid.ox) / cell).floor() as i32).min(cols - 1);
            let r0 = (((cy0 - grid.oy) / cell).floor() as i32).max(0);
            let r1 = (((cy1 - grid.oy) / cell).floor() as i32).min(rows - 1);
            for row in r0..=r1 {
                for col in c0..=c1 {
                    grid.cells[(row * cols + col) as usize].push(i as u32);
                }
            }
        }
        grid
    }

    fn at(&self, x: f32, y: f32) -> &[u32] {
        let col = ((x - self.ox) / self.cell).floor() as i32;
        let row = ((y - self.oy) / self.cell).floor() as i32;
        if col < 0 || row < 0 || col >= self.cols || row >= self.rows {
            &[]
        } else {
            &self.cells[(row * self.cols + col) as usize]
        }
    }
}

/// For each buffer pixel near the polyline, call `plot(x, y, dist, half)`
/// where `dist` is the pixel-center distance to the nearest segment (in
/// path space), `half` that segment's interpolated half-width scaled by
/// `width_scale`, and the whole path is shifted by `off` (layer jitter).
/// Pixels farther from every segment than their own half-width are skipped.
fn for_each_near_path(
    pts: &[(f32, f32)],
    half: &[f32],
    off: (f32, f32),
    width_scale: f32,
    mut plot: impl FnMut(i32, i32, f32, f32),
) {
    let max_half = half.iter().copied().fold(0.5, f32::max) * width_scale;
    let reach = max_half + 2.0;
    let grid = SegGrid::build(pts, reach);
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for p in pts {
        min_x = min_x.min(p.0);
        min_y = min_y.min(p.1);
        max_x = max_x.max(p.0);
        max_y = max_y.max(p.1);
    }
    let x0 = ((min_x + off.0 - reach).floor() as i32).max(0);
    let x1 = ((max_x + off.0 + reach).ceil() as i32).min(i32::MAX / 2);
    let y0 = ((min_y + off.1 - reach).floor() as i32).max(0);
    let y1 = ((max_y + off.1 + reach).ceil() as i32).min(i32::MAX / 2);
    for y in y0..=y1 {
        for x in x0..=x1 {
            // Pixel center mapped into path space (undoing the layer offset).
            let px = x as f32 + 0.5 - off.0;
            let py = y as f32 + 0.5 - off.1;
            let mut best = f32::INFINITY;
            let mut best_h = 0.0f32;
            for &si in grid.at(px, py) {
                let (d, t) = seg_dist(px, py, pts[si as usize], pts[si as usize + 1]);
                if d < best {
                    best = d;
                    best_h = half[si as usize]
                        + (half[si as usize + 1] - half[si as usize]) * t;
                }
            }
            if best.is_finite() {
                plot(x, y, best, best_h * width_scale);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Texture cache
// ---------------------------------------------------------------------------

/// Cache capacity. A board with more live canvases than this recomputes on
/// overflow (correct, rare).
const MAX_ENTRIES: usize = 512;

struct CanvasEntry {
    fingerprint: u64,
    image: Arc<RenderImage>,
}

/// Per-canvas texture cache, owned by the board view. Keyed by element id
/// and validated by the same fingerprint the render cache uses; the pixel
/// buffer is re-rasterized only when something rendering-relevant changed.
///
/// `stale` collects images that were replaced (new stroke, resize, delete):
/// the paint phase drains it into `window.drop_image` so the sprite atlas
/// actually frees the old textures. Draining happens after the new frame's
/// `paint_image` calls — the stale images are never referenced by the
/// current scene, so freeing them mid-paint is safe.
///
/// Wet-stroke animation (落纸晕开) lives here too, outside the settled
/// cache: `wet` holds per-stroke commit instants, `anim_frames` holds the
/// previous animation frame per canvas so exactly one extra atlas texture
/// exists while blooming — each new frame stales its predecessor.
#[derive(Default)]
pub struct CanvasCache {
    entries: RefCell<HashMap<ElementId, CanvasEntry>>,
    stale: RefCell<Vec<Arc<RenderImage>>>,
    wet: RefCell<HashMap<ElementId, Vec<Option<std::time::Instant>>>>,
    anim_frames: RefCell<HashMap<ElementId, Arc<RenderImage>>>,
}

impl CanvasCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Texture for `el`, from cache when the fingerprint matches.
    pub fn image(&self, el: &Element) -> Arc<RenderImage> {
        let fp = crate::render::cache::fingerprint(el);
        if let Some(hit) = self.entries.borrow().get(&el.id) {
            if hit.fingerprint == fp {
                return hit.image.clone();
            }
        }
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(rasterize(el))]));
        let mut entries = self.entries.borrow_mut();
        if entries.len() >= MAX_ENTRIES {
            for entry in entries.values() {
                self.stale.borrow_mut().push(entry.image.clone());
            }
            entries.clear();
        }
        if let Some(old) = entries.insert(
            el.id,
            CanvasEntry {
                fingerprint: fp,
                image: image.clone(),
            },
        ) {
            self.stale.borrow_mut().push(old.image);
        }
        image
    }

    /// Record that a stroke was just committed to canvas `id`: watercolor
    /// strokes bloom (Some(now)), the others render settled immediately.
    /// The wet list is kept parallel to the element's strokes — shorter is
    /// fine (missing entries count as settled).
    pub fn stroke_committed(&self, id: ElementId, brush: CanvasBrush) {
        let mut wet = self.wet.borrow_mut();
        let list = wet.entry(id).or_default();
        list.push(match brush {
            CanvasBrush::Watercolor => Some(std::time::Instant::now()),
            _ => None,
        });
    }

    /// Seed wet state for a batch of AI strokes (same semantics as
    /// [`stroke_committed`], all starting to bloom now).
    pub fn seed_wet(&self, id: ElementId, brushes: &[CanvasBrush]) {
        let now = std::time::Instant::now();
        let mut wet = self.wet.borrow_mut();
        let list = wet.entry(id).or_default();
        for b in brushes {
            list.push(match b {
                CanvasBrush::Watercolor => Some(now),
                _ => None,
            });
        }
    }

    /// Animation frame for `el` when any of its strokes is still wet:
    /// rasterizes with the current [`wet_profile`] (bypassing the settled
    /// entries — intermediate frames must not poison the fingerprint cache),
    /// stales the previous frame, and returns the frame plus `true` (the
    /// caller should keep requesting animation frames). Returns `None` once
    /// fully settled, after flushing any last animation frame to stale so
    /// the settled cache entry becomes the only live texture.
    pub fn image_animated(&self, el: &Element) -> Option<(Arc<RenderImage>, bool)> {
        let stroke_count = match &el.kind {
            ElementKind::Canvas { strokes } => strokes.len(),
            _ => return None,
        };
        let committed = self.wet.borrow().get(&el.id).cloned();
        let profile = match committed {
            Some(c) if !c.is_empty() => {
                // Keep the wet list parallel to the current strokes: undo /
                // redo / history swaps change the count; missing = settled.
                let mut c = c;
                c.truncate(stroke_count);
                wet_profile(&c, std::time::Instant::now())
            }
            _ => return None,
        };
        let still_wet = profile.iter().any(|&p| p < 1.0);
        if !still_wet {
            // Animation over: flush the last frame (if any) and let the
            // settled cache entry take over — settle == 1 renders identical
            // pixels, so the hand-off is seamless.
            if let Some(last) = self.anim_frames.borrow_mut().remove(&el.id) {
                self.stale.borrow_mut().push(last);
            }
            self.wet.borrow_mut().remove(&el.id);
            return None;
        }
        let frame = Arc::new(RenderImage::new(vec![image::Frame::new(rasterize_with(
            el, &profile,
        ))]));
        let last = self
            .anim_frames
            .borrow_mut()
            .insert(el.id, frame.clone());
        if let Some(last) = last {
            self.stale.borrow_mut().push(last);
        }
        Some((frame, true))
    }

    /// Drop cache entries for elements that no longer exist in the scene
    /// (deleted, or swapped out by undo/redo): their textures go stale so
    /// the atlas can reclaim them. Runs each frame before painting.
    pub fn retain_scene(&self, scene: &Scene) {
        self.entries.borrow_mut().retain(|id, entry| {
            let alive = scene.get(*id).is_some();
            if !alive {
                self.stale.borrow_mut().push(entry.image.clone());
            }
            alive
        });
        self.wet.borrow_mut().retain(|id, _| scene.get(*id).is_some());
        self.anim_frames.borrow_mut().retain(|id, img| {
            let alive = scene.get(*id).is_some();
            if !alive {
                self.stale.borrow_mut().push(img.clone());
            }
            alive
        });
    }

    /// Replaced textures pending atlas eviction; drained by the paint phase.
    pub fn take_stale(&self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut *self.stale.borrow_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{ElementKind, ElementStyle, WBounds, WPoint};

    fn canvas_with(stroke: CanvasStroke) -> Element {
        let mut el = Element::new(
            ElementKind::Canvas {
                strokes: vec![stroke],
            },
            WBounds::new(0.0, 0.0, 100.0, 100.0),
            ElementStyle::default(),
        );
        el.seed = 42;
        el
    }

    fn horizontal_stroke(brush: CanvasBrush) -> CanvasStroke {
        CanvasStroke {
            points: vec![WPoint::new(5.0, 50.0), WPoint::new(95.0, 50.0)],
            widths: Vec::new(),
            color: 0x0000ff,
            width: 8.0,
            brush,
            opacity: 1.0,
        }
    }

    /// Alpha of a pixel (BGRA buffer).
    fn alpha_at(img: &image::RgbaImage, x: u32, y: u32) -> u8 {
        img.get_pixel(x, y)[3]
    }

    #[test]
    fn ink_stroke_is_opaque_on_path_and_clipped_off_canvas() {
        let el = canvas_with(horizontal_stroke(CanvasBrush::Ink));
        let img = rasterize(&el);
        // 100×100 world → 200×200 px; the stroke runs at y=100px, half-width
        // 8px (world 8 × scale 2 / 2). Pixel centers quantize to .5, so the
        // AA band's last visible row is 107 (center d=7.5).
        assert!(alpha_at(&img, 100, 100) > 250, "core is opaque");
        assert!(alpha_at(&img, 100, 107) > 0, "stroke reaches its edge row");
        assert_eq!(alpha_at(&img, 100, 112), 0, "past the edge: empty");
        // Stroke that runs off the canvas clips at the buffer edge: points
        // far outside bounds simply produce no pixels outside.
        let out = canvas_with(CanvasStroke {
            points: vec![WPoint::new(-50.0, 50.0), WPoint::new(150.0, 50.0)],
            ..horizontal_stroke(CanvasBrush::Ink)
        });
        let img = rasterize(&out);
        assert!(alpha_at(&img, 0, 100) > 250, "clipped at x=0");
        assert!(alpha_at(&img, 199, 100) > 250, "clipped at the right edge");
    }

    #[test]
    fn watercolor_is_translucent_and_dry_brush_is_sparser_than_ink() {
        let ink = rasterize(&canvas_with(horizontal_stroke(CanvasBrush::Ink)));
        let wash = rasterize(&canvas_with(horizontal_stroke(CanvasBrush::Watercolor)));
        let dry = rasterize(&canvas_with(horizontal_stroke(CanvasBrush::DryBrush)));

        let center = alpha_at(&wash, 100, 100);
        assert!(center > 40, "wash deposits pigment");
        assert!(
            center < alpha_at(&ink, 100, 100),
            "wash stays translucent where ink is solid"
        );

        // Dry brush deposits less total pigment than a solid ink band (the
        // 飞白 look IS the missing coverage). Compare total alpha mass —
        // robust against individual overlapping stipple dots going dark.
        let mass = |img: &image::RgbaImage| -> u64 {
            img.pixels().map(|p| p[3] as u64).sum()
        };
        assert!(
            mass(&dry) < mass(&ink) / 2,
            "dry brush leaves well under half the ink's pigment"
        );
    }

    #[test]
    fn rasterize_is_deterministic_and_background_fills() {
        let a = rasterize(&canvas_with(horizontal_stroke(CanvasBrush::DryBrush)));
        let b = rasterize(&canvas_with(horizontal_stroke(CanvasBrush::DryBrush)));
        assert_eq!(a.as_raw(), b.as_raw(), "same element → same pixels");

        let mut el = canvas_with(horizontal_stroke(CanvasBrush::Ink));
        el.style.background = Some(0xfffaf0);
        let img = rasterize(&el);
        let p = img.get_pixel(5, 5); // BGRA
        assert_eq!((p[2], p[1], p[0], p[3]), (0xff, 0xfa, 0xf0, 0xff));
    }

    #[test]
    fn smooth_centerline_rounds_corners_and_densifies() {
        // A sharp V: the smoothed centerline must stay near the vertices at
        // the ends but pull the corner inward, and gain samples.
        let pts = vec![(0.0, 0.0), (50.0, 40.0), (100.0, 0.0)];
        let half = vec![4.0, 4.0, 4.0];
        let (sp, sh) = smooth_centerline(&pts, &half);
        assert!(sp.len() > pts.len() * 3, "densified: {}", sp.len());
        assert_eq!(sh.len(), sp.len());
        // Endpoints preserved.
        assert!((sp[0].0 - 0.0).abs() < 1e-3 && (sp[0].1 - 0.0).abs() < 1e-3);
        let last = sp[sp.len() - 1];
        assert!((last.0 - 100.0).abs() < 1e-3 && (last.1 - 0.0).abs() < 1e-3);
        // The corner vertex (50,40) is pulled toward the chord (y=0 line):
        // some sample near x=50 sits well below y=40 but above y=0.
        let near_corner = sp
            .iter()
            .filter(|p| (p.0 - 50.0).abs() < 6.0)
            .map(|p| p.1)
            .fold(0.0f32, f32::max);
        assert!(
            near_corner < 34.0 && near_corner > 5.0,
            "corner rounded inward, got {near_corner}"
        );
        // Straight strokes stay straight (collinear spline == chord).
        let (lp, _) = smooth_centerline(&[(0.0, 10.0), (100.0, 10.0)], &[3.0, 3.0]);
        assert!(lp.iter().all(|p| (p.1 - 10.0).abs() < 1e-3));
    }

    #[test]
    fn wander_displaces_bands_organically_not_rigidly() {
        // A straight horizontal band: a rigid shift would move every point
        // by the same offset; the wander must vary along the path (that is
        // what breaks the parallel-slat look).
        let pts: Vec<(f32, f32)> = (0..=20).map(|i| (i as f32 * 10.0, 50.0)).collect();
        let half = vec![8.0; 21];
        let mut rng = Rng::new(7);
        let w = wander_path(&pts, &half, 1.0, &mut rng);
        let dy: Vec<f32> = w.iter().zip(pts.iter()).map(|(a, b)| a.1 - b.1).collect();
        let spread = dy.iter().copied().fold(0.0f32, f32::max)
            - dy.iter().copied().fold(0.0f32, f32::min);
        assert!(
            spread > 2.0,
            "wander varies along the path (spread {spread}px)"
        );
        // Amplitude stays bounded by the (super-linear) half-width scaling.
        let cap = 8.0f32 * (0.45f32 + 8.0 * 0.02).min(1.1) + 1e-3;
        assert!(dy.iter().all(|d| d.abs() <= cap));
        // Deterministic for a given seed.
        let w2 = wander_path(&pts, &half, 1.0, &mut Rng::new(7));
        assert_eq!(w, w2);
    }

    #[test]
    fn watercolor_profile_numeric() {
        // Translucency profile of the wash: center must stay well below
        // opaque, and the pooling band must make the rim darker than the
        // core (pigment gathering at the wet edge).
        let el = canvas_with(horizontal_stroke(CanvasBrush::Watercolor));
        let img = rasterize(&el);
        let center = alpha_at(&img, 100, 100) as f32 / 255.0;
        let a = |y: u32| alpha_at(&img, 100, y) as f32 / 255.0;
        // Peak alpha anywhere in the column (the pooling rim).
        let mut peak = 0.0f32;
        for y in 80..120 {
            peak = peak.max(a(y));
        }
        assert!(
            center > 0.15 && center < 0.75,
            "wash center is translucent, got {center}"
        );
        assert!(
            peak < 0.95,
            "no pixel of the wash goes fully opaque, got {peak}"
        );
        assert!(
            peak > center + 0.08,
            "edge pooling makes the rim darker than the core: peak {peak} vs center {center}"
        );
    }

    #[test]
    fn rerender_saved_scene() {
        // Re-render every canvas element of a saved scene through the
        // current rasterizer (CANVAS_SCENE=<path>): the review loop for
        // brush-quality changes against real artwork, no app launch needed.
        let path = match std::env::var("CANVAS_SCENE") {
            Ok(p) => p,
            Err(_) => return,
        };
        let json = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read {path}: {e}");
        });
        let file: crate::scene::SceneFile = serde_json::from_str(&json).unwrap();
        let canvases: Vec<&Element> = file
            .elements
            .iter()
            .filter(|e| matches!(e.kind, ElementKind::Canvas { .. }))
            .collect();
        assert!(!canvases.is_empty(), "no canvas elements in {path}");
        let imgs: Vec<image::RgbaImage> = canvases.iter().map(|e| rasterize(e)).collect();
        let w = imgs.iter().map(|i| i.width()).max().unwrap();
        let h: u32 = imgs.iter().map(|i| i.height()).sum();
        let mut out = image::RgbaImage::new(w, h);
        let mut y = 0i64;
        for i in &imgs {
            image::imageops::overlay(&mut out, i, 0, y);
            y += i.height() as i64;
        }
        for p in out.pixels_mut() {
            p.0.swap(0, 2);
        }
        out.save(std::env::temp_dir().join("boundless-canvas-rerender.png"))
            .unwrap();
        eprintln!("rerendered {} canvas(es) → {w}×{h}", canvases.len());
    }

    #[test]
    fn preview_png_smoke() {
        // Visual smoke: render all three brushes side by side and write a
        // PNG. Gated behind an env var so normal test runs stay hermetic.
        if std::env::var("CANVAS_PREVIEW").is_err() {
            return;
        }
        let mut el = canvas_with(CanvasStroke {
            points: vec![WPoint::new(5.0, 20.0), WPoint::new(95.0, 20.0)],
            ..horizontal_stroke(CanvasBrush::Ink)
        });
        let strokes = el.canvas_strokes_mut().unwrap();
        strokes.push(CanvasStroke {
            points: vec![
                WPoint::new(10.0, 50.0),
                WPoint::new(40.0, 42.0),
                WPoint::new(70.0, 58.0),
                WPoint::new(95.0, 48.0),
            ],
            widths: Vec::new(),
            color: 0x3a6ea5,
            width: 10.0,
            brush: CanvasBrush::Watercolor,
            opacity: 1.0,
        });
        strokes.push(CanvasStroke {
            points: vec![WPoint::new(10.0, 80.0), WPoint::new(95.0, 80.0)],
            widths: Vec::new(),
            color: 0x8b2f2f,
            width: 10.0,
            brush: CanvasBrush::DryBrush,
            opacity: 1.0,
        });
        el.style.background = Some(0xfffdf6);

        // Seascape strip (mirrors the 62-score review piece): a wide
        // horizontal wash ("sky"), a wide zigzag wash ("wave band") and the
        // four-stage bloom of a medium wash. Slats/parallel-stripes or hard
        // zigzag corners would show up here immediately.
        let sky = Element::new(
            ElementKind::Canvas {
                strokes: vec![CanvasStroke {
                    points: vec![WPoint::new(2.0, 15.0), WPoint::new(98.0, 15.0)],
                    widths: Vec::new(),
                    color: 0x6f9fd8,
                    width: 22.0,
                    brush: CanvasBrush::Watercolor,
                    opacity: 1.0,
                }],
            },
            WBounds::new(0.0, 0.0, 100.0, 30.0),
            ElementStyle {
                background: Some(0xfffdf6),
                ..ElementStyle::default()
            },
        );
        let wave = Element::new(
            ElementKind::Canvas {
                strokes: vec![CanvasStroke {
                    points: vec![
                        WPoint::new(2.0, 18.0),
                        WPoint::new(18.0, 10.0),
                        WPoint::new(34.0, 18.0),
                        WPoint::new(50.0, 10.0),
                        WPoint::new(66.0, 18.0),
                        WPoint::new(82.0, 10.0),
                        WPoint::new(98.0, 18.0),
                    ],
                    widths: Vec::new(),
                    color: 0x8fa8c8,
                    width: 12.0,
                    brush: CanvasBrush::Watercolor,
                    opacity: 1.0,
                }],
            },
            WBounds::new(0.0, 0.0, 100.0, 30.0),
            ElementStyle {
                background: Some(0xfffdf6),
                ..ElementStyle::default()
            },
        );
        let mut frame = Element::new(
            ElementKind::Canvas {
                strokes: vec![CanvasStroke {
                    points: vec![
                        WPoint::new(10.0, 8.0),
                        WPoint::new(40.0, 4.0),
                        WPoint::new(70.0, 14.0),
                        WPoint::new(95.0, 10.0),
                    ],
                    widths: Vec::new(),
                    color: 0x3a6ea5,
                    width: 10.0,
                    brush: CanvasBrush::Watercolor,
                    opacity: 1.0,
                }],
            },
            WBounds::new(0.0, 0.0, 100.0, 20.0),
            ElementStyle {
                background: Some(0xfffdf6),
                ..ElementStyle::default()
            },
        );
        frame.seed = 42;
        let stages = 4;
        let row_h = 60; // px per 30-world-unit band
        let bloom_h = 40; // px per 20-world-unit bloom row
        let mut out = image::RgbaImage::new(200, 200 + row_h * 2 + bloom_h * stages);
        image::imageops::overlay(&mut out, &rasterize(&el), 0, 0);
        image::imageops::overlay(&mut out, &rasterize(&sky), 0, 200);
        image::imageops::overlay(&mut out, &rasterize(&wave), 0, 200 + row_h as i64);
        for k in 0..stages {
            let settle = k as f32 / (stages - 1) as f32;
            let img = rasterize_with(&frame, &[settle]);
            image::imageops::overlay(
                &mut out,
                &img,
                0,
                200 + row_h as i64 * 2 + bloom_h as i64 * k as i64,
            );
        }

        // The buffer is BGRA (RenderImage's convention); swap to RGBA for a
        // truthful PNG before saving.
        for p in out.pixels_mut() {
            p.0.swap(0, 2);
        }
        out.save(std::env::temp_dir().join("boundless-canvas-preview.png"))
            .unwrap();
    }

    #[test]
    fn wet_profile_progresses_and_settles() {
        let now = std::time::Instant::now();
        let ago = |ms: u64| Some(now.checked_sub(std::time::Duration::from_millis(ms)).unwrap());
        // Fresh commit → 0; past WET_MS → 1; mid-way strictly between.
        let p = wet_profile(
            &[
                Some(now),
                ago(WET_MS + 100),
                ago(WET_MS / 2),
                None,
            ],
            now,
        );
        assert_eq!(p[0], 0.0);
        assert_eq!(p[1], 1.0);
        assert!(p[2] > 0.0 && p[2] < 1.0, "mid-way in (0,1), got {}", p[2]);
        assert_eq!(p[3], 1.0, "missing/None entries are settled");
        // Monotone ease.
        assert!(wet_ease(0.2) < wet_ease(0.5) && wet_ease(0.5) < wet_ease(0.9));
        assert_eq!(wet_ease(1.0), 1.0);
        assert_eq!(wet_ease(2.0), 1.0, "clamped past the end");
    }

    #[test]
    fn bloom_renders_darker_then_wider() {
        let el = canvas_with(horizontal_stroke(CanvasBrush::Watercolor));
        let fresh = rasterize_with(&el, &[0.0]);
        let settled = rasterize_with(&el, &[1.0]);
        // Fresh ink is more concentrated at the path center…
        assert!(
            alpha_at(&fresh, 100, 100) > alpha_at(&settled, 100, 100) + 8,
            "wet core is darker: {} vs {}",
            alpha_at(&fresh, 100, 100),
            alpha_at(&settled, 100, 100)
        );
        // …and blooms outward as it settles (wider column coverage).
        let extent = |img: &image::RgbaImage| -> usize {
            (0..img.height())
                .filter(|&y| alpha_at(img, 100, y) > 0)
                .count()
        };
        assert!(
            extent(&settled) > extent(&fresh),
            "settled wash spreads further"
        );
        // settle == 1 is exactly the settled raster (seamless hand-off).
        assert_eq!(rasterize_with(&el, &[1.0]).as_raw(), rasterize(&el).as_raw());
        // Ink strokes ignore the wet parameter.
        let ink = canvas_with(horizontal_stroke(CanvasBrush::Ink));
        assert_eq!(
            rasterize_with(&ink, &[0.0]).as_raw(),
            rasterize(&ink).as_raw()
        );
    }

    #[test]
    fn image_animated_lifecycle_without_living_2_5s() {
        let cache = CanvasCache::new();
        let mut el = canvas_with(horizontal_stroke(CanvasBrush::Ink));
        let id = el.id;

        // No wet state → settled path.
        assert!(cache.image_animated(&el).is_none());

        // Committed ink stroke: recorded but settled → None immediately, no
        // animation frames allocated.
        cache.stroke_committed(id, CanvasBrush::Ink);
        assert!(cache.image_animated(&el).is_none());

        // Watercolor stroke: animates. Two calls → previous frame staled.
        cache.stroke_committed(id, CanvasBrush::Watercolor);
        el.canvas_strokes_mut().unwrap().push(horizontal_stroke(
            CanvasBrush::Watercolor,
        ));
        let (f1, still1) = cache.image_animated(&el).unwrap();
        assert!(still1);
        assert!(cache.take_stale().is_empty(), "first frame stales nothing");
        let (f2, still2) = cache.image_animated(&el).unwrap();
        assert!(still2);
        assert!(!Arc::ptr_eq(&f1, &f2));
        let stale = cache.take_stale();
        assert_eq!(stale.len(), 1);
        assert!(Arc::ptr_eq(&f1, &stale[0]), "frame N-1 goes stale on frame N");

        // Force the wet instant into the past: fully settled → None, the
        // last animation frame is flushed to stale, wet state cleared.
        let past = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(WET_MS + 50))
            .unwrap();
        cache
            .wet
            .borrow_mut()
            .insert(id, vec![Some(past), Some(past)]);
        assert!(cache.image_animated(&el).is_none());
        assert_eq!(cache.take_stale().len(), 1, "last frame flushed");
        assert!(cache.wet.borrow().get(&id).is_none(), "wet state cleared");
    }

    #[test]
    fn cache_hits_until_fingerprint_changes_then_stales_old() {
        let cache = CanvasCache::new();
        let el = canvas_with(horizontal_stroke(CanvasBrush::Ink));

        let first = cache.image(&el);
        assert!(Arc::ptr_eq(&first, &cache.image(&el)), "hit is stable");

        let mut changed = el.clone();
        changed
            .canvas_strokes_mut()
            .unwrap()
            .push(horizontal_stroke(CanvasBrush::Ink));
        let second = cache.image(&changed);
        assert!(!Arc::ptr_eq(&first, &second), "new stroke re-rasterizes");
        let stale = cache.take_stale();
        assert_eq!(stale.len(), 1);
        assert!(Arc::ptr_eq(&first, &stale[0]), "replaced texture is stale");
        assert!(cache.take_stale().is_empty(), "stale drains once");

        // Deleting the element evicts its texture.
        let mut scene = Scene::new();
        scene.add(changed.clone());
        cache.retain_scene(&scene);
        assert!(cache.take_stale().is_empty(), "still alive: not stale");
        let empty = Scene::new();
        cache.retain_scene(&empty);
        assert_eq!(cache.take_stale().len(), 1, "gone from scene → stale");
    }
}
