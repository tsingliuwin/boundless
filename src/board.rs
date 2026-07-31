//! BoardView: the infinite canvas — scene, camera, tools, selection,
//! text editing and the surrounding chrome (toolbar, style bar, zoom bar).

use std::ops::Range;
use std::path::PathBuf;

use gpui::prelude::*;
use gpui::*;

use crate::ai::panel::AiPanel;
use crate::camera::Camera;
use crate::history::History;
use crate::render::rough::{color_u32, paths_for_element, ReadyPath};
use crate::render::{dot_grid, handle_rects, measure_text, shape_text, ShapedTextLine};
use crate::scene::{
    Element, ElementId, ElementKind, ElementStyle, Scene, SceneFile, StrokeStyle, WBounds, WPoint,
    DEFAULT_FONT_SIZE, LINE_HEIGHT,
};
use crate::text::{utf16_to_utf8, utf8_to_utf16, TextEditSession};
use crate::tools::{ActiveTool, DragState};

actions!(
    boundless,
    [
        Undo, Redo, SaveScene, OpenScene, DeleteSelection, CancelOp, ZoomIn, ZoomOut, ZoomReset,
        SelectTool, HandTool, RectTool, DiamondTool, EllipseTool, ArrowTool, LineTool, PenTool,
        TextTool, EraserTool, ToggleAi,
    ]
);

pub const SELECTION_COLOR: u32 = 0x4c9ffe;
const BG_COLOR: u32 = 0xffffff;
const GRID_COLOR: u32 = 0xcccccc;

pub struct EditingState {
    pub element_id: ElementId,
    pub session: TextEditSession,
    pub font_size: f64,
    pub wrap_width: Option<f64>,
    pub min_height: Option<f64>,
    /// True when the text element was created by this editing session.
    pub is_new: bool,
}

pub struct BoardView {
    pub scene: Scene,
    pub camera: Camera,
    pub tool: ActiveTool,
    drag: DragState,
    pub selection: Vec<ElementId>,
    pub style: ElementStyle,
    history: History,
    editing: Option<EditingState>,
    draft: Option<Element>,
    canvas_bounds: Bounds<Pixels>,
    file_path: Option<PathBuf>,
    dirty: bool,
    notice: Option<String>,
    focus_handle: FocusHandle,
    ai_panel: Option<Entity<AiPanel>>,
    /// Text elements whose bounds need precise (re)measurement, done during
    /// the next render where a `&Window` is available.
    pending_measure: Vec<ElementId>,
    /// Cached hover state (updated on mouse move) used to pick a cursor that
    /// hints the available action: move over an element, resize over a handle.
    hover_over_element: bool,
    hover_handle: Option<crate::render::Handle>,
}

impl BoardView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self {
            scene: Scene::new(),
            camera: Camera::default(),
            tool: ActiveTool::Select,
            drag: DragState::Idle,
            selection: Vec::new(),
            style: ElementStyle::default(),
            history: History::new(),
            editing: None,
            draft: None,
            canvas_bounds: Bounds::default(),
            file_path: None,
            dirty: false,
            notice: None,
            focus_handle,
            ai_panel: None,
            pending_measure: Vec::new(),
            hover_over_element: false,
            hover_handle: None,
        }
    }

    // ------------------------------------------------------------------
    // coordinate helpers

    fn canvas_origin(&self) -> Point<Pixels> {
        self.canvas_bounds.origin
    }

    fn to_world(&self, screen: Point<Pixels>) -> WPoint {
        self.camera.screen_to_world(screen, self.canvas_origin())
    }

    fn hit_tolerance(&self) -> f64 {
        4.0 / self.camera.zoom
    }

    // ------------------------------------------------------------------
    // mutations

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn set_notice(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some(msg.into());
        cx.notify();
    }

    pub fn set_tool(&mut self, tool: ActiveTool, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        self.drag = DragState::Idle;
        self.draft = None;
        self.tool = tool;
        if tool != ActiveTool::Select {
            self.selection.clear();
        }
        cx.notify();
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        self.history.record(&self.scene);
        for id in self.selection.drain(..) {
            self.scene.remove(id);
        }
        self.mark_dirty();
        cx.notify();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        if self.history.undo(&mut self.scene) {
            self.selection
                .retain(|id| self.scene.get(*id).is_some());
            cx.notify();
        }
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        if self.history.redo(&mut self.scene) {
            self.selection
                .retain(|id| self.scene.get(*id).is_some());
            cx.notify();
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            // Esc commits the edit; switch to Select and select a freshly
            // created text element so it can be moved/styled immediately.
            if let Some(new_id) = self.commit_editing(window, cx) {
                self.tool = ActiveTool::Select;
                self.selection = vec![new_id];
                cx.notify();
            }
            return;
        }
        self.drag = DragState::Idle;
        self.draft = None;
        if !self.selection.is_empty() {
            self.selection.clear();
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // text editing

    fn start_editing(
        &mut self,
        element_id: ElementId,
        is_new: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(el) = self.scene.get(element_id) else {
            return;
        };
        let (text, font_size, wrap_width, min_height) = match &el.kind {
            ElementKind::Text { text, font_size, wrap_width, min_height } => {
                (text.clone(), *font_size, *wrap_width, *min_height)
            }
            _ => return,
        };
        self.editing = Some(EditingState {
            element_id,
            session: TextEditSession::new(&text),
            font_size,
            wrap_width,
            min_height,
            is_new,
        });
        cx.notify();
    }

    /// Commit the in-progress text edit. Returns the id of the element if it
    /// was a newly-created text element with non-empty content — callers use
    /// this to switch to the Select tool and select it (Excalidraw behavior).
    fn commit_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<ElementId> {
        let ed = self.editing.take()?;
        let text = ed.session.text();
        let is_empty = text.trim().is_empty();
        let id = ed.element_id;
        if is_empty {
            // Empty text: remove the element. For a brand-new element the
            // creation was already recorded, so nothing else is needed.
            if !ed.is_new {
                self.history.record(&self.scene);
            }
            self.scene.remove(id);
        } else if let Some(el) = self.scene.get(id) {
            let changed = el.text() != Some(text.as_str());
            if changed {
                if !ed.is_new {
                    self.history.record(&self.scene);
                }
                let (w, h) = measure_text(&text, ed.font_size, ed.wrap_width, ed.min_height, window);
                if let Some(el) = self.scene.get_mut(id) {
                    if let ElementKind::Text { text: t, .. } = &mut el.kind {
                        *t = text.clone();
                    }
                    el.bounds.w = w.max(1.0);
                    el.bounds.h = h.max(1.0);
                }
                self.mark_dirty();
            }
        }
        cx.notify();
        // Signal a freshly created, non-empty text element so the caller can
        // switch to Select and select it.
        if ed.is_new && !is_empty {
            Some(id)
        } else {
            None
        }
    }

    /// Add a text element with AI-generated content near the current
    /// selection (or at the viewport center). Bounds are estimated here and
    /// precisely measured during the next render.
    pub fn insert_ai_text(&mut self, text: String, cx: &mut Context<Self>) {
        let origin = if let Some(id) = self.selection.first() {
            self.scene
                .get(*id)
                .map(|e| WPoint::new(e.bounds.x, e.bounds.bottom() + 24.0))
        } else {
            None
        };
        let origin = origin.unwrap_or_else(|| {
            let center_screen = point(
                self.canvas_bounds.origin.x + self.canvas_bounds.size.width * 0.5,
                self.canvas_bounds.origin.y + self.canvas_bounds.size.height * 0.5,
            );
            let c = self.camera.screen_to_world(center_screen, self.canvas_origin());
            WPoint::new(c.x - 150.0, c.y - 40.0)
        });
        self.history.record(&self.scene);
        let mut el = Element::new_text(origin, text, self.style.clone());
        // Rough estimate; render() refines with the real text system.
        let lines = el.text().map(|t| t.lines().count()).unwrap_or(1).max(1);
        let max_chars = el
            .text()
            .map(|t| t.lines().map(|l| l.chars().count()).max().unwrap_or(1))
            .unwrap_or(1);
        el.bounds.w = (max_chars as f64 * DEFAULT_FONT_SIZE).max(1.0);
        el.bounds.h = lines as f64 * DEFAULT_FONT_SIZE * LINE_HEIGHT;
        let id = self.scene.add(el);
        self.pending_measure.push(id);
        self.selection = vec![id];
        self.mark_dirty();
        cx.notify();
    }

    /// Content of the first selected text element, if any.
    pub fn selected_text_content(&self) -> Option<String> {
        self.selection
            .iter()
            .find_map(|id| self.scene.get(*id))
            .and_then(|el| el.text().map(str::to_string))
    }

    // ------------------------------------------------------------------
    // AI panel

    fn toggle_ai_panel(&mut self, cx: &mut Context<Self>) {
        if self.ai_panel.take().is_none() {
            let weak = cx.weak_entity();
            self.ai_panel = Some(cx.new(|cx| AiPanel::new(weak, cx)));
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // persistence

    fn save(&mut self, save_as: bool, cx: &mut Context<Self>) {
        let path = if save_as || self.file_path.is_none() {
            let dialog = rfd::FileDialog::new()
                .add_filter("Boundless 场景", &["boundless"])
                .set_file_name("untitled.boundless");
            match dialog.save_file() {
                Some(p) => p,
                None => return,
            }
        } else {
            self.file_path.clone().unwrap()
        };
        let file = SceneFile::new(&self.scene, self.camera);
        let result = serde_json::to_string_pretty(&file)
            .map_err(anyhow::Error::from)
            .and_then(|json| std::fs::write(&path, json).map_err(anyhow::Error::from));
        match result {
            Ok(()) => {
                self.file_path = Some(path.clone());
                self.dirty = false;
                self.set_notice(format!("已保存到 {}", path.display()), cx);
            }
            Err(e) => self.set_notice(format!("保存失败: {e}"), cx),
        }
    }

    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Boundless 场景", &["boundless"])
            .pick_file()
        else {
            return;
        };
        let result = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|json| SceneFile::parse(&json));
        match result {
            Ok(file) => {
                self.scene.restore(file.elements);
                self.camera = file.camera;
                self.selection.clear();
                self.history = History::new();
                self.file_path = Some(path.clone());
                self.dirty = false;
                self.set_notice(format!("已打开 {}", path.display()), cx);
            }
            Err(e) => self.set_notice(format!("打开失败: {e}"), cx),
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // zoom actions

    fn zoom_by(&mut self, factor: f64, cx: &mut Context<Self>) {
        let center = point(
            self.canvas_bounds.origin.x + self.canvas_bounds.size.width * 0.5,
            self.canvas_bounds.origin.y + self.canvas_bounds.size.height * 0.5,
        );
        self.camera.zoom_at(factor, center, self.canvas_origin());
        cx.notify();
    }

    fn zoom_reset(&mut self, cx: &mut Context<Self>) {
        let center = point(
            self.canvas_bounds.origin.x + self.canvas_bounds.size.width * 0.5,
            self.canvas_bounds.origin.y + self.canvas_bounds.size.height * 0.5,
        );
        let factor = 1.0 / self.camera.zoom;
        self.camera.zoom_at(factor, center, self.canvas_origin());
        cx.notify();
    }

    // ------------------------------------------------------------------
    // mouse handlers

    fn on_left_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window);
        let world = self.to_world(event.position);

        // While editing: clicks inside the text move the caret; outside commits.
        if let Some(ed_id) = self.editing.as_ref().map(|e| e.element_id) {
            if let Some(el) = self.scene.get(ed_id) {
                if el.bounds.inflate(8.0, 8.0).contains(world) {
                    if let Some(char_off) = self.char_index_for_screen(event.position, window) {
                        if let Some(ed) = self.editing.as_mut() {
                            ed.session.set_caret(char_off, event.modifiers.shift);
                        }
                        cx.notify();
                    }
                    return;
                }
            }
            // Committing on outside-click: if a brand-new text element was
            // just created, switch to Select and select it (Excalidraw style).
            if let Some(new_id) = self.commit_editing(window, cx) {
                self.tool = ActiveTool::Select;
                self.selection = vec![new_id];
                cx.notify();
                return;
            }
        }

        match self.tool {
            ActiveTool::Hand => {
                self.drag = DragState::Panning {
                    last_screen: event.position,
                };
            }
            ActiveTool::Select => self.select_down(event, world, cx),
            ActiveTool::Rectangle | ActiveTool::Diamond | ActiveTool::Ellipse => {
                self.drag = DragState::Drawing { start: world };
            }
            ActiveTool::Arrow | ActiveTool::Line => {
                self.drag = DragState::Drawing { start: world };
            }
            ActiveTool::Pen => {
                self.drag = DragState::Freedraw {
                    points: vec![world],
                };
            }
            ActiveTool::Text => {
                let hit = self.scene.hit_test(world, self.hit_tolerance());
                if let Some(id) = hit.filter(|id| {
                    self.scene.get(*id).is_some_and(|e| e.is_text())
                }) {
                    self.start_editing(id, false, cx);
                } else {
                    // Drag to size the text box first; release to edit.
                    self.drag = DragState::Drawing { start: world };
                }
            }
            ActiveTool::Eraser => {
                let mut removed = false;
                if let Some(id) = self.scene.hit_test(world, self.hit_tolerance()) {
                    self.history.record(&self.scene);
                    self.scene.remove(id);
                    self.selection.retain(|s| *s != id);
                    removed = true;
                    self.mark_dirty();
                }
                self.drag = DragState::Erasing {
                    removed_any: removed,
                };
                cx.notify();
            }
        }
        cx.notify();
    }

    fn select_down(&mut self, event: &MouseDownEvent, world: WPoint, cx: &mut Context<Self>) {
        // 1. resize handles first (screen-space hit test).
        if !self.selection.is_empty() {
            if let Some(bounds) = self.selection_bounds_world() {
                let screen_bounds = self.world_bounds_to_screen(bounds);
                for (handle, rect) in handle_rects(screen_bounds) {
                    if rect.contains(&event.position) {
                        let originals: Vec<Element> = self
                            .selection
                            .iter()
                            .filter_map(|id| self.scene.get(*id).cloned())
                            .collect();
                        self.drag = DragState::Resizing {
                            handle,
                            original_bounds: bounds,
                            originals,
                            recorded: false,
                        };
                        return;
                    }
                }
            }
        }

        // 2. element hit test.
        if let Some(id) = self.scene.hit_test(world, self.hit_tolerance()) {
            let already = self.selection.contains(&id);
            if event.modifiers.shift {
                if already {
                    self.selection.retain(|s| *s != id);
                } else {
                    self.selection.push(id);
                }
            } else if !already {
                self.selection = vec![id];
            }
            // Double-click on a text element starts editing.
            if event.click_count == 2
                && self.scene.get(id).is_some_and(|e| e.is_text())
                && !event.modifiers.shift
            {
                self.start_editing(id, false, cx);
                return;
            }
            self.drag = DragState::Moving {
                last_world: world,
                recorded: false,
            };
        } else {
            // 3. empty canvas: marquee (or clear selection).
            let base = if event.modifiers.shift {
                self.selection.clone()
            } else {
                Vec::new()
            };
            if !event.modifiers.shift {
                self.selection.clear();
            }
            self.drag = DragState::Marquee {
                start: world,
                current: world,
                base_selection: base,
            };
        }
        cx.notify();
    }

    fn on_middle_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag = DragState::Panning {
            last_screen: event.position,
        };
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let world = self.to_world(event.position);
        // Take the drag state out to avoid borrowing conflicts while
        // mutating the scene/history.
        let drag = std::mem::take(&mut self.drag);
        match drag {
            DragState::Idle => {
                // Hover detection: which element/handle is under the cursor?
                // Used by render() to pick a move/resize cursor that hints the
                // available action.
                let new_handle = if !self.selection.is_empty()
                    && self.tool == ActiveTool::Select
                {
                    if let Some(bounds) = self.selection_bounds_world() {
                        let screen_bounds = self.world_bounds_to_screen(bounds);
                        crate::render::handle_rects(screen_bounds)
                            .into_iter()
                            .find(|(_, rect)| rect.contains(&event.position))
                            .map(|(h, _)| h)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let new_over_element = new_handle.is_none()
                    && self.tool == ActiveTool::Select
                    && self
                        .scene
                        .hit_test(world, self.hit_tolerance())
                        .is_some();
                if new_over_element != self.hover_over_element || new_handle != self.hover_handle
                {
                    self.hover_over_element = new_over_element;
                    self.hover_handle = new_handle;
                    cx.notify();
                }
                self.drag = DragState::Idle;
            }
            DragState::Panning { mut last_screen } => {
                let dx = event.position.x - last_screen.x;
                let dy = event.position.y - last_screen.y;
                self.camera.pan_by_screen(dx, dy);
                last_screen = event.position;
                self.drag = DragState::Panning { last_screen };
                cx.notify();
            }
            DragState::Drawing { start } => {
                self.update_draft(start, world, event.modifiers.shift);
                self.drag = DragState::Drawing { start };
                cx.notify();
            }
            DragState::Freedraw { mut points } => {
                let min_dist = 2.0 / self.camera.zoom;
                if points
                    .last()
                    .is_none_or(|last| last.distance(world) > min_dist)
                {
                    points.push(world);
                    cx.notify();
                }
                self.drag = DragState::Freedraw { points };
            }
            DragState::Moving {
                mut last_world,
                mut recorded,
            } => {
                let dx = world.x - last_world.x;
                let dy = world.y - last_world.y;
                if dx != 0.0 || dy != 0.0 {
                    if !recorded {
                        self.history.record(&self.scene);
                        recorded = true;
                        self.mark_dirty();
                    }
                    for id in &self.selection {
                        if let Some(el) = self.scene.get_mut(*id) {
                            el.translate(dx, dy);
                        }
                    }
                    last_world = world;
                    cx.notify();
                }
                self.drag = DragState::Moving {
                    last_world,
                    recorded,
                };
            }
            DragState::Resizing {
                handle,
                original_bounds,
                originals,
                mut recorded,
            } => {
                let new_bounds = handle.resize_bounds(original_bounds, world);
                let (sx, sy) = if original_bounds.w > 1e-6 && original_bounds.h > 1e-6 {
                    (
                        new_bounds.w / original_bounds.w,
                        new_bounds.h / original_bounds.h,
                    )
                } else {
                    (1.0, 1.0)
                };
                if !recorded {
                    self.history.record(&self.scene);
                    recorded = true;
                    self.mark_dirty();
                }
                let pivot = WPoint::new(original_bounds.x, original_bounds.y);
                let is_corner = matches!(
                    handle,
                    crate::render::Handle::Nw
                        | crate::render::Handle::Ne
                        | crate::render::Handle::Se
                        | crate::render::Handle::Sw
                );
                let is_horizontal_edge = matches!(
                    handle,
                    crate::render::Handle::E | crate::render::Handle::W
                );
                for original in &originals {
                    let mut scaled = original.clone();
                    if let ElementKind::Text { font_size, wrap_width, min_height, .. } = &mut scaled.kind {
                        // Text resize is handle-aware:
                        //  - corner: scale font (and wrap width) uniformly
                        //  - horizontal edge (E/W): change wrap width, keep font
                        //  - vertical edge (N/S): change frame height (min_height),
                        //    keep font size and wrap width — text keeps its
                        //    layout; extra height is blank space below the text.
                        if is_corner {
                            let scale = (sx.abs() + sy.abs()) / 2.0;
                            *font_size = (*font_size * scale.max(0.05)).clamp(4.0, 400.0);
                            if let Some(w) = wrap_width {
                                *w = (*w * scale).max(4.0);
                            }
                            *min_height = min_height.map(|h| (h * scale).max(4.0));
                        } else if is_horizontal_edge {
                            // Enable/update wrapping at the new width.
                            *wrap_width = Some(new_bounds.w.max(8.0));
                        } else {
                            // Vertical edge (N/S): font size unchanged; width
                            // reverts to fit content (drop wrap_width) and only
                            // the frame height follows the drag.
                            *wrap_width = None;
                            *min_height = Some(new_bounds.h.max(*font_size * LINE_HEIGHT));
                        }
                        // Position the element at the new top-left; width/height
                        // will be recomputed by pending_measure.
                        scaled.bounds.x = new_bounds.x;
                        scaled.bounds.y = new_bounds.y;
                    } else {
                        scaled.rescale(sx, sy, pivot);
                    }
                    if scaled.is_text() {
                        self.pending_measure.push(original.id);
                    }
                    if let Some(el) = self.scene.get_mut(original.id) {
                        *el = scaled;
                    }
                }
                self.drag = DragState::Resizing {
                    handle,
                    original_bounds,
                    originals,
                    recorded,
                };
                cx.notify();
            }
            DragState::Marquee {
                start,
                base_selection,
                ..
            } => {
                let current = world;
                let area = WBounds::from_corners(start, current);
                let mut new_selection = base_selection.clone();
                for id in self.scene.elements_in(&area) {
                    if !new_selection.contains(&id) {
                        new_selection.push(id);
                    }
                }
                self.selection = new_selection;
                self.drag = DragState::Marquee {
                    start,
                    current,
                    base_selection,
                };
                cx.notify();
            }
            DragState::Erasing { mut removed_any } => {
                if let Some(id) = self.scene.hit_test(world, self.hit_tolerance()) {
                    if !removed_any {
                        self.history.record(&self.scene);
                        removed_any = true;
                    }
                    self.scene.remove(id);
                    self.selection.retain(|s| *s != id);
                    self.mark_dirty();
                    cx.notify();
                }
                self.drag = DragState::Erasing { removed_any };
            }
        }
    }

    fn update_draft(&mut self, start: WPoint, current: WPoint, constrain: bool) {
        let mut end = current;
        if constrain {
            // Shift: squares / circles / straight lines.
            let dx = current.x - start.x;
            let dy = current.y - start.y;
            end = if matches!(self.tool, ActiveTool::Arrow | ActiveTool::Line) {
                // snap to 15°
                let angle = dy.atan2(dx);
                let snapped = (angle / (15f64.to_radians())).round() * 15f64.to_radians();
                let len = (dx * dx + dy * dy).sqrt();
                WPoint::new(start.x + len * snapped.cos(), start.y + len * snapped.sin())
            } else {
                let side = dx.abs().max(dy.abs());
                WPoint::new(
                    start.x + side * dx.signum(),
                    start.y + side * dy.signum(),
                )
            };
        }
        let bounds = WBounds::from_corners(start, end);
        let kind = match self.tool {
            ActiveTool::Rectangle => Some(ElementKind::Rectangle),
            ActiveTool::Diamond => Some(ElementKind::Diamond),
            ActiveTool::Ellipse => Some(ElementKind::Ellipse),
            ActiveTool::Line => Some(ElementKind::Line {
                points: relative_points(start, end, bounds),
            }),
            ActiveTool::Arrow => Some(ElementKind::Arrow {
                points: relative_points(start, end, bounds),
                end_arrowhead: true,
                start_arrowhead: false,
            }),
            // Text tool: show a dashed rectangle as the box being sized.
            ActiveTool::Text => {
                let mut style = self.style.clone();
                style.stroke_style = StrokeStyle::Dashed;
                style.roughness = 0.0;
                self.draft = Some(Element::new(ElementKind::Rectangle, bounds, style));
                return;
            }
            _ => None,
        };
        if let Some(kind) = kind {
            self.draft = Some(Element::new(kind, bounds, self.style.clone()));
        }
    }

    fn on_left_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        let world = self.to_world(event.position);
        match std::mem::take(&mut self.drag) {
            DragState::Drawing { start } => {
                // Click without drag creates a default-sized shape; a drag
                // commits the draft.
                if self.draft.is_none() {
                    let size = 120.0;
                    let end = WPoint::new(start.x + size, start.y + size * 0.75);
                    self.update_draft(start, end, false);
                }
                if self.tool == ActiveTool::Text {
                    // Text tool: turn the dragged box into a text element
                    // (wrap_width + min_height from the box) and start editing.
                    if let Some(draft) = self.draft.take() {
                        let b = draft.bounds;
                        let dragged = start.distance(WPoint::new(b.right(), b.bottom())) > 8.0;
                        self.history.record(&self.scene);
                        let mut el =
                            Element::new_text(WPoint::new(b.x, b.y), String::new(), self.style.clone());
                        if dragged {
                            if let ElementKind::Text {
                                font_size,
                                wrap_width,
                                min_height,
                                ..
                            } = &mut el.kind
                            {
                                // Size the default font to fill the box height:
                                // a single line is font_size * LINE_HEIGHT, so
                                // font_size = box_height / LINE_HEIGHT fills it.
                                *font_size = (b.h / LINE_HEIGHT).clamp(8.0, 200.0);
                                *wrap_width = Some(b.w.max(20.0));
                                *min_height = Some(b.h);
                            }
                        }
                        let id = self.scene.add(el);
                        self.pending_measure.push(id);
                        self.mark_dirty();
                        self.selection = vec![id];
                        self.start_editing(id, true, cx);
                    }
                } else if let Some(mut el) = self.draft.take() {
                    if matches!(el.kind, ElementKind::Line { .. } | ElementKind::Arrow { .. })
                        && world.distance(start) < 1e-6
                    {
                        // zero-length line: skip
                    } else {
                        if el.is_point_based() {
                            el.normalize_point_bounds();
                        }
                        self.history.record(&self.scene);
                        let id = self.scene.add(el);
                        self.selection = vec![id];
                        self.mark_dirty();
                        self.tool = ActiveTool::Select;
                    }
                }
            }
            DragState::Freedraw { points } => {
                if points.len() >= 2 {
                    self.history.record(&self.scene);
                    let el = Element::from_absolute_points(
                        |points| ElementKind::Freedraw { points },
                        points,
                        self.style.clone(),
                    );
                    let id = self.scene.add(el);
                    self.selection = vec![id];
                    self.mark_dirty();
                    self.tool = ActiveTool::Select;
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn on_middle_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.drag, DragState::Panning { .. }) {
            self.drag = DragState::Idle;
            cx.notify();
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(20.0));
        if event.modifiers.control || event.modifiers.platform {
            let factor = (-delta.y.to_f64() * 0.002).exp();
            self.camera
                .zoom_at(factor, event.position, self.canvas_origin());
        } else if event.modifiers.shift {
            self.camera.pan_by_screen(delta.y, px(0.0));
        } else {
            self.camera.pan_by_screen(delta.x, delta.y);
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_none() {
            return;
        }
        let key = event.keystroke.key.as_str();
        let shift = event.keystroke.modifiers.shift;
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        // Only intercept editing control keys here. Character keys must fall
        // through to GPUI's translate_message path so WM_CHAR is generated and
        // reaches replace_text_in_range via EntityInputHandler. Calling
        // stop_propagation on a character key would suppress WM_CHAR entirely.
        let handled = if key == "escape" {
            // Esc commits the edit; switch to Select and select a freshly
            // created text element so it can be moved/styled immediately.
            if let Some(new_id) = self.commit_editing(window, cx) {
                self.tool = ActiveTool::Select;
                self.selection = vec![new_id];
                cx.notify();
            }
            true
        } else if let Some(ed) = self.editing.as_mut() {
            match key {
                "enter" => {
                    ed.session.insert("\n");
                    true
                }
                "backspace" => {
                    ed.session.backspace();
                    true
                }
                "delete" => {
                    ed.session.delete_forward();
                    true
                }
                "left" => {
                    ed.session.move_left(shift);
                    true
                }
                "right" => {
                    ed.session.move_right(shift);
                    true
                }
                "up" => {
                    ed.session.move_vertical(-1, shift);
                    true
                }
                "down" => {
                    ed.session.move_vertical(1, shift);
                    true
                }
                "home" => {
                    ed.session.move_home(shift);
                    true
                }
                "end" => {
                    ed.session.move_end(shift);
                    true
                }
                "a" if ctrl => {
                    ed.session.select_all();
                    true
                }
                // Character and other keys: do NOT handle here — let GPUI
                // translate the keystroke to WM_CHAR so text input flows in.
                _ => false,
            }
        } else {
            false
        };
        if handled {
            cx.stop_propagation();
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // selection helpers

    fn selection_bounds_world(&self) -> Option<WBounds> {
        let mut iter = self.selection.iter().filter_map(|id| self.scene.get(*id));
        let first = iter.next()?;
        Some(iter.fold(first.bounds, |acc, el| acc.union(&el.bounds)))
    }

    fn world_bounds_to_screen(&self, b: WBounds) -> Bounds<Pixels> {
        let origin = self
            .camera
            .world_to_screen(WPoint::new(b.x, b.y), self.canvas_origin());
        Bounds {
            origin,
            size: size(
                self.camera.scale(b.w),
                self.camera.scale(b.h),
            ),
        }
    }

    /// Map a screen point to a char offset in the currently-edited text.
    fn char_index_for_screen(&self, screen: Point<Pixels>, window: &Window) -> Option<usize> {
        let ed = self.editing.as_ref()?;
        let el = self.scene.get(ed.element_id)?;
        let world = self.to_world(screen);
        let text = ed.session.text();
        let color = color_u32(0x000000, 1.0);
        let (lines, line_height) = shape_text(&text, ed.font_size, &self.camera, color, ed.wrap_width, window);
        if lines.is_empty() {
            return Some(0);
        }
        let rel_y = (world.y - el.bounds.y) * self.camera.zoom;
        let line_idx = ((rel_y / line_height.to_f64()).floor() as isize)
            .clamp(0, lines.len() as isize - 1) as usize;
        let rel_x = px(((world.x - el.bounds.x) * self.camera.zoom) as f32);
        let line = &lines[line_idx];
        let byte_in_line = line
            .line
            .index_for_x(rel_x)
            .unwrap_or(line.byte_range.len())
            .min(line.byte_range.len());
        let global_byte = line.byte_range.start + byte_in_line;
        Some(ed.session.rope.byte_to_char(global_byte))
    }

    /// Screen-space caret rect for a char offset in the editing session:
    /// (top-left position, line height).
    fn caret_screen_rect(
        &self,
        el: &Element,
        ed: &EditingState,
        char_off: usize,
        window: &Window,
    ) -> Option<(Point<Pixels>, Pixels)> {
        let text = ed.session.text();
        let color = color_u32(0x000000, 1.0);
        let (lines, line_height) = shape_text(&text, ed.font_size, &self.camera, color, ed.wrap_width, window);
        if lines.is_empty() {
            return None;
        }
        let byte = ed.session.rope.char_to_byte(char_off);
        let origin =
            self.camera
                .world_to_screen(WPoint::new(el.bounds.x, el.bounds.y), self.canvas_origin());
        for (i, line) in lines.iter().enumerate() {
            let in_line = byte >= line.byte_range.start
                && (byte <= line.byte_range.end || i == lines.len() - 1);
            if in_line {
                let x = line
                    .line
                    .x_for_index((byte - line.byte_range.start).min(line.byte_range.len()));
                return Some((
                    point(origin.x + x, origin.y + line_height * i as f32),
                    line_height,
                ));
            }
        }
        None
    }
}

fn relative_points(start: WPoint, end: WPoint, bounds: WBounds) -> Vec<WPoint> {
    let origin = WPoint::new(bounds.x, bounds.y);
    vec![start - origin, end - origin]
}

// ---------------------------------------------------------------------
// EntityInputHandler: IME + text input for the canvas text editor
// ---------------------------------------------------------------------

impl EntityInputHandler for BoardView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let ed = self.editing.as_ref()?;
        let text = ed.session.text();
        let start = utf16_to_utf8(&text, range.start);
        let end = utf16_to_utf8(&text, range.end);
        text.get(start..end).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let ed = self.editing.as_ref()?;
        Some(UTF16Selection {
            range: ed.session.utf16_selection(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        let ed = self.editing.as_ref()?;
        let marked = ed.session.marked.clone()?;
        let text = ed.session.text();
        let start = utf8_to_utf16(&text, crate::text::char_to_byte(&ed.session.rope, marked.start));
        let end = utf8_to_utf16(&text, crate::text::char_to_byte(&ed.session.rope, marked.end));
        Some(start..end)
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = self.editing.as_mut() {
            ed.session.marked = None;
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ed) = self.editing.as_mut() {
            ed.session.replace_utf16_range(range, text);
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ed) = self.editing.as_mut() {
            ed.session
                .replace_and_mark_utf16_range(range, new_text, new_selected_range);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let ed = self.editing.as_ref()?;
        let el = self.scene.get(ed.element_id)?;
        let text = ed.session.text();
        let byte_off = utf16_to_utf8(&text, range_utf16.end);
        let char_off = ed.session.rope.byte_to_char(byte_off);
        let (origin, line_height) = self.caret_screen_rect(el, ed, char_off, window)?;
        Some(Bounds {
            origin,
            size: size(px(2.0), line_height),
        })
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let char_off = self.char_index_for_screen(point, window)?;
        let ed = self.editing.as_ref()?;
        let text = ed.session.text();
        let byte = crate::text::char_to_byte(&ed.session.rope, char_off);
        Some(utf8_to_utf16(&text, byte))
    }
}

// ---------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------

struct TextPaintItem {
    lines: Vec<ShapedTextLine>,
    origin: Point<Pixels>,
    line_height: Pixels,
}

struct EditingPaint {
    item: TextPaintItem,
    text_bounds: Bounds<Pixels>,
    selection_quads: Vec<PaintQuad>,
    marked_quads: Vec<PaintQuad>,
    caret_quad: Option<PaintQuad>,
}

struct BoardPaint {
    grid: Vec<PaintQuad>,
    paths: Vec<ReadyPath>,
    texts: Vec<TextPaintItem>,
    editing: Option<EditingPaint>,
    selection_outline: Option<PaintQuad>,
    handles: Vec<PaintQuad>,
    marquee: Option<PaintQuad>,
}

impl BoardView {
    fn build_paint(&self, viewport: Bounds<Pixels>, window: &Window) -> BoardPaint {
        let origin = viewport.origin;
        let margin = px(120.0);
        let view_world_tl = self.camera.screen_to_world(
            point(viewport.origin.x - margin, viewport.origin.y - margin),
            origin,
        );
        let view_world_br = self.camera.screen_to_world(
            point(
                viewport.origin.x + viewport.size.width + margin,
                viewport.origin.y + viewport.size.height + margin,
            ),
            origin,
        );
        let view_world = WBounds::from_corners(view_world_tl, view_world_br);

        let grid = dot_grid(&self.camera, viewport, color_u32(GRID_COLOR, 1.0));

        let editing_id = self.editing.as_ref().map(|e| e.element_id);
        let mut paths = Vec::new();
        let mut texts = Vec::new();

        for el in &self.scene.elements {
            if !el.bounds.inflate(40.0, 40.0).intersects(&view_world) {
                continue;
            }
            if Some(el.id) == editing_id {
                continue; // painted via the editing session
            }
            match &el.kind {
                ElementKind::Text { text, font_size, .. } => {
                    let color = color_u32(el.style.stroke, el.style.opacity);
                    let (lines, line_height) =
                        shape_text(text, *font_size, &self.camera, color, el.wrap_width(), window);
                    let screen_origin = self.camera.world_to_screen(
                        WPoint::new(el.bounds.x, el.bounds.y),
                        origin,
                    );
                    texts.push(TextPaintItem {
                        lines,
                        origin: screen_origin,
                        line_height,
                    });
                }
                _ => {
                    paths.extend(paths_for_element(el, &self.camera, origin));
                }
            }
        }

        // In-progress shapes.
        if let Some(draft) = &self.draft {
            paths.extend(paths_for_element(draft, &self.camera, origin));
        }
        if let DragState::Freedraw { points } = &self.drag {
            if points.len() >= 2 {
                let draft = Element::from_absolute_points(
                    |points| ElementKind::Freedraw { points },
                    points.clone(),
                    self.style.clone(),
                );
                paths.extend(paths_for_element(&draft, &self.camera, origin));
            }
        }

        // Selection overlay.
        let mut selection_outline = None;
        let mut handle_quads = Vec::new();
        if !self.selection.is_empty() && matches!(self.tool, ActiveTool::Select) {
            if let Some(bounds) = self.selection_bounds_world() {
                let screen = self.world_bounds_to_screen(bounds);
                // Pad the selection frame to visually enclose rough hand-drawn
                // ink. Overshoot ≈ roughness + half stroke width (the 2.0×
                // max_randomness_offset is the upper bound, but continuous
                // curves like ellipses smooth it out, so use 1.0×).
                // smooth ≈ 1px, sketchy ≈ 2px, bold ≈ 3px (at zoom 1).
                let pad_world = self
                    .selection
                    .iter()
                    .filter_map(|id| self.scene.get(*id))
                    .map(|el| el.style.roughness as f64 + el.style.stroke_width * 0.5)
                    .fold(1.0f64, f64::max);
                let pad = self.camera.scale(pad_world).max(px(2.0));
                let screen = Bounds {
                    origin: point(screen.origin.x - pad, screen.origin.y - pad),
                    size: size(screen.size.width + pad * 2.0, screen.size.height + pad * 2.0),
                };
                let sel = color_u32(SELECTION_COLOR, 1.0);
                selection_outline = Some(outline(screen, sel, BorderStyle::Solid));
                for (_, rect) in handle_rects(screen) {
                    handle_quads.push(quad(
                        rect,
                        px(2.0),
                        color_u32(0xffffff, 1.0),
                        px(1.0),
                        sel,
                        BorderStyle::Solid,
                    ));
                }
            }
        }

        // Marquee overlay.
        let mut marquee = None;
        if let DragState::Marquee { start, current, .. } = &self.drag {
            let b = WBounds::from_corners(*start, *current);
            let screen = self.world_bounds_to_screen(b);
            marquee = Some(quad(
                screen,
                px(0.0),
                color_u32(SELECTION_COLOR, 0.08),
                px(1.0),
                color_u32(SELECTION_COLOR, 0.6),
                BorderStyle::Solid,
            ));
        }

        // Editing session paint.
        let editing = self.build_editing_paint(origin, window);

        BoardPaint {
            grid,
            paths,
            texts,
            editing,
            selection_outline,
            handles: handle_quads,
            marquee,
        }
    }

    fn build_editing_paint(&self, canvas_origin: Point<Pixels>, window: &Window) -> Option<EditingPaint> {
        let ed = self.editing.as_ref()?;
        let el = self.scene.get(ed.element_id)?;
        let text = ed.session.text();
        let color = color_u32(el.style.stroke, el.style.opacity);
        let (lines, line_height) = shape_text(&text, ed.font_size, &self.camera, color, ed.wrap_width, window);
        let origin = self
            .camera
            .world_to_screen(WPoint::new(el.bounds.x, el.bounds.y), canvas_origin);

        let sel_color = color_u32(SELECTION_COLOR, 0.35);
        let mark_color = color_u32(SELECTION_COLOR, 1.0);

        // Selection highlight.
        let mut selection_quads = Vec::new();
        let sel = ed.session.selection();
        if !sel.is_empty() {
            let start_byte = ed.session.rope.char_to_byte(sel.start);
            let end_byte = ed.session.rope.char_to_byte(sel.end);
            for (i, line) in lines.iter().enumerate() {
                let overlap_start = start_byte.max(line.byte_range.start);
                let overlap_end = end_byte.min(line.byte_range.end);
                if overlap_start < overlap_end || (overlap_start == overlap_end && sel.start == sel.end) {
                    let x0 = line.line.x_for_index(
                        (overlap_start - line.byte_range.start).min(line.byte_range.len()),
                    );
                    let x1 = line.line.x_for_index(
                        (overlap_end - line.byte_range.start).min(line.byte_range.len()),
                    );
                    if x1 > x0 {
                        selection_quads.push(fill(
                            Bounds {
                                origin: point(origin.x + x0, origin.y + line_height * i as f32),
                                size: size(x1 - x0, line_height),
                            },
                            sel_color,
                        ));
                    }
                }
            }
        }

        // IME marked-text underline.
        let mut marked_quads = Vec::new();
        if let Some(marked) = &ed.session.marked {
            let start_byte = ed.session.rope.char_to_byte(marked.start);
            let end_byte = ed.session.rope.char_to_byte(marked.end);
            for (i, line) in lines.iter().enumerate() {
                let overlap_start = start_byte.max(line.byte_range.start);
                let overlap_end = end_byte.min(line.byte_range.end);
                if overlap_start <= overlap_end && marked.end > marked.start {
                    let x0 = line.line.x_for_index(
                        (overlap_start - line.byte_range.start).min(line.byte_range.len()),
                    );
                    let x1 = line.line.x_for_index(
                        (overlap_end - line.byte_range.start).min(line.byte_range.len()),
                    );
                    marked_quads.push(fill(
                        Bounds {
                            origin: point(
                                origin.x + x0,
                                origin.y + line_height * (i as f32 + 1.0) - px(2.0),
                            ),
                            size: size((x1 - x0).max(px(2.0)), px(1.5)),
                        },
                        mark_color,
                    ));
                }
            }
        }

        // Caret.
        let caret_quad = if sel.is_empty() {
            self.caret_screen_rect(el, ed, sel.start, window)
                .map(|(pos, lh)| {
                    fill(
                        Bounds {
                            origin: pos,
                            size: size(px(1.5), lh),
                        },
                        color_u32(0x1e1e1e, 1.0),
                    )
                })
        } else {
            None
        };

        let text_bounds = Bounds {
            origin,
            size: size(
                self.camera.scale(el.bounds.w.max(1.0)),
                self.camera.scale(el.bounds.h.max(1.0)),
            ),
        };

        Some(EditingPaint {
            item: TextPaintItem {
                lines,
                origin,
                line_height,
            },
            text_bounds,
            selection_quads,
            marked_quads,
            caret_quad,
        })
    }
}

// ---------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------

impl Render for BoardView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Refine bounds of freshly-inserted text elements now that we have
        // access to the text system.
        if !self.pending_measure.is_empty() {
            let pending = std::mem::take(&mut self.pending_measure);
            for id in pending {
                let info = self.scene.get(id).and_then(|el| match &el.kind {
                    ElementKind::Text { text, font_size, wrap_width, min_height } => {
                        Some((text.clone(), *font_size, *wrap_width, *min_height))
                    }
                    _ => None,
                });
                if let Some((text, font_size, wrap_width, min_height)) = info {
                    let (w, h) = measure_text(&text, font_size, wrap_width, min_height, window);
                    if let Some(el) = self.scene.get_mut(id) {
                        el.bounds.w = w.max(1.0);
                        el.bounds.h = h.max(1.0);
                    }
                }
            }
        }

        let this = cx.entity();
        let focus = self.focus_handle.clone();
        let editing = self.editing.is_some();

        // Compute the canvas cursor BEFORE constructing the canvas element so
        // it can be applied to the canvas's own hitbox (GPUI picks the cursor
        // from the topmost element under the pointer; setting it on the outer
        // div alone is overridden by the canvas's default).
        let cursor = if self.editing.is_some() {
            // While editing text, always show the text caret cursor.
            CursorStyle::IBeam
        } else {
            match (&self.drag, self.tool) {
            (DragState::Panning { .. }, _) => CursorStyle::OpenHand,
            (DragState::Moving { .. }, _) => CursorStyle::PointingHand,
            (DragState::Resizing { handle, .. }, _) => match handle {
                crate::render::Handle::N | crate::render::Handle::S => CursorStyle::ResizeUpDown,
                crate::render::Handle::E | crate::render::Handle::W => CursorStyle::ResizeLeftRight,
                // Diagonal handles: GPUI's Windows backend doesn't map
                // ResizeUpLeftDownRight/ResizeUpRightDownLeft (they fall back
                // to Arrow). Use PointingHand so at least the cursor changes
                // and hints the handle is interactive.
                _ => CursorStyle::PointingHand,
            },
            (_, ActiveTool::Hand) => CursorStyle::OpenHand,
            (_, ActiveTool::Select) => {
                if let Some(h) = self.hover_handle {
                    match h {
                        crate::render::Handle::N | crate::render::Handle::S => {
                            CursorStyle::ResizeUpDown
                        }
                        crate::render::Handle::E | crate::render::Handle::W => {
                            CursorStyle::ResizeLeftRight
                        }
                        // Diagonal handles fall back to a pointing hand (see
                        // the Resizing branch comment for why).
                        _ => CursorStyle::PointingHand,
                    }
                } else if self.hover_over_element {
                    CursorStyle::PointingHand
                } else {
                    CursorStyle::Arrow
                }
            }
            (_, ActiveTool::Text) => CursorStyle::IBeam,
            (_, ActiveTool::Eraser) => CursorStyle::PointingHand,
            _ => CursorStyle::Crosshair,
            }
        };

        let canvas_el = canvas(
            {
                let this = this.clone();
                move |bounds, window, cx| {
                    this.update(cx, |v, _| v.canvas_bounds = bounds);
                    let view = this.read(cx);
                    view.build_paint(bounds, window)
                }
            },
            {
                let focus = focus.clone();
                move |_bounds, paint: BoardPaint, window, cx| {
                    for q in paint.grid {
                        window.paint_quad(q);
                    }
                    for rp in paint.paths {
                        window.paint_path(rp.path, rp.color);
                    }
                    for item in &paint.texts {
                        paint_text_item(item, window, cx);
                    }
                    if let Some(ed) = &paint.editing {
                        for q in &ed.selection_quads {
                            window.paint_quad(q.clone());
                        }
                        paint_text_item(&ed.item, window, cx);
                        for q in &ed.marked_quads {
                            window.paint_quad(q.clone());
                        }
                        if let Some(q) = &ed.caret_quad {
                            window.paint_quad(q.clone());
                        }
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(ed.text_bounds, this.clone()),
                            cx,
                        );
                    }
                    if let Some(q) = paint.selection_outline {
                        window.paint_quad(q);
                    }
                    for q in paint.handles {
                        window.paint_quad(q);
                    }
                    if let Some(q) = paint.marquee {
                        window.paint_quad(q);
                    }
                }
            },
        )
        .absolute()
        .inset_0()
        .cursor(cursor);

        div()
            .key_context(if editing { "Editor" } else { "Board" })
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(rgb(BG_COLOR))
            .font_family(".SystemUIFont")
            .text_color(rgb(0x1e1e1e))
            .cursor(cursor)
            .on_action(cx.listener(|this, _: &Undo, window, cx| this.undo(window, cx)))
            .on_action(cx.listener(|this, _: &Redo, window, cx| this.redo(window, cx)))
            .on_action(cx.listener(|this, _: &SaveScene, _window, cx| this.save(false, cx)))
            .on_action(cx.listener(|this, _: &OpenScene, window, cx| this.open(window, cx)))
            .on_action(cx.listener(|this, _: &DeleteSelection, _window, cx| this.delete_selection(cx)))
            .on_action(cx.listener(|this, _: &CancelOp, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _window, cx| this.zoom_by(1.25, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _window, cx| this.zoom_by(0.8, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _window, cx| this.zoom_reset(cx)))
            .on_action(cx.listener(|this, _: &SelectTool, window, cx| this.set_tool(ActiveTool::Select, window, cx)))
            .on_action(cx.listener(|this, _: &HandTool, window, cx| this.set_tool(ActiveTool::Hand, window, cx)))
            .on_action(cx.listener(|this, _: &RectTool, window, cx| this.set_tool(ActiveTool::Rectangle, window, cx)))
            .on_action(cx.listener(|this, _: &DiamondTool, window, cx| this.set_tool(ActiveTool::Diamond, window, cx)))
            .on_action(cx.listener(|this, _: &EllipseTool, window, cx| this.set_tool(ActiveTool::Ellipse, window, cx)))
            .on_action(cx.listener(|this, _: &ArrowTool, window, cx| this.set_tool(ActiveTool::Arrow, window, cx)))
            .on_action(cx.listener(|this, _: &LineTool, window, cx| this.set_tool(ActiveTool::Line, window, cx)))
            .on_action(cx.listener(|this, _: &PenTool, window, cx| this.set_tool(ActiveTool::Pen, window, cx)))
            .on_action(cx.listener(|this, _: &TextTool, window, cx| this.set_tool(ActiveTool::Text, window, cx)))
            .on_action(cx.listener(|this, _: &EraserTool, window, cx| this.set_tool(ActiveTool::Eraser, window, cx)))
            .on_action(cx.listener(|this, _: &ToggleAi, _window, cx| this.toggle_ai_panel(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_left_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_middle_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(canvas_el)
            .child(self.render_toolbar(cx))
            .child(self.render_style_bar(cx))
            .child(self.render_zoom_bar(cx))
            .child(self.render_notice_bar())
            .children(self.ai_panel.clone())
    }
}

fn paint_text_item(item: &TextPaintItem, window: &mut Window, cx: &mut App) {
    for (i, line) in item.lines.iter().enumerate() {
        let origin = point(
            item.origin.x,
            item.origin.y + item.line_height * i as f32,
        );
        let _ = line.line.paint(origin, item.line_height, window, cx);
    }
}

// ---------------------------------------------------------------------
// Chrome: toolbar / style bar / zoom bar / notice bar
// ---------------------------------------------------------------------

const STROKE_COLORS: [u32; 5] = [0x1e1e1e, 0xe03131, 0x2f9e44, 0x1971c2, 0xf08c00];
const BG_COLORS: [Option<u32>; 5] = [None, Some(0xffc9c9), Some(0xb2f2bb), Some(0xa5d8ff), Some(0xffec99)];
const STROKE_WIDTHS: [(f64, f32); 3] = [(1.0, 1.0), (2.0, 2.0), (4.0, 4.0)];
const ROUGHNESSES: [f32; 3] = [0.0, 1.0, 2.0];

const ICON_ACTIVE: u32 = 0x1a5fd7;
const ICON_NORMAL: u32 = 0x3b3b3b;

fn bar_container() -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .p_1()
        .bg(rgb(0xffffff))
        .border_1()
        .border_color(rgb(0xe3e2df))
        .rounded_lg()
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

fn bar_button(label: &'static str, active: bool) -> Stateful<Div> {
    let mut b = div()
        .id(label)
        .flex()
        .items_center()
        .justify_center()
        .h_7()
        .px_2()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .child(label);
    if active {
        b = b.bg(rgb(0xdce8ff)).text_color(rgb(0x1a5fd7));
    } else {
        b = b.hover(|s| s.bg(rgb(0xf1f0ee)));
    }
    b
}

/// A square icon button for the toolbar. `id` must be unique. `icon_child` is
/// rendered inside a 20×20 box so vector icons fill the button.
fn bar_icon_button(
    id: impl Into<gpui::ElementId>,
    active: bool,
    icon_child: impl IntoElement,
) -> Stateful<Div> {
    let mut b = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size_7()
        .rounded_md()
        .cursor_pointer()
        .child(div().size(px(crate::icons::S)).child(icon_child));
    if active {
        b = b.bg(rgb(0xdce8ff));
    } else {
        b = b.hover(|s| s.bg(rgb(0xf1f0ee)));
    }
    b
}

/// Icon color: blue when active, dark gray otherwise.
fn icon_color(active: bool) -> Hsla {
    color_u32(if active { ICON_ACTIVE } else { ICON_NORMAL }, 1.0)
}

impl BoardView {
    fn apply_style_to_selection(
        &mut self,
        apply: impl Fn(&mut ElementStyle) + Copy,
        cx: &mut Context<Self>,
    ) {
        apply(&mut self.style);
        if !self.selection.is_empty() {
            self.history.record(&self.scene);
            for id in &self.selection {
                if let Some(el) = self.scene.get_mut(*id) {
                    apply(&mut el.style);
                    el.seed = crate::scene::new_seed();
                }
            }
            self.mark_dirty();
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::icons as ic;
        let weak = cx.weak_entity();

        // (tool enum, label, icon factory)
        let tools: [(ActiveTool, &str, fn(Hsla) -> gpui::AnyElement); 10] = [
            (ActiveTool::Select, "选择", |c| ic::select(c).into_any_element()),
            (ActiveTool::Hand, "抓手", |c| ic::hand(c).into_any_element()),
            (ActiveTool::Rectangle, "矩形", |c| ic::rectangle(c).into_any_element()),
            (ActiveTool::Diamond, "菱形", |c| ic::diamond(c).into_any_element()),
            (ActiveTool::Ellipse, "椭圆", |c| ic::ellipse(c).into_any_element()),
            (ActiveTool::Arrow, "箭头", |c| ic::arrow(c).into_any_element()),
            (ActiveTool::Line, "直线", |c| ic::line(c).into_any_element()),
            (ActiveTool::Pen, "画笔", |c| ic::pen(c).into_any_element()),
            (ActiveTool::Text, "文本", |c| ic::text(c).into_any_element()),
            (ActiveTool::Eraser, "橡皮", |c| ic::eraser(c).into_any_element()),
        ];

        let mut bar = bar_container();
        for (tool, label, icon_fn) in tools {
            let weak = weak.clone();
            let active = self.tool == tool;
            bar = bar.child(
                bar_icon_button(gpui::ElementId::Name(label.into()), active, icon_fn(icon_color(active)))
                    .on_click(move |_, window, cx| {
                        weak.update(cx, |this, cx| this.set_tool(tool, window, cx)).ok();
                    }),
            );
        }

        let weak_undo = weak.clone();
        let weak_redo = weak.clone();
        let weak_save = weak.clone();
        let weak_open = weak.clone();
        let weak_ai = weak.clone();
        let ai_active = self.ai_panel.is_some();
        bar = bar
            .child(div().w(px(1.0)).h_5().bg(rgb(0xe3e2df)).mx_1())
            .child(
                bar_icon_button("撤销", false, ic::undo(icon_color(false)))
                    .on_click(move |_, window, cx| {
                        weak_undo.update(cx, |this, cx| this.undo(window, cx)).ok();
                    }),
            )
            .child(
                bar_icon_button("重做", false, ic::redo(icon_color(false)))
                    .on_click(move |_, window, cx| {
                        weak_redo.update(cx, |this, cx| this.redo(window, cx)).ok();
                    }),
            )
            .child(div().w(px(1.0)).h_5().bg(rgb(0xe3e2df)).mx_1())
            .child(
                bar_icon_button("保存", false, ic::save(icon_color(false)))
                    .on_click(move |_, _, cx| {
                        weak_save.update(cx, |this, cx| this.save(false, cx)).ok();
                    }),
            )
            .child(
                bar_icon_button("打开", false, ic::open(icon_color(false)))
                    .on_click(move |_, window, cx| {
                        weak_open.update(cx, |this, cx| this.open(window, cx)).ok();
                    }),
            )
            .child(div().w(px(1.0)).h_5().bg(rgb(0xe3e2df)).mx_1())
            .child(
                bar_icon_button("AI", ai_active, ic::ai(icon_color(ai_active)))
                    .on_click(move |_, _, cx| {
                        weak_ai.update(cx, |this, cx| this.toggle_ai_panel(cx)).ok();
                    }),
            );

        div()
            .absolute()
            .top_3()
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(bar)
    }

    fn render_style_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let show = self.tool.is_drawing() || !self.selection.is_empty();
        // Stroke width / roughness only affect shapes (not text). Hide them
        // when the selection contains only text elements to avoid the
        // impression that styling "doesn't work" on text.
        let only_text = !self.selection.is_empty()
            && self
                .selection
                .iter()
                .all(|id| self.scene.get(*id).is_some_and(|e| e.is_text()));
        let show_shape_options = self.tool.is_drawing() || !only_text;

        let mut bar = bar_container().flex_col().items_start().gap_2().p_2();

        // Stroke colors.
        let mut row = div().flex().flex_row().gap_1();
        for color in STROKE_COLORS {
            let weak = weak.clone();
            let active = self.style.stroke == color;
            let mut swatch = div()
                .id(gpui::ElementId::named_usize("stroke", color as usize))
                .size_5()
                .rounded_sm()
                .bg(rgb(color))
                .border_1()
                .cursor_pointer();
            swatch = if active {
                swatch.border_color(rgb(SELECTION_COLOR)).border_2()
            } else {
                swatch.border_color(rgb(0xcccccc))
            };
            row = row.child(swatch.on_click(move |_, _, cx| {
                weak.update(cx, |this, cx| {
                    this.apply_style_to_selection(|s| s.stroke = color, cx)
                })
                .ok();
            }));
        }
        bar = bar.child(row);

        // Background colors.
        let mut row = div().flex().flex_row().gap_1();
        for (ix, color) in BG_COLORS.iter().enumerate() {
            let weak = weak.clone();
            let color = *color;
            let active = self.style.background == color;
            let mut swatch = div()
                .id(gpui::ElementId::named_usize("bg", ix))
                .size_5()
                .rounded_sm()
                .border_1()
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .text_xs();
            swatch = match color {
                Some(c) => swatch.bg(rgb(c)).child(""),
                None => swatch.bg(rgb(0xffffff)).text_color(rgb(0x999999)).child("无"),
            };
            swatch = if active {
                swatch.border_color(rgb(SELECTION_COLOR)).border_2()
            } else {
                swatch.border_color(rgb(0xcccccc))
            };
            row = row.child(swatch.on_click(move |_, _, cx| {
                weak.update(cx, |this, cx| {
                    this.apply_style_to_selection(|s| s.background = color, cx)
                })
                .ok();
            }));
        }
        bar = bar.child(row);

        // Stroke width (shapes only): three lines of increasing thickness.
        if show_shape_options {
            use crate::icons as ic;
            let mut row = div().flex().flex_row().gap_1();
            for (width, px_w) in STROKE_WIDTHS {
                let weak = weak.clone();
                let active = (self.style.stroke_width - width).abs() < 1e-6;
                row = row.child(
                    bar_icon_button(
                        gpui::ElementId::named_usize("sw", width as usize),
                        active,
                        ic::stroke_width_icon(icon_color(active), px_w),
                    )
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.apply_style_to_selection(|s| s.stroke_width = width, cx)
                        })
                        .ok();
                    }),
                );
            }
            bar = bar.child(row);

            // Roughness (shapes only): three lines of increasing wobble.
            let mut row = div().flex().flex_row().gap_1();
            for roughness in ROUGHNESSES {
                let weak = weak.clone();
                let active = (self.style.roughness - roughness).abs() < 1e-6;
                row = row.child(
                    bar_icon_button(
                        gpui::ElementId::named_usize("rg", roughness.to_bits() as usize),
                        active,
                        ic::roughness_icon(icon_color(active), roughness),
                    )
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.apply_style_to_selection(|s| s.roughness = roughness, cx)
                        })
                        .ok();
                    }),
                );
            }
            bar = bar.child(row);
        }

        let wrapper = div()
            .absolute()
            .left_3()
            .top_24()
            .flex()
            .when(show, |d| d.child(bar));
        wrapper
    }

    fn render_zoom_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let percent = format!("{:.0}%", self.camera.zoom * 100.0);

        let weak_out = weak.clone();
        let weak_in = weak.clone();
        let weak_reset = weak.clone();
        let weak_fit = weak.clone();

        div().absolute().bottom_3().right_3().flex().child(
            bar_container()
                .child(bar_button("−", false).on_click(move |_, _, cx| {
                    weak_out.update(cx, |this, cx| this.zoom_by(0.8, cx)).ok();
                }))
                .child(
                    div()
                        .w_12()
                        .text_center()
                        .text_sm()
                        .child(percent),
                )
                .child(bar_button("+", false).on_click(move |_, _, cx| {
                    weak_in.update(cx, |this, cx| this.zoom_by(1.25, cx)).ok();
                }))
                .child(bar_button("重置", false).on_click(move |_, _, cx| {
                    weak_reset.update(cx, |this, cx| this.zoom_reset(cx)).ok();
                }))
                .child(bar_button("适应", false).on_click(move |_, _, cx| {
                    weak_fit
                        .update(cx, |this, cx| {
                            if let Some(bounds) = this.scene.content_bounds() {
                                let viewport = this.canvas_bounds.size;
                                this.camera.zoom_to_fit(bounds, viewport);
                                cx.notify();
                            }
                        })
                        .ok();
                })),
        )
    }

    fn render_notice_bar(&self) -> impl IntoElement {
        let mut text = String::new();
        if let Some(path) = &self.file_path {
            text.push_str(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("untitled"),
            );
            if self.dirty {
                text.push_str(" •");
            }
        } else if self.dirty {
            text.push_str("未保存 •");
        }
        if let Some(notice) = &self.notice {
            if !text.is_empty() {
                text.push_str("　");
            }
            text.push_str(notice);
        }
        div()
            .absolute()
            .bottom_3()
            .left_3()
            .text_xs()
            .text_color(rgb(0x888888))
            .child(text)
    }
}
