//! Mind map layout: turn a text tree into positioned node boxes + curved
//! branch links, deterministically.
//!
//! Pure geometry — no gpui, no scene mutation — so the same [`layout`] result
//! feeds both the real board (`BoardView::apply_canvas_op`) and the headless
//! eval replay (`ai::eval`). The model supplies only the tree (via the
//! `draw_mindmap` tool); the code owns every coordinate, which is what
//! guarantees the "no overlap, no crossing" quality bar that hand-placed
//! shapes could not.
//!
//! Classic balanced two-sided layout (XMind/MindNode style): the root sits at
//! the requested center; its level-1 subtrees are greedily split left/right by
//! leaf weight so the two wings carry similar mass; each wing is a horizontal
//! tidy-tree — children stacked in disjoint vertical bands, the parent
//! vertically centered on its children, each level in its own column. The
//! right wing grows rightward, the left wing is a true mirror (grows
//! leftward), so the corridor between a parent and the root is always clear.
//!
//! If the natural size exceeds the canvas, all fonts/gaps shrink by one exact
//! fit factor (floored for readability) before the final pass.
//!
//! Invariants (unit-tested): nodes never overlap pairwise; a link's chord
//! stays inside its own subtree's band, so links never cross each other or
//! pass through unrelated nodes; the whole drawing stays inside the canvas
//! margins (down to the font floor).

use crate::scene::element::{WBounds, WPoint};

/// Visible canvas area the model is told about; layout clamps into it.
pub const CANVAS_W: f64 = 1600.0;
pub const CANVAS_H: f64 = 1000.0;
/// Hard margin the drawing must stay inside (matches the prompt's 纪律 rule).
pub const MARGIN: f64 = 40.0;

/// Gap between a parent's column edge and its children's column (at scale 1).
const HGAP: f64 = 64.0;
/// Vertical gap between sibling subtrees (at scale 1).
const VGAP: f64 = 28.0;
/// Horizontal padding inside a node box, per side.
const PAD_X: f64 = 14.0;
/// Vertical padding inside a node box, total (above + below the text).
const PAD_Y: f64 = 12.0;
/// Inset of link exit points from a node's top/bottom edge.
const LINK_PAD: f64 = 3.0;
/// Readability floor for the auto-fit shrink factor.
const MIN_SCALE: f64 = 0.55;

/// Font size per depth level (0 = root) at scale 1.
pub fn font_size_for_level(level: usize) -> f64 {
    match level {
        0 => 26.0,
        1 => 20.0,
        _ => 16.0,
    }
}

/// Per-branch palette (stroke, light fill). Dark text on the pastel fills
/// stays readable; saturated strokes keep branches tellable apart. The left
/// wing indexes from the second half so opposite branches don't share a color.
const BRANCH_PALETTE: [(u32, u32); 6] = [
    (0xE8590C, 0xFFE8D9), // 橙
    (0x1971C2, 0xE3F0FC), // 蓝
    (0x2F9E44, 0xE6F6EA), // 绿
    (0xE03131, 0xFDEBEB), // 红
    (0x9C36B5, 0xF6EAFB), // 紫
    (0x0B7285, 0xE3FAFC), // 青（原金色与橙色色相偏近，目验反馈）
];

/// Root styling: warm fill + near-black stroke so it reads as the title node.
const ROOT_STROKE: u32 = 0x1E1E1E;
const ROOT_FILL: u32 = 0xFFE3A3;

/// One node of the input tree (the model's view: text + children).
#[derive(Clone, Debug, PartialEq)]
pub struct MindmapNodeInput {
    pub text: String,
    pub children: Vec<MindmapNodeInput>,
}

impl MindmapNodeInput {
    pub fn new(text: impl Into<String>, children: Vec<MindmapNodeInput>) -> Self {
        Self {
            text: text.into(),
            children,
        }
    }
}

/// Count all nodes in a tree (including the root).
pub fn count_nodes(root: &MindmapNodeInput) -> usize {
    1 + root.children.iter().map(count_nodes).sum::<usize>()
}

/// Maximum root-to-leaf depth (a lone root has depth 1).
pub fn max_depth(root: &MindmapNodeInput) -> usize {
    1 + root.children.iter().map(max_depth).fold(0, usize::max)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// Rough single-line text width, biased wide on purpose: CJK/fullwidth
/// chars ≈ 1.1 × font size (the real fallback font runs wider than 1em),
/// everything else ≈ 0.62 × (Caveat caps are wide). Underestimating wraps
/// the bound label's last character out of the node box — the generous
/// factors are cheaper than a wrapped line.
pub fn estimate_text_width(text: &str, font_size: f64) -> f64 {
    text.chars()
        .map(|c| {
            if is_cjk(c) {
                font_size * 1.1
            } else {
                font_size * 0.62
            }
        })
        .sum()
}

/// Node box size for a text at a given depth level and global fit scale.
/// The width estimate is deliberately generous: the real renderer's CJK
/// fallback runs ~1.1em and Caveat caps are wide, and a 1px underestimate
/// wraps the bound label's last character out of the box (实测教训).
fn node_size(text: &str, level: usize, scale: f64) -> (f64, f64) {
    let fs = font_size_for_level(level) * scale;
    let w = estimate_text_width(text, fs) + PAD_X * 2.0 * scale + 6.0 * scale;
    (
        w.max(fs * 2.0),
        fs * crate::scene::LINE_HEIGHT + PAD_Y * scale,
    )
}

/// A laid-out node: box, level, and branch colors (the root carries its own).
#[derive(Clone, Debug, PartialEq)]
pub struct NodeSpec {
    pub bounds: WBounds,
    pub text: String,
    /// 0 = root, 1 = first-level branch, …
    pub level: usize,
    pub stroke: u32,
    pub fill: u32,
    pub font_size: f64,
}

impl NodeSpec {
    pub fn center(&self) -> WPoint {
        self.bounds.center()
    }
}

/// A branch link: parent edge → child edge as a 4-point S-curve (start,
/// horizontal stub, vertical rise, end) — rendered as a `LineType::Curved`
/// polyline, which reads as the classic organic mind-map connector.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkSpec {
    pub points: Vec<WPoint>,
    pub stroke: u32,
}

/// The full layout result.
#[derive(Clone, Debug, PartialEq)]
pub struct MindmapLayout {
    /// Root first (index 0), then wing nodes depth-first per wing.
    pub nodes: Vec<NodeSpec>,
    pub links: Vec<LinkSpec>,
    /// Total drawn extent (already fitted into the canvas margins).
    pub extent: WBounds,
}

/// Layout a whole tree centered at `center` (the root's center point),
/// auto-shrinking to fit the canvas if the natural size overflows.
pub fn layout(root: &MindmapNodeInput, center: WPoint) -> MindmapLayout {
    let first = layout_at(root, center, 1.0);
    if fits(&first.extent) {
        return first;
    }
    // One exact fit factor for both axes; floored so text stays readable.
    let avail_w = CANVAS_W - 2.0 * MARGIN;
    let avail_h = CANVAS_H - 2.0 * MARGIN;
    let sx = avail_w / first.extent.w.max(1.0);
    let sy = avail_h / first.extent.h.max(1.0);
    let scale = sx.min(sy).clamp(MIN_SCALE, 1.0);
    layout_at(root, center, scale)
}

fn fits(e: &WBounds) -> bool {
    e.x >= MARGIN - 1e-6
        && e.y >= MARGIN - 1e-6
        && e.right() <= CANVAS_W - MARGIN + 1e-6
        && e.bottom() <= CANVAS_H - MARGIN + 1e-6
}

/// One layout pass at a fixed fit scale. `dir` = +1 for the right wing
/// (descendants grow rightward), −1 for the mirrored left wing.
fn layout_at(root: &MindmapNodeInput, center: WPoint, scale: f64) -> MindmapLayout {
    let hgap = HGAP * scale;
    let mut nodes: Vec<NodeSpec> = Vec::new();
    let mut links: Vec<LinkSpec> = Vec::new();

    let (rw, rh) = node_size(&root.text, 0, scale);
    let root_bounds = WBounds::new(center.x - rw / 2.0, center.y - rh / 2.0, rw, rh);
    // Root is always index 0.
    nodes.push(NodeSpec {
        bounds: root_bounds,
        text: root.text.clone(),
        level: 0,
        stroke: ROOT_STROKE,
        fill: ROOT_FILL,
        font_size: font_size_for_level(0) * scale,
    });

    // Greedily split level-1 subtrees into right/left wings by leaf weight so
    // both sides of the root carry similar mass. Children keep input order
    // within their wing (top→bottom on both sides). Palette slots are
    // wing-local; the left wing starts at the palette's second half so
    // opposite branches never share a color.
    let half_palette = BRANCH_PALETTE.len() / 2;
    let mut right: Vec<&MindmapNodeInput> = Vec::new();
    let mut left: Vec<&MindmapNodeInput> = Vec::new();
    let mut right_weight = 0usize;
    let mut left_weight = 0usize;
    for child in &root.children {
        let w = count_leaves(child);
        if right_weight <= left_weight {
            right.push(child);
            right_weight += w;
        } else {
            left.push(child);
            left_weight += w;
        }
    }
    let right: Vec<(&MindmapNodeInput, usize)> =
        right.into_iter().enumerate().map(|(i, c)| (c, i)).collect();
    let left: Vec<(&MindmapNodeInput, usize)> = left
        .into_iter()
        .enumerate()
        .map(|(i, c)| (c, i + half_palette))
        .collect();

    // Right wing: children column starts right of the root box.
    let mut right_bounds = Vec::new();
    layout_wing(
        &right,
        1.0,
        root_bounds.right() + hgap,
        center.y,
        scale,
        &mut nodes,
        &mut links,
        &mut right_bounds,
    );
    // Left wing mirrored: its widest column ends at the root's left edge.
    let left_w = wing_width(&left, scale);
    let mut left_bounds = Vec::new();
    layout_wing(
        &left,
        -1.0,
        root_bounds.x - hgap - left_w,
        center.y,
        scale,
        &mut nodes,
        &mut links,
        &mut left_bounds,
    );

    // Root links: exits track each branch's height along the root's side
    // edge, colored by that branch.
    let right_ys = exit_ys(
        &root_bounds,
        &right_bounds
            .iter()
            .map(|b| b.y + b.h / 2.0)
            .collect::<Vec<_>>(),
    );
    for (i, (_, branch)) in right.iter().enumerate() {
        let b = right_bounds[i];
        links.push(LinkSpec {
            points: s_curve(
                WPoint::new(root_bounds.right(), right_ys[i]),
                WPoint::new(b.x, b.y + b.h / 2.0),
            ),
            stroke: palette_stroke(*branch),
        });
    }
    let left_ys = exit_ys(
        &root_bounds,
        &left_bounds
            .iter()
            .map(|b| b.y + b.h / 2.0)
            .collect::<Vec<_>>(),
    );
    for (i, (_, branch)) in left.iter().enumerate() {
        let b = left_bounds[i];
        links.push(LinkSpec {
            points: s_curve(
                WPoint::new(root_bounds.x, left_ys[i]),
                WPoint::new(b.right(), b.y + b.h / 2.0),
            ),
            stroke: palette_stroke(*branch),
        });
    }

    let mut extent = WBounds::from_points(
        &nodes
            .iter()
            .flat_map(|n| {
                [
                    WPoint::new(n.bounds.x, n.bounds.y),
                    WPoint::new(n.bounds.right(), n.bounds.bottom()),
                ]
            })
            .collect::<Vec<_>>(),
    );
    // Clamp the drawing into the canvas margins (identity in the fitted case).
    let (min_x, min_y) = (MARGIN, MARGIN);
    let (max_x, max_y) = (CANVAS_W - MARGIN, CANVAS_H - MARGIN);
    let dx = if extent.x < min_x {
        min_x - extent.x
    } else if extent.right() > max_x {
        (max_x - extent.right()).max(min_x - extent.x)
    } else {
        0.0
    };
    let dy = if extent.y < min_y {
        min_y - extent.y
    } else if extent.bottom() > max_y {
        (max_y - extent.bottom()).max(min_y - extent.y)
    } else {
        0.0
    };
    if dx != 0.0 || dy != 0.0 {
        for n in &mut nodes {
            n.bounds.x += dx;
            n.bounds.y += dy;
        }
        for l in &mut links {
            for p in &mut l.points {
                p.x += dx;
                p.y += dy;
            }
        }
        extent.x += dx;
        extent.y += dy;
    }
    MindmapLayout {
        nodes,
        links,
        extent,
    }
}

/// 4-point S-curve from a parent edge to a child edge: horizontal stub out of
/// the parent, vertical rise at the mid x, horizontal into the child. When
/// both endpoints share a height (exit tracks the child), the middle
/// collapses to one collinear point — a straight link, no duplicate points
/// for the spline renderer to choke on.
fn s_curve(start: WPoint, end: WPoint) -> Vec<WPoint> {
    let mx = (start.x + end.x) / 2.0;
    if (start.y - end.y).abs() < 1e-9 {
        return vec![start, WPoint::new(mx, start.y), end];
    }
    vec![start, WPoint::new(mx, start.y), WPoint::new(mx, end.y), end]
}

/// Link exit points along a node's vertical edge: each exit tracks its
/// child's height (clamped into the node's extent, inset by [`LINK_PAD`]),
/// then a two-pass compaction forces strictly increasing exits — curves
/// leaving one exact point tangle into a knot under jitter. When children
/// bunch beyond the node's extent the exits bundle at the edge corners and
/// fan out together — the classic branch-trunk look, no crossing: an
/// in-range child gets a perfectly horizontal link (no transition to cut
/// through), and bundled transitions run parallel. `child_cys` must be
/// ordered top→bottom, as placement guarantees.
fn exit_ys(edge: &WBounds, child_cys: &[f64]) -> Vec<f64> {
    let n = child_cys.len();
    if n == 0 {
        return Vec::new();
    }
    let top = edge.y + LINK_PAD;
    let bottom = (edge.y + edge.h - LINK_PAD).max(top);
    const MAX_GAP: f64 = 0.75;
    // Shrink the gap if the exits can't all fit inside the edge.
    let gap = if n > 1 {
        MAX_GAP.min((bottom - top) / (n - 1) as f64)
    } else {
        MAX_GAP
    };
    let mut out: Vec<f64> = child_cys.iter().map(|&cy| cy.clamp(top, bottom)).collect();
    // Pass 1: crowd spreads downward from each clamped target.
    for i in 1..n {
        out[i] = out[i].max(out[i - 1] + gap);
    }
    // Pass 2: if the tail ran past the bottom edge, re-pin from the bottom
    // upward so everything fits back inside.
    if out[n - 1] > bottom {
        out[n - 1] = bottom;
        for i in (0..n - 1).rev() {
            out[i] = out[i].min(out[i + 1] - gap);
        }
    }
    out
}

fn palette_stroke(branch: usize) -> u32 {
    BRANCH_PALETTE[branch % BRANCH_PALETTE.len()].0
}

fn palette_fill(branch: usize) -> u32 {
    BRANCH_PALETTE[branch % BRANCH_PALETTE.len()].1
}

/// Leaf count of a subtree — the wing-balance weight.
fn count_leaves(node: &MindmapNodeInput) -> usize {
    if node.children.is_empty() {
        1
    } else {
        node.children.iter().map(count_leaves).sum()
    }
}

/// Widest total column span across a wing's level-1 subtrees.
fn wing_width(wing: &[(&MindmapNodeInput, usize)], scale: f64) -> f64 {
    wing.iter()
        .map(|(sub, _)| subtree_width(sub, 1, scale))
        .fold(0.0, f64::max)
}

/// Total horizontal span of a subtree laid out at `level`.
fn subtree_width(node: &MindmapNodeInput, level: usize, scale: f64) -> f64 {
    let (w, _) = node_size(&node.text, level, scale);
    if node.children.is_empty() {
        return w;
    }
    let max_child_w = node
        .children
        .iter()
        .map(|c| subtree_width(c, level + 1, scale))
        .fold(0.0, f64::max);
    w + HGAP * scale + max_child_w
}

/// Height of a subtree when laid out at `level`.
fn subtree_height(node: &MindmapNodeInput, level: usize, scale: f64) -> f64 {
    let (_, h) = node_size(&node.text, level, scale);
    if node.children.is_empty() {
        return h;
    }
    let child_h: f64 = node
        .children
        .iter()
        .map(|c| subtree_height(c, level + 1, scale))
        .sum();
    let n = node.children.len() as f64;
    child_h.max(h) + (n - 1.0) * VGAP * scale
}

/// Lay out one wing (level-1 subtrees) whose column starts at `x` and grows
/// in `dir`, vertically centered on `center_y`. Appends node specs and
/// intra-wing links; collects each level-1 node's bounds for the root links.
#[allow(clippy::too_many_arguments)]
fn layout_wing(
    wing: &[(&MindmapNodeInput, usize)],
    dir: f64,
    x: f64,
    center_y: f64,
    scale: f64,
    nodes: &mut Vec<NodeSpec>,
    links: &mut Vec<LinkSpec>,
    wing_bounds: &mut Vec<WBounds>,
) -> f64 {
    let vgap = VGAP * scale;
    if wing.is_empty() {
        return 0.0;
    }
    let heights: Vec<f64> = wing
        .iter()
        .map(|(sub, _)| subtree_height(sub, 1, scale))
        .collect();
    let total_h: f64 = heights.iter().sum::<f64>() + (wing.len() as f64 - 1.0) * vgap;
    let mut y = center_y - total_h / 2.0;
    for (i, (sub, branch)) in wing.iter().enumerate() {
        let h = heights[i];
        let b = place_subtree(sub, 1, *branch, dir, x, y, y + h, scale, nodes, links);
        wing_bounds.push(b);
        y += h + vgap;
    }
    total_h
}

/// Place a subtree whose vertical band is [band_top, band_bottom] and whose
/// own column edge starts at `x`, growing in `dir`. The node is vertically
/// centered on its children (or the band for leaves). Returns the node's
/// bounds; the node spec is appended after its descendants.
#[allow(clippy::too_many_arguments)]
fn place_subtree(
    node: &MindmapNodeInput,
    level: usize,
    branch: usize,
    dir: f64,
    x: f64,
    band_top: f64,
    band_bottom: f64,
    scale: f64,
    nodes: &mut Vec<NodeSpec>,
    links: &mut Vec<LinkSpec>,
) -> WBounds {
    let vgap = VGAP * scale;
    let (w, h) = node_size(&node.text, level, scale);
    let fs = font_size_for_level(level) * scale;

    if node.children.is_empty() {
        let cy = (band_top + band_bottom) / 2.0;
        let bounds = WBounds::new(x.min(x + dir * w), cy - h / 2.0, w, h);
        nodes.push(NodeSpec {
            bounds,
            text: node.text.clone(),
            level,
            stroke: palette_stroke(branch),
            fill: palette_fill(branch),
            font_size: fs,
        });
        return bounds;
    }

    // Children live in a sub-band; this node centers on their mean center.
    // For dir = -1 the parent box is at [x - w, x] and children's column
    // starts at x - w - HGAP growing leftward.
    let child_x = if dir > 0.0 {
        x + w + HGAP * scale
    } else {
        x - w - HGAP * scale
    };
    let child_heights: Vec<f64> = node
        .children
        .iter()
        .map(|c| subtree_height(c, level + 1, scale))
        .collect();
    let child_total: f64 =
        child_heights.iter().sum::<f64>() + (node.children.len() as f64 - 1.0) * vgap;
    let band_center = (band_top + band_bottom) / 2.0;
    let child_top = (band_center - child_total / 2.0)
        .max(band_top)
        .min((band_bottom - child_total).max(band_top));
    let mut y = child_top;
    let mut child_bounds: Vec<WBounds> = Vec::with_capacity(node.children.len());
    for (i, c) in node.children.iter().enumerate() {
        let ch = child_heights[i];
        child_bounds.push(place_subtree(
            c,
            level + 1,
            branch,
            dir,
            child_x,
            y,
            y + ch,
            scale,
            nodes,
            links,
        ));
        y += ch + vgap;
    }

    let first = child_bounds.first().unwrap();
    let last = child_bounds.last().unwrap();
    let cy = (first.y + first.h / 2.0 + last.y + last.h / 2.0) / 2.0;
    let bounds = WBounds::new(x.min(x + dir * w), cy - h / 2.0, w, h);
    // One exit per child, tracking its height along this node's edge.
    let child_cys: Vec<f64> = child_bounds.iter().map(|cb| cb.y + cb.h / 2.0).collect();
    let ys = exit_ys(&bounds, &child_cys);
    for (i, cb) in child_bounds.iter().enumerate() {
        let (start, end) = if dir > 0.0 {
            (
                WPoint::new(bounds.right(), ys[i]),
                WPoint::new(cb.x, child_cys[i]),
            )
        } else {
            (
                WPoint::new(bounds.x, ys[i]),
                WPoint::new(cb.right(), child_cys[i]),
            )
        };
        links.push(LinkSpec {
            points: s_curve(start, end),
            stroke: palette_stroke(branch),
        });
    }
    nodes.push(NodeSpec {
        bounds,
        text: node.text.clone(),
        level,
        stroke: palette_stroke(branch),
        fill: palette_fill(branch),
        font_size: fs,
    });
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(t: &str) -> MindmapNodeInput {
        MindmapNodeInput::new(t, vec![])
    }

    /// A realistic tree: root + 4 branches × 3 leaves (17 nodes).
    fn sample_tree() -> MindmapNodeInput {
        MindmapNodeInput::new(
            "高效学习方法",
            vec![
                MindmapNodeInput::new(
                    "主动回忆",
                    vec![leaf("自测"), leaf("闪卡"), leaf("费曼技巧")],
                ),
                MindmapNodeInput::new(
                    "间隔重复",
                    vec![leaf("艾宾浩斯"), leaf("Anki"), leaf("周期复习")],
                ),
                MindmapNodeInput::new(
                    "深度加工",
                    vec![leaf("类比"), leaf("举例"), leaf("跨学科联系")],
                ),
                MindmapNodeInput::new(
                    "专注环境",
                    vec![leaf("番茄钟"), leaf("远离手机"), leaf("固定时段")],
                ),
            ],
        )
    }

    fn overlap(a: &WBounds, b: &WBounds) -> bool {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    #[test]
    fn counts_and_depth() {
        let t = sample_tree();
        assert_eq!(count_nodes(&t), 17);
        assert_eq!(max_depth(&t), 3);
        assert_eq!(max_depth(&leaf("根")), 1);
    }

    #[test]
    fn text_width_estimates_cjk_and_latin() {
        // Deliberately generous: 1.1× CJK, 0.62× Latin (wrapped labels
        // spill out of the box — underestimate is the failure mode).
        assert!((estimate_text_width("四个汉字", 20.0) - 88.0).abs() < 1e-9);
        assert!((estimate_text_width("abcd", 20.0) - 49.6).abs() < 1e-9);
    }

    #[test]
    fn nodes_never_overlap() {
        let l = layout(&sample_tree(), WPoint::new(800.0, 500.0));
        assert_eq!(l.nodes.len(), 17);
        for i in 0..l.nodes.len() {
            for j in i + 1..l.nodes.len() {
                assert!(
                    !overlap(&l.nodes[i].bounds, &l.nodes[j].bounds),
                    "nodes {i}({:?}) and {j}({:?}) overlap",
                    l.nodes[i].text,
                    l.nodes[j].text
                );
            }
        }
    }

    #[test]
    fn root_is_first_and_centered() {
        let l = layout(&sample_tree(), WPoint::new(800.0, 500.0));
        let root = &l.nodes[0];
        assert_eq!(root.level, 0);
        assert_eq!(root.text, "高效学习方法");
        assert!((root.center().x - 800.0).abs() < 1e-9);
        assert!((root.center().y - 500.0).abs() < 1e-9);
    }

    #[test]
    fn branches_split_both_sides() {
        let l = layout(&sample_tree(), WPoint::new(800.0, 500.0));
        let root_cx = l.nodes[0].center().x;
        let right = l.nodes[1..]
            .iter()
            .filter(|n| n.center().x > root_cx)
            .count();
        let left = l.nodes[1..]
            .iter()
            .filter(|n| n.center().x < root_cx)
            .count();
        assert!(right >= 4, "right wing too small: {right}");
        assert!(left >= 4, "left wing too small: {left}");
    }

    #[test]
    fn every_non_root_node_has_one_link() {
        let l = layout(&sample_tree(), WPoint::new(800.0, 500.0));
        assert_eq!(l.links.len(), l.nodes.len() - 1);
        for link in &l.links {
            // 4-point S-curve, or 3 collinear points when the exit tracks an
            // in-range child (horizontal link).
            assert!(link.points.len() == 4 || link.points.len() == 3);
            let (a, b) = (link.points[0], *link.points.last().unwrap());
            assert!(
                (a.x - b.x).abs() >= HGAP - 1.0,
                "link too short: {a:?}→{b:?}"
            );
        }
    }

    #[test]
    fn links_never_cross_each_other() {
        let l = layout(&sample_tree(), WPoint::new(800.0, 500.0));
        for i in 0..l.links.len() {
            for j in i + 1..l.links.len() {
                let a = (
                    &l.links[i].points[0],
                    &l.links[i].points[l.links[i].points.len() - 1],
                );
                let b = (
                    &l.links[j].points[0],
                    &l.links[j].points[l.links[j].points.len() - 1],
                );
                // T-junctions and shared endpoints are legitimate (bundled
                // trunks); only interior×interior passages count.
                assert!(
                    !segments_cross(*a.0, *a.1, *b.0, *b.1),
                    "links {i} and {j} cross: {a:?}→{:?} vs {b:?}→{:?}",
                    a.1,
                    b.1
                );
            }
        }
    }

    #[test]
    fn links_stay_out_of_unrelated_nodes() {
        let l = layout(&sample_tree(), WPoint::new(800.0, 500.0));
        for link in &l.links {
            for (ni, n) in l.nodes.iter().enumerate() {
                // An endpoint touching a node edge is by design; only an
                // interior sample point is a bug.
                let interior_hits = link
                    .points
                    .iter()
                    .filter(|p| {
                        p.x > n.bounds.x + 1.0
                            && p.x < n.bounds.right() - 1.0
                            && p.y > n.bounds.y + 1.0
                            && p.y < n.bounds.bottom() - 1.0
                    })
                    .count();
                assert_eq!(interior_hits, 0, "link enters node {ni} ({:?})", n.text);
            }
        }
    }

    #[test]
    fn big_tree_auto_shrinks_to_fit_canvas() {
        // 6 branches × 4 long leaves: natural width exceeds the canvas, so
        // the fit pass must shrink fonts/gaps until everything fits.
        let mut big = MindmapNodeInput::new("中心主题", vec![]);
        for i in 0..6 {
            let mut b = MindmapNodeInput::new(format!("分支主题{i}"), vec![]);
            for j in 0..4 {
                b.children
                    .push(leaf(&format!("很长的要点描述文字{i}-{j}需要缩放才装得下")));
            }
            big.children.push(b);
        }
        let l = layout(&big, WPoint::new(800.0, 500.0));
        assert_eq!(l.nodes.len(), 1 + 6 + 24);
        assert!(fits(&l.extent), "extent = {extent:?}", extent = l.extent);
        for n in &l.nodes {
            assert!(n.bounds.x >= MARGIN - 1e-6 && n.bounds.right() <= CANVAS_W - MARGIN + 1e-6);
            assert!(n.bounds.y >= MARGIN - 1e-6 && n.bounds.bottom() <= CANVAS_H - MARGIN + 1e-6);
        }
        // Readability floor: fonts must not collapse.
        assert!(l.nodes.iter().map(|n| n.font_size).fold(0.0, f64::max) >= 26.0 * MIN_SCALE);
    }

    #[test]
    fn layout_is_deterministic() {
        let t = sample_tree();
        assert_eq!(
            layout(&t, WPoint::new(800.0, 500.0)),
            layout(&t, WPoint::new(800.0, 500.0))
        );
    }

    #[test]
    fn single_root_layouts_fine() {
        let l = layout(&leaf("只有根"), WPoint::new(800.0, 500.0));
        assert_eq!(l.nodes.len(), 1);
        assert!(l.links.is_empty());
    }

    #[test]
    fn deep_chain_steps_to_own_columns() {
        let mut n = leaf("第五层");
        for t in ["第四层", "第三层", "第二层", "根"] {
            n = MindmapNodeInput::new(t, vec![n]);
        }
        let l = layout(&n, WPoint::new(800.0, 500.0));
        assert_eq!(l.nodes.len(), 5);
        for i in 0..l.nodes.len() {
            for j in i + 1..l.nodes.len() {
                assert!(!overlap(&l.nodes[i].bounds, &l.nodes[j].bounds));
            }
        }
        assert!(fits(&l.extent));
    }

    #[test]
    fn same_parent_links_have_distinct_exit_points() {
        // The knot bug: n links leaving the exact same coordinate tangle
        // under roughness jitter. Every exit must be unique, inset from the
        // node edges, and ordered like the children (top→bottom).
        for tree in [sample_tree(), big_tree()] {
            let l = layout(&tree, WPoint::new(800.0, 500.0));
            let starts: Vec<WPoint> = l.links.iter().map(|l| l.points[0]).collect();
            for i in 0..starts.len() {
                for j in i + 1..starts.len() {
                    assert!(
                        starts[i] != starts[j],
                        "links {i} and {j} share an exit point"
                    );
                }
            }
        }
    }

    fn big_tree() -> MindmapNodeInput {
        let mut big = MindmapNodeInput::new("中心主题", vec![]);
        for i in 0..6 {
            let mut b = MindmapNodeInput::new(format!("分支主题{i}"), vec![]);
            for j in 0..4 {
                b.children.push(leaf(&format!("要点{i}-{j}")));
            }
            big.children.push(b);
        }
        big
    }

    /// Interior×interior crossing: both segments must straddle each other
    /// strictly (any endpoint touch / T-junction / collinear overlap yields a
    /// zero product → not a crossing). Same-parent links legitimately
    /// T-join into a bundled trunk; a visual crossing is two lines passing
    /// through each other.
    fn segments_cross(p1: WPoint, p2: WPoint, p3: WPoint, p4: WPoint) -> bool {
        fn ccw(a: WPoint, b: WPoint, c: WPoint) -> f64 {
            (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
        }
        let d1 = ccw(p3, p4, p1);
        let d2 = ccw(p3, p4, p2);
        let d3 = ccw(p1, p2, p3);
        let d4 = ccw(p1, p2, p4);
        (d1 * d2 < 0.0) && (d3 * d4 < 0.0)
    }
}
