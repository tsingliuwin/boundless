//! Tool definitions and the pointer-drag state machine types.

use gpui::{Pixels, Point};

use crate::render::Handle;
use crate::scene::{Element, ElementId, WBounds, WPoint};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActiveTool {
    #[default]
    Select,
    Hand,
    Rectangle,
    Diamond,
    Ellipse,
    Arrow,
    Line,
    Pen,
    Text,
    Eraser,
}

impl ActiveTool {
    /// Human-readable label (used for tooltips / accessibility).
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            ActiveTool::Select => "选择",
            ActiveTool::Hand => "抓手",
            ActiveTool::Rectangle => "矩形",
            ActiveTool::Diamond => "菱形",
            ActiveTool::Ellipse => "椭圆",
            ActiveTool::Arrow => "箭头",
            ActiveTool::Line => "直线",
            ActiveTool::Pen => "画笔",
            ActiveTool::Text => "文本",
            ActiveTool::Eraser => "橡皮",
        }
    }

    #[allow(dead_code)]
    pub fn shortcut(self) -> &'static str {
        match self {
            ActiveTool::Select => "V",
            ActiveTool::Hand => "H",
            ActiveTool::Rectangle => "R",
            ActiveTool::Diamond => "D",
            ActiveTool::Ellipse => "O",
            ActiveTool::Arrow => "A",
            ActiveTool::Line => "L",
            ActiveTool::Pen => "P",
            ActiveTool::Text => "T",
            ActiveTool::Eraser => "E",
        }
    }

    #[allow(dead_code)]
    pub fn is_shape(self) -> bool {
        matches!(
            self,
            ActiveTool::Rectangle | ActiveTool::Diamond | ActiveTool::Ellipse
        )
    }

    pub fn is_drawing(self) -> bool {
        matches!(
            self,
            ActiveTool::Rectangle
                | ActiveTool::Diamond
                | ActiveTool::Ellipse
                | ActiveTool::Arrow
                | ActiveTool::Line
                | ActiveTool::Pen
        )
    }
}

/// Which control point of a selected line/arrow is being targeted. Vertex(i)
/// is the i-th polyline point; Midpoint(seg) is the midpoint handle of the
/// segment between points `seg` and `seg + 1` — dragging it inserts a new
/// vertex there (Excalidraw-style bending).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointTarget {
    Vertex(usize),
    Midpoint(usize),
}

/// In-progress pointer interaction.
pub enum DragState {
    /// No active drag.
    Idle,
    /// Panning the camera (hand tool / middle mouse).
    Panning { last_screen: Point<Pixels> },
    /// Drawing a shape/line/arrow from `start`; the draft element is stored
    /// separately on the board. The `seed` is generated once at drag start
    /// and reused for every draft frame: rough hand-drawn jitter is seeded,
    /// so a fresh seed per frame would make the draft visibly "swim".
    Drawing { start: WPoint, seed: u64 },
    /// Freehand stroke in progress. The `seed` is generated once at drag
    /// start and reused for every draft frame (and the committed stroke):
    /// rough hand-drawn jitter is seeded, so a fresh seed per frame would
    /// make the whole line visibly "swim" while drawing.
    Freedraw { points: Vec<WPoint>, seed: u64 },
    /// Moving the current selection.
    Moving {
        last_world: WPoint,
        /// History is recorded lazily on the first actual move.
        recorded: bool,
    },
    /// Resizing the selection via a bbox handle. Originals are kept so each
    /// move recomputes from a stable base.
    Resizing {
        handle: Handle,
        original_bounds: WBounds,
        originals: Vec<Element>,
        recorded: bool,
    },
    /// Dragging a vertex/midpoint control handle of a single selected
    /// line/arrow. `original` is the stable base element: every move
    /// recomputes from it, which makes midpoint drags idempotent (each move
    /// re-inserts the new vertex at the same segment). History is recorded
    /// lazily on the first actual move.
    EditingPoint {
        element: ElementId,
        original: Element,
        target: PointTarget,
        recorded: bool,
    },
    /// Rubber-band selection.
    Marquee {
        start: WPoint,
        current: WPoint,
        /// Selection present before the marquee started (shift-union).
        base_selection: Vec<ElementId>,
    },
    /// Erasing while dragging.
    Erasing { removed_any: bool },
}

impl Default for DragState {
    fn default() -> Self {
        DragState::Idle
    }
}

impl DragState {
    #[allow(dead_code)]
    pub fn is_idle(&self) -> bool {
        matches!(self, DragState::Idle)
    }
}
