//! BoardView: the infinite canvas — scene, camera, tools, selection,
//! text editing and the surrounding chrome (toolbar, style bar, zoom bar).

use std::ops::Range;
use std::path::PathBuf;

use gpui::prelude::*;
use gpui::*;

use crate::ai::canvas_ops::{CanvasOp, CanvasOpError, CanvasOpOutcome, CanvasStyle};
use crate::ai::panel::AiPanel;
use crate::camera::Camera;
use crate::history::History;
use crate::render::rough::{color_u32, paths_for_element, ReadyPath};
use crate::render::{dot_grid, handle_rects, measure_text, point_handle_rects, ShapedTextLine};
use crate::scene::{
    Element, ElementId, ElementKind, ElementStyle, LineType, Scene, SceneFile, StrokeStyle,
    TextAlign, WBounds, WPoint, DEFAULT_FONT_SIZE, LINE_HEIGHT,
};
use crate::text::{utf16_to_utf8, utf8_to_utf16, TextEditSession};
use crate::tools::{ActiveTool, DragState, PointTarget};
use gpui_component::{Icon, IconName};

actions!(
    boundless,
    [
        Undo,
        Redo,
        SaveScene,
        OpenScene,
        DeleteSelection,
        CancelOp,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        SelectTool,
        HandTool,
        RectTool,
        DiamondTool,
        EllipseTool,
        ArrowTool,
        LineTool,
        PenTool,
        TextTool,
        EraserTool,
        ToggleAi,
        BringToFront,
        SendToBack,
        BringForward,
        SendBackward,
        Quit,
        CheckForUpdates,
    ]
);

pub const SELECTION_COLOR: u32 = 0x4c9ffe;
const BG_COLOR: u32 = 0xffffff;
const GRID_COLOR: u32 = 0xcccccc;

pub struct EditingState {
    pub element_id: ElementId,
    pub session: TextEditSession,
    pub font_size: f64,
    pub font_family: String,
    pub wrap_width: Option<f64>,
    pub min_height: Option<f64>,
    /// Container the edited text is bound to (label); None = standalone.
    pub container_id: Option<ElementId>,
    pub text_align: TextAlign,
    /// True when the text element was created by this editing session.
    pub is_new: bool,
}

/// Which way to reorder the selection in the z-stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerOp {
    ToFront,
    ToBack,
    Forward,
    Backward,
}

/// An open right-click context menu: anchored at a screen position, with an
/// optional vertex target (when right-clicked on a vertex handle of a
/// selected line/arrow).
#[derive(Clone, Debug)]
pub struct ContextMenuState {
    pub position: Point<Pixels>,
    pub vertex: Option<usize>,
}

/// Progress / completion messages from the updater download task to the GPUI
/// `cx.spawn` that drains it.
enum DownloadMsg {
    /// 0.0..=1.0 of the artifact.
    Progress(f64),
    /// Final outcome: the verified artifact path on success, an error string on
    /// failure (download or signature verification).
    Done(std::result::Result<std::path::PathBuf, String>),
}

pub struct BoardView {
    pub scene: Scene,
    pub camera: Camera,
    pub tool: ActiveTool,
    drag: DragState,
    pub selection: Vec<ElementId>,
    pub style: ElementStyle,
    /// Default font size for newly created text. Updated whenever the user
    /// picks a size in the style bar (Excalidraw-style "last used wins").
    text_font_size: f64,
    /// Default font family for newly created text.
    text_font_family: String,
    /// Default alignment for newly created text.
    text_align: TextAlign,
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
    /// Hovered vertex/midpoint control handle of a single selected
    /// line/arrow (wins over `hover_handle` where they overlap).
    hover_point: Option<PointTarget>,
    /// True while a temporary pen stroke is in progress (Shift + left-drag from
    /// any non-Pen tool). The current tool is left untouched, so releasing the
    /// mouse returns to it — Excalidraw's "hold a modifier to sketch" gesture.
    temp_pen: bool,
    /// Ink "笔锋" effect (crate::ink): per-point width from simulated
    /// pressure (slow = thick, fast = thin). On by default so new strokes get
    /// the handwriting feel; off yields the legacy uniform-width stroke.
    /// Applies to new strokes only — existing strokes keep their baked widths.
    pen_taper: bool,
    /// True while a temporary canvas pan is in progress (Ctrl + left-drag from
    /// any tool). Like temp_pen, the active tool is left untouched so releasing
    /// the mouse returns to it.
    temp_pan: bool,
    /// Last-known keyboard modifier state, updated on every mouse move so the
    /// render-time cursor can hint the pending gesture (Ctrl => pan hand,
    /// Shift => pen crosshair) before the button is pressed.
    modifiers: Modifiers,
    /// Open right-click context menu (layer ops / delete / delete-vertex).
    /// None when closed.
    context_menu: Option<ContextMenuState>,
    /// Whether the dot grid is painted behind the canvas. Toggled from the
    /// zoom-bar; persisted in the scene file.
    show_grid: bool,
    /// Canvas background color (0xRRGGBB) — the "board surface". None = the
    /// default white board. Switchable from the zoom-bar swatches and by the
    /// AI (`set_canvas_background`); persisted in the scene file.
    canvas_background: Option<u32>,
    /// Index of the currently open top-level menu in the Windows in-app menu
    /// bar (None = all collapsed). Unused on macOS, which uses the native
    /// `set_menus` bar; the field compiles everywhere because the menu-bar
    /// rendering is pure GPUI.
    menubar_open: Option<usize>,
    /// Auto-update flow state (check / download / ready-to-restart). Driven by
    /// the `CheckForUpdates` action + a delayed startup poll; see `src/updater.rs`.
    update_state: crate::updater::UpdateState,
    /// Per-element world-geometry cache (render/cache.rs): skips roughr
    /// generation and spline/ribbon rebuilds for unchanged elements. Pan/zoom
    /// never invalidates it; invalidation is by element fingerprint.
    render_cache: crate::render::cache::RenderCache,
    /// Shaped-text cache: skips per-frame text shaping (wrapped paragraphs
    /// shape per character). Keyed by shaping fingerprint incl. zoom.
    text_cache: crate::render::cache::TextCache,
}

impl BoardView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        // Windows Ink: subclass the window to observe WM_POINTER packets so
        // the ink pipeline gets real stylus pressure and the eraser tip.
        // Idempotent; no-op on other platforms.
        crate::platform::init_pen_input(window);
        let view = Self {
            scene: Scene::new(),
            camera: Camera::default(),
            tool: ActiveTool::Select,
            drag: DragState::Idle,
            selection: Vec::new(),
            style: ElementStyle::default(),
            text_font_size: DEFAULT_FONT_SIZE,
            text_font_family: crate::render::HANDWRITTEN_FONT.to_string(),
            text_align: TextAlign::Left,
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
            hover_point: None,
            temp_pen: false,
            pen_taper: true,
            temp_pan: false,
            modifiers: Modifiers::default(),
            context_menu: None,
            show_grid: false,
            canvas_background: None,
            menubar_open: None,
            update_state: crate::updater::UpdateState::default(),
            render_cache: crate::render::cache::RenderCache::new(),
            text_cache: crate::render::cache::TextCache::new(),
        };
        // Auto-update: poll silently 30s after launch, then every 4h. Only a
        // real available update surfaces (silent = no "up to date" / transient
        // error banners). Stops when the view is dropped.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(30))
                .await;
            if this
                .update(cx, |this, cx| this.check_for_updates(cx, true))
                .is_err()
            {
                return;
            }
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(4 * 60 * 60))
                    .await;
                if this
                    .update(cx, |this, cx| this.check_for_updates(cx, true))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
        view
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

    /// Start a freehand drag: the rough-jitter seed is generated once (stable
    /// across draft frames), and ink capture begins with the current zoom,
    /// base stroke width, and 笔锋 state. The first pointer sample is pushed
    /// immediately (with its hardware pressure, when a stylus is live) so
    /// taps aren't lost.
    fn begin_freedraw(&self, world: WPoint) -> DragState {
        let hw = self.hw_pressure();
        let mut collector = crate::ink::InkCollector::new(self.camera.zoom, self.pen_taper);
        collector.push_with_pressure(world, hw);
        DragState::Freedraw {
            collector,
            seed: crate::scene::new_seed(),
        }
    }

    /// Freshest stylus pressure for the ink collector (`None` = velocity
    /// simulation). The eraser tip never contributes pressure — it routes to
    /// the eraser instead.
    fn hw_pressure(&self) -> Option<f64> {
        crate::platform::latest_pen_sample()
            .filter(|s| !s.eraser)
            .map(|s| s.pressure as f64)
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

    /// Remove an element; bound labels of a removed container go with it.
    fn remove_element(&mut self, id: ElementId) {
        self.scene.remove(id);
        let labels: Vec<ElementId> = self
            .scene
            .elements
            .iter()
            .filter_map(|e| match &e.kind {
                ElementKind::Text {
                    container_id: Some(cid),
                    ..
                } if *cid == id => Some(e.id),
                _ => None,
            })
            .collect();
        for lid in labels {
            self.scene.remove(lid);
        }
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        self.history.record(&self.scene);
        let ids: Vec<ElementId> = self.selection.drain(..).collect();
        for id in ids {
            self.remove_element(id);
        }
        self.mark_dirty();
        cx.notify();
    }

    /// Delete one vertex of the single selected line/arrow (keeping >= 2
    /// points), re-placing any bound label. Used by the context menu's
    /// "delete vertex" item.
    fn delete_vertex(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(id) = self.selection.first().copied() else {
            return;
        };
        let can = self
            .scene
            .get(id)
            .map(|el| el.is_point_based() && el.absolute_points().len() > 2)
            .unwrap_or(false);
        if !can {
            return;
        }
        self.history.record(&self.scene);
        if let Some(el) = self.scene.get_mut(id) {
            el.remove_point(index);
        }
        self.update_container_labels(&[id]);
        self.context_menu = None;
        self.mark_dirty();
        cx.notify();
    }

    /// Reorder the selected elements in the z-stack. No-op (and no history
    /// entry) when the selection is empty or the move can't apply.
    fn reorder_layers(&mut self, op: LayerOp, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        let ids = self.selection.clone();
        let can = match op {
            LayerOp::ToFront => self.scene.can_front(&ids),
            LayerOp::ToBack => self.scene.can_back(&ids),
            LayerOp::Forward => self.scene.can_forward(&ids),
            LayerOp::Backward => self.scene.can_backward(&ids),
        };
        if !can {
            return;
        }
        self.history.record(&self.scene);
        match op {
            LayerOp::ToFront => self.scene.move_to_front(&ids),
            LayerOp::ToBack => self.scene.send_to_back(&ids),
            LayerOp::Forward => self.scene.bring_forward(&ids),
            LayerOp::Backward => self.scene.send_backward(&ids),
        }
        self.context_menu = None;
        self.mark_dirty();
        cx.notify();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        if self.history.undo(&mut self.scene) {
            self.selection.retain(|id| self.scene.get(*id).is_some());
            cx.notify();
        }
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        if self.history.redo(&mut self.scene) {
            self.selection.retain(|id| self.scene.get(*id).is_some());
            cx.notify();
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // An open context menu absorbs Esc: close it instead of clearing the
        // selection / committing an edit.
        if self.context_menu.is_some() {
            self.context_menu = None;
            cx.notify();
            return;
        }
        if self.editing.is_some() {
            // Esc commits the edit; switch to Select and select a freshly
            // created text element so it can be moved/styled immediately.
            // A bound label selects its container instead.
            if let Some(new_id) = self.commit_editing(window, cx) {
                self.tool = ActiveTool::Select;
                let target = self
                    .scene
                    .get(new_id)
                    .and_then(|e| e.container_id())
                    .unwrap_or(new_id);
                self.selection = vec![target];
                cx.notify();
            }
            return;
        }
        // Nothing in-app left to cancel. Fall through to the system behaviour
        // that GPUI's key dispatch otherwise swallows: macOS exits fullscreen
        // (or un-zooms a maximized window) via the window's default
        // `cancelOperation:`, but GPUI overrides `doCommandBySelector:` without
        // forwarding it up the responder chain, and this `escape` binding
        // consumes the event. Restore that cascade here so Esc can leave the
        // fullscreen/zoomed state the green button put us in.
        let had_state = !matches!(self.drag, DragState::Idle)
            || self.draft.is_some()
            || !self.selection.is_empty();
        self.drag = DragState::Idle;
        self.draft = None;
        self.selection.clear();

        if had_state {
            cx.notify();
        } else if window.is_fullscreen() {
            window.toggle_fullscreen();
        } else if window.is_maximized() {
            window.zoom_window();
        }
    }

    // ------------------------------------------------------------------
    // text editing

    fn start_editing(&mut self, element_id: ElementId, is_new: bool, cx: &mut Context<Self>) {
        let Some(el) = self.scene.get(element_id) else {
            return;
        };
        let (text, font_size, font_family, wrap_width, min_height, container_id, text_align) =
            match &el.kind {
                ElementKind::Text {
                    text,
                    font_size,
                    font_family,
                    wrap_width,
                    min_height,
                    container_id,
                    text_align,
                } => (
                    text.clone(),
                    *font_size,
                    font_family.clone(),
                    *wrap_width,
                    *min_height,
                    *container_id,
                    *text_align,
                ),
                _ => return,
            };
        self.editing = Some(EditingState {
            element_id,
            session: TextEditSession::new(&text),
            font_size,
            font_family,
            wrap_width,
            min_height,
            container_id,
            text_align,
            is_new,
        });
        cx.notify();
    }

    /// Double-click on a shape (or text-tool click on one): edit the shape's
    /// bound text label, creating and centering one on first use
    /// (Excalidraw-style labeled shapes).
    fn edit_container_label(&mut self, container_id: ElementId, cx: &mut Context<Self>) {
        // Edit the existing label if the shape already has one.
        let existing = self.scene.elements.iter().find_map(|e| match &e.kind {
            ElementKind::Text {
                container_id: Some(cid),
                ..
            } if *cid == container_id => Some(e.id),
            _ => None,
        });
        if let Some(id) = existing {
            self.selection = vec![id];
            self.start_editing(id, false, cx);
            return;
        }
        let Some(cb) = self.scene.get(container_id).map(|c| c.bounds) else {
            return;
        };
        let mut el = self.new_text_element(WPoint::new(cb.x, cb.y), String::new());
        if let ElementKind::Text {
            wrap_width,
            container_id: cid,
            text_align,
            ..
        } = &mut el.kind
        {
            *wrap_width = Some(cb.w.max(10.0));
            *cid = Some(container_id);
            // Labels are centered on their container by default.
            *text_align = TextAlign::Center;
        }
        place_label(&mut el.bounds, cb, TextAlign::Center);
        self.history.record(&self.scene);
        let id = self.scene.add(el);
        self.pending_measure.push(id);
        self.mark_dirty();
        self.selection = vec![id];
        self.start_editing(id, true, cx);
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
                let (mut w, h) = measure_text(
                    &text,
                    ed.font_size,
                    ed.wrap_width,
                    ed.min_height,
                    &ed.font_family,
                    window,
                );
                // Standalone wrapped boxes keep their wrap width (selection
                // frame matches the editing outline); bound labels hug their
                // content width so centering is true centering.
                if ed.container_id.is_none() {
                    if let Some(ww) = ed.wrap_width {
                        w = ww.max(w);
                    }
                }
                let cb = ed
                    .container_id
                    .and_then(|cid| self.scene.get(cid))
                    .map(|c| c.bounds);
                if let Some(el) = self.scene.get_mut(id) {
                    if let ElementKind::Text { text: t, .. } = &mut el.kind {
                        *t = text.clone();
                    }
                    el.bounds.w = w.max(1.0);
                    el.bounds.h = h.max(1.0);
                    // A bound label keeps its alignment on the container.
                    if let Some(cb) = cb {
                        place_label(&mut el.bounds, cb, ed.text_align);
                    }
                }
                self.mark_dirty();
            }
        }
        // A committed bound label must not stay selected on its own — labels
        // aren't independently selectable; select the container instead.
        if let Some(cid) = ed.container_id {
            if self.selection.contains(&id) {
                self.selection.retain(|s| *s != id);
                if self.scene.get(cid).is_some() && !self.selection.contains(&cid) {
                    self.selection.push(cid);
                }
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

    /// Create a text element using the board's current defaults (style,
    /// font size, font family, alignment).
    fn new_text_element(&self, origin: WPoint, text: String) -> Element {
        let mut el = Element::new_text(origin, text, self.style.clone());
        if let ElementKind::Text {
            font_size,
            font_family,
            text_align,
            ..
        } = &mut el.kind
        {
            *font_size = self.text_font_size;
            *font_family = self.text_font_family.clone();
            *text_align = self.text_align;
        }
        el.bounds.h = self.text_font_size * LINE_HEIGHT;
        el
    }

    /// Text elements the style bar currently controls: selected text
    /// elements plus the bound labels of selected containers.
    fn panel_text_ids(&self) -> Vec<ElementId> {
        let mut ids: Vec<ElementId> = Vec::new();
        for id in &self.selection {
            if self.scene.get(*id).is_some_and(|e| e.is_text()) {
                ids.push(*id);
            }
        }
        for el in &self.scene.elements {
            if let Some(cid) = el.container_id() {
                if self.selection.contains(&cid) && !ids.contains(&el.id) {
                    ids.push(el.id);
                }
            }
        }
        ids
    }

    /// Elements the shape-style buttons (stroke width, roughness) act on:
    /// the selection itself — or the containers when only bound labels are
    /// selected, since stroke width / roughness don't affect text.
    fn panel_shape_ids(&self) -> Vec<ElementId> {
        let bound_labels_only = !self.selection.is_empty()
            && self.selection.iter().all(|id| {
                self.scene.get(*id).is_some_and(|e| {
                    e.container_id()
                        .is_some_and(|cid| self.scene.get(cid).is_some())
                })
            });
        if bound_labels_only {
            self.selection
                .iter()
                .filter_map(|id| self.scene.get(*id))
                .filter_map(|e| e.container_id())
                .collect()
        } else {
            self.selection.clone()
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
            let c = self
                .camera
                .screen_to_world(center_screen, self.canvas_origin());
            WPoint::new(c.x - 150.0, c.y - 40.0)
        });
        self.history.record(&self.scene);
        let mut el = self.new_text_element(origin, text);
        // Rough estimate; render() refines with the real text system.
        let lines = el.text().map(|t| t.lines().count()).unwrap_or(1).max(1);
        let max_chars = el
            .text()
            .map(|t| t.lines().map(|l| l.chars().count()).max().unwrap_or(1))
            .unwrap_or(1);
        el.bounds.w = (max_chars as f64 * self.text_font_size).max(1.0);
        el.bounds.h = lines as f64 * self.text_font_size * LINE_HEIGHT;
        let id = self.scene.add(el);
        self.pending_measure.push(id);
        self.selection = vec![id];
        self.mark_dirty();
        cx.notify();
    }

    /// Apply a single AI drawing operation: translate the op into an element,
    /// record it to history (so it's undoable), add it to the scene, and — for
    /// text — queue it for precise remeasure during the next render. This is
    /// the main-thread entry point driven by the AI agent's tool calls.
    pub fn apply_canvas_op(
        &mut self,
        op: CanvasOp,
        pre_assigned_id: Option<uuid::Uuid>,
        cx: &mut Context<Self>,
    ) -> CanvasOpOutcome {
        // Merge the op's optional style over the board's current style so that
        // omitted fields inherit "last used wins" — the same behavior as a
        // hand-drawn shape.
        let style = self.style.clone();
        let styled = |s: CanvasStyle| CanvasStyle::merge_into(s, style);

        let outcome: CanvasOpOutcome = match op {
            CanvasOp::Rectangle {
                x,
                y,
                w,
                h,
                style,
                text,
            } => {
                if !(w > 0.0 && h > 0.0) {
                    return Err(CanvasOpError::invalid_args("矩形宽高必须为正数"));
                }
                let Some(id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                self.history.record(&self.scene);
                let el = Element::new_with_id(
                    id,
                    ElementKind::Rectangle,
                    WBounds::new(x, y, w, h),
                    styled(style),
                );
                let added = self.scene.add(el);
                #[cfg(debug_assertions)]
                if let Some(f) = self
                    .scene
                    .get(added)
                    .map(|e| (e.style.background, e.style.fill_style, e.style.opacity))
                {
                    let (f, fs, op) = f;
                    eprintln!(
                        "[op-diag] shape id={} background={:?} fill_style={:?} opacity={}",
                        &added.to_string()[..8],
                        f,
                        fs,
                        op
                    );
                }
                if let Some(t) = text.filter(|t| !t.is_empty()) {
                    self.add_bound_label(added, WBounds::new(x, y, w, h), t);
                }
                Ok(format!("已添加矩形 id={}", &added.to_string()[..8]))
            }
            CanvasOp::Ellipse {
                x,
                y,
                w,
                h,
                style,
                text,
            } => {
                if !(w > 0.0 && h > 0.0) {
                    return Err(CanvasOpError::invalid_args("椭圆宽高必须为正数"));
                }
                let Some(id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                self.history.record(&self.scene);
                let el = Element::new_with_id(
                    id,
                    ElementKind::Ellipse,
                    WBounds::new(x, y, w, h),
                    styled(style),
                );
                let added = self.scene.add(el);
                #[cfg(debug_assertions)]
                if let Some(f) = self
                    .scene
                    .get(added)
                    .map(|e| (e.style.background, e.style.fill_style, e.style.opacity))
                {
                    let (f, fs, op) = f;
                    eprintln!(
                        "[op-diag] shape id={} background={:?} fill_style={:?} opacity={}",
                        &added.to_string()[..8],
                        f,
                        fs,
                        op
                    );
                }
                if let Some(t) = text.filter(|t| !t.is_empty()) {
                    self.add_bound_label(added, WBounds::new(x, y, w, h), t);
                }
                Ok(format!("已添加椭圆 id={}", &added.to_string()[..8]))
            }
            CanvasOp::Polygon { points, style } => {
                if points.len() < 3 || points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
                    return Err(CanvasOpError::invalid_args(
                        "多边形至少需要三个有限坐标点",
                    ));
                }
                let Some(id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                self.history.record(&self.scene);
                let pts: Vec<WPoint> = points.iter().map(|p| WPoint::new(p.x, p.y)).collect();
                let mut el = Element::from_absolute_points_with_id(
                    id,
                    |points| ElementKind::Polygon { points },
                    pts,
                    styled(style),
                );
                el.style.fill_style = crate::scene::FillStyle::Solid;
                let added = self.scene.add(el);
                #[cfg(debug_assertions)]
                if let Some(f) = self
                    .scene
                    .get(added)
                    .map(|e| (e.style.background, e.style.fill_style, e.style.opacity))
                {
                    let (f, fs, op) = f;
                    eprintln!(
                        "[op-diag] shape id={} background={:?} fill_style={:?} opacity={}",
                        &added.to_string()[..8],
                        f,
                        fs,
                        op
                    );
                }
                Ok(format!("已添加多边形 id={}", &added.to_string()[..8]))
            }
            CanvasOp::Diamond {
                x,
                y,
                w,
                h,
                style,
                text,
            } => {
                if !(w > 0.0 && h > 0.0) {
                    return Err(CanvasOpError::invalid_args("菱形宽高必须为正数"));
                }
                let Some(id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                self.history.record(&self.scene);
                let el = Element::new_with_id(
                    id,
                    ElementKind::Diamond,
                    WBounds::new(x, y, w, h),
                    styled(style),
                );
                let added = self.scene.add(el);
                #[cfg(debug_assertions)]
                if let Some(f) = self
                    .scene
                    .get(added)
                    .map(|e| (e.style.background, e.style.fill_style, e.style.opacity))
                {
                    let (f, fs, op) = f;
                    eprintln!(
                        "[op-diag] shape id={} background={:?} fill_style={:?} opacity={}",
                        &added.to_string()[..8],
                        f,
                        fs,
                        op
                    );
                }
                if let Some(t) = text.filter(|t| !t.is_empty()) {
                    self.add_bound_label(added, WBounds::new(x, y, w, h), t);
                }
                Ok(format!("已添加菱形 id={}", &added.to_string()[..8]))
            }
            CanvasOp::Line {
                points,
                style,
                text,
            } => {
                let pts: Vec<WPoint> = points.into_iter().map(Into::into).collect();
                if pts.len() < 2 {
                    return Err(CanvasOpError::invalid_args("至少需要两个坐标点"));
                }
                let Some(id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                self.history.record(&self.scene);
                let el = Element::from_absolute_points_with_id(
                    id,
                    |p| ElementKind::Line { points: p },
                    pts,
                    styled(style),
                );
                let bounds = el.bounds;
                let added = self.scene.add(el);
                #[cfg(debug_assertions)]
                if let Some(f) = self
                    .scene
                    .get(added)
                    .map(|e| (e.style.background, e.style.fill_style, e.style.opacity))
                {
                    let (f, fs, op) = f;
                    eprintln!(
                        "[op-diag] shape id={} background={:?} fill_style={:?} opacity={}",
                        &added.to_string()[..8],
                        f,
                        fs,
                        op
                    );
                }
                if let Some(t) = text.filter(|t| !t.is_empty()) {
                    self.add_bound_label(added, bounds, t);
                }
                Ok(format!("已添加直线 id={}", &added.to_string()[..8]))
            }
            CanvasOp::Arrow {
                points,
                start_arrowhead,
                end_arrowhead,
                style,
                text,
            } => {
                let pts: Vec<WPoint> = points.into_iter().map(Into::into).collect();
                if pts.len() < 2 {
                    return Err(CanvasOpError::invalid_args("至少需要两个坐标点"));
                }
                let Some(id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                self.history.record(&self.scene);
                let el = Element::from_absolute_points_with_id(
                    id,
                    |p| ElementKind::Arrow {
                        points: p,
                        start_arrowhead,
                        end_arrowhead,
                    },
                    pts,
                    styled(style),
                );
                let bounds = el.bounds;
                let added = self.scene.add(el);
                #[cfg(debug_assertions)]
                if let Some(f) = self
                    .scene
                    .get(added)
                    .map(|e| (e.style.background, e.style.fill_style, e.style.opacity))
                {
                    let (f, fs, op) = f;
                    eprintln!(
                        "[op-diag] shape id={} background={:?} fill_style={:?} opacity={}",
                        &added.to_string()[..8],
                        f,
                        fs,
                        op
                    );
                }
                if let Some(t) = text.filter(|t| !t.is_empty()) {
                    self.add_bound_label(added, bounds, t);
                }
                Ok(format!("已添加箭头 id={}", &added.to_string()[..8]))
            }
            CanvasOp::Text {
                x,
                y,
                text,
                font_size,
                align,
                font_family,
                wrap_width,
                style,
            } => {
                let Some(id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                let fs = font_size.unwrap_or(self.text_font_size).max(4.0);
                let ta = align.map(Into::into).unwrap_or(self.text_align);
                let text = crate::ai::canvas_ops::normalize_text(text);
                self.history.record(&self.scene);
                let mut el = Element::new_text(WPoint::new(x, y), text, styled(style));
                // Override the auto-generated id with the pre-assigned one.
                el.id = id;
                if let ElementKind::Text {
                    font_size: fs2,
                    text_align: ta2,
                    font_family: family2,
                    wrap_width: wrap2,
                    ..
                } = &mut el.kind
                {
                    *fs2 = fs;
                    *ta2 = ta;
                    // Font alias → concrete family (unknown aliases degrade to
                    // the hand-drawn default instead of tofu).
                    if let Some(family) = font_family {
                        *family2 = crate::render::resolve_font_family(&family);
                    }
                    *wrap2 = wrap_width;
                }
                // Rough estimate; render() refines with the real text system,
                // matching how insert_ai_text pre-sizes a new text element.
                // With wrap_width the estimated width is capped so the
                // pre-measure box approximates the wrapped layout.
                let lines = el.text().map(|t| t.lines().count()).unwrap_or(1).max(1);
                let max_chars = el
                    .text()
                    .map(|t| t.lines().map(|l| l.chars().count()).max().unwrap_or(1))
                    .unwrap_or(1);
                el.bounds.w = (max_chars as f64 * fs)
                    .min(wrap_width.unwrap_or(f64::MAX))
                    .max(1.0);
                el.bounds.h = lines as f64 * fs * LINE_HEIGHT;
                let added = self.scene.add(el);
                #[cfg(debug_assertions)]
                if let Some(f) = self
                    .scene
                    .get(added)
                    .map(|e| (e.style.background, e.style.fill_style, e.style.opacity))
                {
                    let (f, fs, op) = f;
                    eprintln!(
                        "[op-diag] shape id={} background={:?} fill_style={:?} opacity={}",
                        &added.to_string()[..8],
                        f,
                        fs,
                        op
                    );
                }
                self.pending_measure.push(added);
                Ok(format!("已添加文本 id={}", &added.to_string()[..8]))
            }
            CanvasOp::SetBackground { color } => {
                let label = match color {
                    Some(c) => {
                        self.set_canvas_background(Some(c), cx);
                        format!("画布底色已设为 #{:06x}", c)
                    }
                    None => {
                        self.set_canvas_background(None, cx);
                        "画布底色已恢复白色".to_string()
                    }
                };
                Ok(label)
            }
            CanvasOp::Mindmap { root, cx, cy } => {
                let Some(root_id) = pre_assigned_id else {
                    return Err(CanvasOpError::internal("内部错误：缺少元素 ID"));
                };
                let input = crate::scene::mindmap::MindmapNodeInput::from(&root);
                let center = WPoint::new(cx.unwrap_or(800.0), cy.unwrap_or(500.0));
                let layout = crate::scene::mindmap::layout(&input, center);
                let node_count = layout.nodes.len();
                self.history.record(&self.scene);
                // Links first so node boxes (and labels) paint on top of them.
                for link in &layout.links {
                    let mut style = ElementStyle {
                        stroke: link.stroke,
                        stroke_width: 2.0,
                        roughness: 1.0,
                        ..ElementStyle::default()
                    };
                    style.line_type = LineType::Curved;
                    let el = Element::from_absolute_points(
                        |p| ElementKind::Line { points: p },
                        link.points.clone(),
                        style,
                    );
                    self.scene.add(el);
                }
                for (i, spec) in layout.nodes.iter().enumerate() {
                    let mut style = ElementStyle {
                        stroke: spec.stroke,
                        background: Some(spec.fill),
                        stroke_width: match spec.level {
                            0 => 2.5,
                            1 => 2.0,
                            _ => 1.5,
                        },
                        roughness: 1.0,
                        ..ElementStyle::default()
                    };
                    style.fill_style = crate::scene::FillStyle::Solid;
                    let el = if i == 0 {
                        Element::new_with_id(
                            root_id,
                            ElementKind::Rectangle,
                            spec.bounds,
                            style,
                        )
                    } else {
                        Element::new(ElementKind::Rectangle, spec.bounds, style)
                    };
                    let added = self.scene.add(el);
                    self.add_bound_label(added, spec.bounds, spec.text.clone());
                }
                Ok(format!(
                    "已添加思维导图（{node_count} 个节点）id={}",
                    &root_id.to_string()[..8]
                ))
            }
            CanvasOp::UpdateElement {
                id,
                x,
                y,
                mut text,
                style,
                font_size,
            } => {
                // The model only knows the 8-char id prefix draw tools report
                // back, so resolve by prefix (also accepts a full UUID).
                let Some(uuid) = self.scene.find_by_id_prefix(&id) else {
                    return Err(CanvasOpError::not_found(format!("找不到元素 id={id}")));
                };
                text = text.map(|t| crate::ai::canvas_ops::normalize_text(t));
                self.history.record(&self.scene);
                // Phase 1: update position / style / font size (needs a mutable
                // borrow of the element).
                let mut needs_label_update = false;
                let mut remeasure = false;
                if let Some(el) = self.scene.get_mut(uuid) {
                    if let Some(nx) = x {
                        el.bounds.x = nx;
                    }
                    if let Some(ny) = y {
                        el.bounds.y = ny;
                    }
                    // Style override: omitted fields keep the element's current
                    // style (same "last used wins" overlay as a fresh draw).
                    let base = el.style.clone();
                    el.style = style.merge_into(base);
                    // Determine if this is a standalone text element.
                    if let Some(nt) = &text {
                        if let ElementKind::Text {
                            text: ref mut t, ..
                        } = el.kind
                        {
                            *t = nt.clone();
                        } else {
                            needs_label_update = true;
                        }
                    }
                    // font_size applies only to text elements.
                    if let Some(fs) = font_size {
                        if let ElementKind::Text {
                            font_size: ref mut f,
                            ..
                        } = el.kind
                        {
                            *f = fs.max(4.0);
                            remeasure = true;
                        }
                    }
                }
                if remeasure {
                    self.pending_measure.push(uuid);
                }
                // Phase 2: if the element is a shape (not Text), update or
                // create its bound label. Done in a separate scope so the
                // earlier mutable borrow is released.
                if needs_label_update {
                    if let Some(nt) = &text {
                        // Find existing bound label for this container.
                        let label_id = self.scene.elements.iter().find_map(|e| {
                            if let ElementKind::Text {
                                container_id: Some(cid),
                                ..
                            } = &e.kind
                            {
                                if *cid == uuid {
                                    Some(e.id)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        });
                        if let Some(lid) = label_id {
                            if let Some(label) = self.scene.get_mut(lid) {
                                if let ElementKind::Text {
                                    text: ref mut t, ..
                                } = label.kind
                                {
                                    *t = nt.clone();
                                }
                            }
                            let cb = self.scene.get(uuid).map(|e| e.bounds);
                            if let Some(cb) = cb {
                                if let Some(label) = self.scene.get_mut(lid) {
                                    place_label(&mut label.bounds, cb, TextAlign::Center);
                                }
                            }
                            self.pending_measure.push(lid);
                        } else if let Some(cb) = self.scene.get(uuid).map(|e| e.bounds) {
                            self.add_bound_label(uuid, cb, nt.clone());
                        }
                    }
                }
                // Phase 3: move bound labels to follow the (possibly moved) container.
                if x.is_some() || y.is_some() {
                    let cb = self.scene.get(uuid).map(|e| e.bounds);
                    if let Some(cb) = cb {
                        for e in self.scene.elements.iter_mut() {
                            if let ElementKind::Text {
                                container_id: Some(cid),
                                ..
                            } = &e.kind
                            {
                                if *cid == uuid {
                                    place_label(&mut e.bounds, cb, TextAlign::Center);
                                }
                            }
                        }
                    }
                }
                Ok(format!("已更新元素 id={}", &uuid.to_string()[..8]))
            }
            CanvasOp::DeleteElement { id } => {
                let Some(uuid) = self.scene.find_by_id_prefix(&id) else {
                    return Err(CanvasOpError::not_found(format!("找不到元素 id={id}")));
                };
                self.history.record(&self.scene);
                self.remove_element(uuid);
                Ok(format!("已删除元素 id={}", &uuid.to_string()[..8]))
            }
            CanvasOp::Clear => {
                self.history.record(&self.scene);
                self.scene.restore(Vec::new());
                self.selection.clear();
                self.editing = None;
                Ok("已清空画布".to_string())
            }
        };
        if outcome.is_ok() {
            self.mark_dirty();
            cx.notify();
        }
        outcome
    }

    /// Create a bound text label centered inside a container shape. Mirrors
    /// what `edit_container_label` does for interactive double-click: creates a
    /// `Text` element with `container_id` set, wraps to the container width,
    /// centers it, and queues it for measurement so the real text system
    /// refines its bounds on the next render.
    ///
    /// For line/arrow containers, a white background is added to the label so
    /// the text is readable over the line stroke.
    fn add_bound_label(&mut self, container_id: ElementId, bounds: WBounds, text: String) {
        // Check if the container is a line/arrow — those need a label background
        // so text is legible over the stroke.
        let is_line_container = self.scene.get(container_id).is_some_and(|e| {
            matches!(e.kind, ElementKind::Arrow { .. } | ElementKind::Line { .. })
        });
        let mut el = self.new_text_element(WPoint::new(bounds.x, bounds.y), text);
        if let ElementKind::Text {
            wrap_width,
            container_id: cid,
            text_align,
            ..
        } = &mut el.kind
        {
            *wrap_width = Some(bounds.w.max(10.0));
            *cid = Some(container_id);
            *text_align = TextAlign::Center;
        }
        // Give line/arrow labels an opaque white background so the label text
        // is readable on top of the line stroke.
        if is_line_container {
            el.style.background = Some(0xffffff);
        }
        place_label(&mut el.bounds, bounds, TextAlign::Center);
        let id = self.scene.add(el);
        self.pending_measure.push(id);
    }

    /// Build a lightweight snapshot of all canvas elements for the AI agent's
    /// `list_elements` tool. Each entry carries the element's short id, kind
    /// label, optional text, and bounding box.
    pub fn element_snapshot(&self) -> Vec<crate::ai::tools::ElementSnapshot> {
        use crate::ai::tools::ElementSnapshot;
        self.scene
            .elements
            .iter()
            .map(|el| {
                let (kind, text) = match &el.kind {
                    ElementKind::Rectangle => ("rectangle", el.text().map(|t| t.to_string())),
                    ElementKind::Ellipse => ("ellipse", el.text().map(|t| t.to_string())),
                    ElementKind::Diamond => ("diamond", el.text().map(|t| t.to_string())),
                    ElementKind::Arrow { .. } => ("arrow", el.text().map(|t| t.to_string())),
                    ElementKind::Line { .. } => ("line", el.text().map(|t| t.to_string())),
                    ElementKind::Text { text, .. } => ("text", Some(text.clone())),
                    ElementKind::Freedraw { .. } => ("freedraw", None),
                    ElementKind::Polygon { .. } => ("polygon", None),
                };
                ElementSnapshot {
                    id: el.id.to_string()[..8].to_string(),
                    kind: kind.to_string(),
                    text,
                    x: el.bounds.x,
                    y: el.bounds.y,
                    w: el.bounds.w,
                    h: el.bounds.h,
                }
            })
            .collect()
    }

    /// Build the per-turn runtime-context snapshot the AI agent sees before it
    /// draws: the visible world region, the current canvas contents, and the
    /// style that omitted `style` fields inherit. Mirrors the harness pattern
    /// of injecting dynamic state as a fresh snapshot each turn (so the model
    /// never acts on stale state) instead of baking it into the static system
    /// prompt. The "supersedes earlier snapshots" header is added by the agent.
    pub fn runtime_context(&self) -> String {
        let mut body = String::new();

        // Visible world region so the agent draws on-screen instead of at the
        // hardcoded [0,1600]x[0,1000] the system prompt uses as a fallback.
        let origin = self.canvas_origin();
        if self.canvas_bounds.size.width.to_f64() > 0.0
            && self.canvas_bounds.size.height.to_f64() > 0.0
        {
            let br_screen = point(
                origin.x + self.canvas_bounds.size.width,
                origin.y + self.canvas_bounds.size.height,
            );
            let tl = self.camera.screen_to_world(origin, origin);
            let br = self.camera.screen_to_world(br_screen, origin);
            body.push_str(&format!(
                "可见区域（世界坐标）：x∈[{:.0}, {:.0}]，y∈[{:.0}, {:.0}]，缩放 {:.0}%\n",
                tl.x,
                br.x,
                tl.y,
                br.y,
                self.camera.zoom * 100.0
            ));
        }

        let snapshots = self.element_snapshot();
        if snapshots.is_empty() {
            body.push_str("画布为空，尚无任何元素。\n");
        } else {
            body.push_str(&format!("画布现有 {} 个元素：\n", snapshots.len()));
            for (i, e) in snapshots.iter().enumerate() {
                body.push_str(&format!("{}. {}\n", i + 1, e.summary()));
            }
        }

        body.push_str(&self.style_summary());
        body
    }

    /// One-line summary of the board's live style, for the runtime context.
    fn style_summary(&self) -> String {
        let s = &self.style;
        let fill = match s.background {
            Some(c) => format!("#{c:06x}"),
            None => "无填充".to_string(),
        };
        let dash = match s.stroke_style {
            StrokeStyle::Solid => "实线",
            StrokeStyle::Dashed => "虚线",
        };
        let opacity = if (s.opacity - 1.0).abs() < 0.001 {
            "不透明".to_string()
        } else {
            format!("透明度 {:.0}%", s.opacity * 100.0)
        };
        format!(
            "当前样式：描边 #{:06x}、线宽 {:.0}、粗糙度 {:.1}、{}、{}、{}",
            s.stroke, s.stroke_width, s.roughness, dash, fill, opacity
        )
    }

    /// World-space point at the center of the visible canvas. Kept for future
    /// auto-layout use; box/text ops currently carry absolute coordinates.
    #[allow(dead_code)]
    fn viewport_center_world(&self) -> WPoint {
        let center_screen = point(
            self.canvas_bounds.origin.x + self.canvas_bounds.size.width * 0.5,
            self.canvas_bounds.origin.y + self.canvas_bounds.size.height * 0.5,
        );
        self.camera
            .screen_to_world(center_screen, self.canvas_origin())
    }

    // ------------------------------------------------------------------
    // AI panel

    fn toggle_ai_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.ai_panel.take() {
            // Closing: undo the pan applied on open (plus any accumulated
            // during resizing). The net shift to reverse is exactly the
            // panel's current width / 2: open panned -w0/2, each resize panned
            // -Δ/2, so the total is -w_current/2.
            let w = panel.read(cx).width();
            self.camera.pan_by_screen(px(w / 2.0), px(0.0));
        } else {
            // Opening: the right-docked panel overlays the canvas, so the
            // visible canvas center jumps left by w/2. Pan the camera to
            // follow it, keeping whatever was centered still centered in the
            // (narrower) visible area - otherwise the focal content can end up
            // hidden behind the panel. Excalidraw does the same. Zoom is left
            // untouched (no need to fit + restore 100%).
            let weak = cx.weak_entity();
            let panel = cx.new(|cx| AiPanel::new(weak, window, cx));
            let w = panel.read(cx).width();
            self.ai_panel = Some(panel);
            self.camera.pan_by_screen(px(-w / 2.0), px(0.0));
        }
        cx.notify();
    }

    /// Current AI panel width (its live, user-resizable value), or the default
    /// if the panel is closed. Used to offset the toolbar / zoom bar so they
    /// never sit under the panel.
    fn ai_panel_width(&self, cx: &mut Context<Self>) -> f32 {
        self.ai_panel
            .as_ref()
            .map(|p| p.read(cx).width())
            .unwrap_or(crate::ai::panel::DEFAULT_WIDTH)
    }

    // ------------------------------------------------------------------
    // Auto-update
    //
    // Network/IO runs on the shared tokio runtime (crate::ai::client::
    // tokio_runtime); results/progress ferry back through futures channels to
    // a GPUI `cx.spawn` task that updates `update_state` + `cx.notify()`. This
    // mirrors the AI streaming pattern (src/ai/panel.rs).

    /// Check the manifest for a newer version. Sets `update_state` and, if an
    /// update exists, starts a silent download. No-op while a check/download is
    /// already in flight or an update is already ready. When `silent` (the
    /// automatic startup poll), an up-to-date or error result doesn't raise a
    /// banner - only a real available update surfaces.
    fn check_for_updates(&mut self, cx: &mut Context<Self>, silent: bool) {
        use crate::updater::UpdateState;
        if matches!(
            self.update_state,
            UpdateState::Checking
                | UpdateState::Downloading { .. }
                | UpdateState::Installing
                | UpdateState::Ready { .. }
        ) {
            return;
        }
        self.update_state = UpdateState::Checking;
        cx.notify();

        let (tx, rx) = futures::channel::oneshot::channel();
        crate::ai::client::tokio_runtime().spawn(async move {
            let res = async {
                let manifest = crate::updater::fetch_manifest().await?;
                let entry = manifest
                    .current_platform()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no update entry for platform {}",
                            crate::updater::platform_key()
                        )
                    })?
                    .clone();
                let newer =
                    crate::updater::is_newer(&manifest.version, crate::updater::current_version())?;
                Ok::<_, anyhow::Error>(if newer {
                    Some((manifest.version, manifest.notes, entry.url, entry.signature))
                } else {
                    None
                })
            }
            .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            let res = rx.await;
            this.update(cx, |this, cx| {
                use crate::updater::UpdateState;
                match res {
                    Ok(Ok(Some((version, notes, url, sig)))) => {
                        // Found a newer version - go straight into a silent
                        // download (no separate "available" banner state).
                        this.download_update(url, sig, version, notes, cx);
                    }
                    Ok(Ok(None)) => {
                        this.update_state = if silent {
                            UpdateState::Idle
                        } else {
                            UpdateState::UpToDate
                        };
                        cx.notify();
                    }
                    Ok(Err(e)) => {
                        this.update_state = if silent {
                            UpdateState::Idle
                        } else {
                            UpdateState::Error {
                                message: e.to_string(),
                            }
                        };
                        cx.notify();
                    }
                    Err(_) => {
                        // Sender dropped (task cancelled) - back to idle.
                        this.update_state = UpdateState::Idle;
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Download + minisign-verify the artifact, then leave it `Ready` for the
    /// user to restart. `version`/`notes` are carried through to the `Ready`
    /// state for the restart banner.
    fn download_update(
        &mut self,
        url: String,
        signature: String,
        version: String,
        notes: String,
        cx: &mut Context<Self>,
    ) {
        use crate::updater::UpdateState;
        let ext = if cfg!(target_os = "macos") {
            "app.tar.gz"
        } else {
            "zip"
        };
        let dest =
            std::env::temp_dir().join(format!("boundless-update-{}.{}", std::process::id(), ext));
        self.update_state = UpdateState::Downloading { fraction: 0.0 };
        cx.notify();

        let (tx, rx) = futures::channel::mpsc::unbounded::<DownloadMsg>();
        let url_c = url.clone();
        let dest_c = dest.clone();
        let sig_c = signature.clone();
        crate::ai::client::tokio_runtime().spawn(async move {
            let tx2 = tx.clone();
            let res = crate::updater::download(&url_c, &dest_c, move |done, total| {
                let frac = if total > 0 {
                    done as f64 / total as f64
                } else {
                    0.0
                };
                let _ = tx2.unbounded_send(DownloadMsg::Progress(frac));
            })
            .await;
            let res = res.and_then(|()| crate::updater::verify(&dest_c, &sig_c).map(|()| dest_c));
            let _ = tx.unbounded_send(DownloadMsg::Done(res.map_err(|e| e.to_string())));
        });

        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            let mut rx = rx;
            while let Some(msg) = rx.next().await {
                match msg {
                    DownloadMsg::Progress(frac) => {
                        this.update(cx, |this, cx| {
                            this.update_state = UpdateState::Downloading { fraction: frac };
                            cx.notify();
                        })
                        .ok();
                    }
                    DownloadMsg::Done(res) => {
                        this.update(cx, |this, cx| {
                            this.update_state = match res {
                                Ok(artifact) => UpdateState::Ready {
                                    version: version.clone(),
                                    notes: notes.clone(),
                                    artifact,
                                },
                                Err(message) => UpdateState::Error { message },
                            };
                            cx.notify();
                        })
                        .ok();
                        break;
                    }
                }
            }
        })
        .detach();
    }

    /// Apply the downloaded artifact and restart. Runs `apply` on a background
    /// thread (it does file IO then exits); on success the process exits, on
    /// failure the error is surfaced.
    fn install_and_restart(&mut self, cx: &mut Context<Self>) {
        use crate::updater::UpdateState;
        let artifact = match &self.update_state {
            UpdateState::Ready { artifact, .. } => artifact.clone(),
            _ => return,
        };
        self.update_state = UpdateState::Installing;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { crate::updater::apply(&artifact) })
                .await;
            // apply() exits on success, so we only reach here on error.
            if let Err(e) = res {
                this.update(cx, |this, cx| {
                    this.update_state = UpdateState::Error {
                        message: e.to_string(),
                    };
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Dismiss the current update state back to idle (e.g. user clicks "稍后").
    fn dismiss_update(&mut self, cx: &mut Context<Self>) {
        self.update_state = crate::updater::UpdateState::Idle;
        cx.notify();
    }

    /// True if `position` (window/content coordinates) lies over the AI panel.
    /// The panel docks against the right edge, so this is "right of the panel's
    /// left edge". Used to make the canvas's mouse/scroll handlers ignore events
    /// that the panel owns — without this, the canvas would steal focus on every
    /// click inside the panel (and would otherwise need stop_propagation on the
    /// panel root, which breaks gpui-component's text selection drag, since GPUI
    /// runs all mouse listeners — element handlers and TextView's selection
    /// listeners alike — through one loop that aborts on stop_propagation).
    fn over_ai_panel(
        &self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.ai_panel.is_none() {
            return false;
        }
        let panel_w = self.ai_panel_width(cx);
        let win_right = f32::from(window.viewport_size().width);
        let panel_left = win_right - panel_w;
        f32::from(position.x) >= panel_left
    }

    /// The cursor the canvas should show right now. Centralized so both the
    /// render path and the on-modifiers-changed handler agree on it (the latter
    /// forces an immediate cursor update on Windows; see platform::refresh_cursor).
    fn cursor_style(&self) -> CursorStyle {
        if self.editing.is_some() {
            // While editing text, always show the text caret cursor.
            return CursorStyle::IBeam;
        }
        match (&self.drag, self.tool) {
            // A temporary (Shift-drag) freehand stroke is in progress: show the
            // same crosshair the Pen tool uses.
            (DragState::Freedraw { .. }, _) if self.temp_pen => CursorStyle::Crosshair,
            // A temporary (Ctrl-drag) canvas pan is in progress: show a hand.
            (DragState::Panning { .. }, _) if self.temp_pan => CursorStyle::PointingHand,
            // Hover hint while idle: holding Shift will sketch (crosshair) and
            // holding Ctrl will pan (hand), so reflect the pending gesture.
            (DragState::Idle, _) if self.modifiers.shift && self.tool != ActiveTool::Pen => {
                CursorStyle::Crosshair
            }
            (DragState::Idle, _)
                if (self.modifiers.control || self.modifiers.platform)
                    && self.tool != ActiveTool::Hand =>
            {
                CursorStyle::PointingHand
            }
            // GPUI's Windows backend doesn't implement OpenHand (it falls
            // back to the plain arrow), so use PointingHand which maps to
            // IDC_HAND there. Panning is a "grabbing" gesture; using the
            // hand cursor is the best available hint on Windows.
            (DragState::Panning { .. }, _) => CursorStyle::PointingHand,
            (DragState::Moving { .. }, _) => CursorStyle::PointingHand,
            (DragState::EditingPoint { .. }, _) => CursorStyle::PointingHand,
            (DragState::Resizing { handle, .. }, _) => match handle {
                crate::render::Handle::N | crate::render::Handle::S => CursorStyle::ResizeUpDown,
                crate::render::Handle::E | crate::render::Handle::W => CursorStyle::ResizeLeftRight,
                // Diagonal handles: GPUI's Windows backend doesn't map
                // ResizeUpLeftDownRight/ResizeUpRightDownLeft (they fall back
                // to Arrow). Use PointingHand so at least the cursor changes
                // and hints the handle is interactive.
                _ => CursorStyle::PointingHand,
            },
            // Hand tool: OpenHand isn't implemented on Windows (see the
            // Panning comment above), so use PointingHand to get a hand cursor.
            (_, ActiveTool::Hand) => CursorStyle::PointingHand,
            (_, ActiveTool::Select) => {
                if self.hover_point.is_some() {
                    CursorStyle::PointingHand
                } else if let Some(h) = self.hover_handle {
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
            // Text tool: crosshair while sizing a box (editing shows IBeam via
            // the early branch above).
            (_, ActiveTool::Text) => CursorStyle::Crosshair,
            (_, ActiveTool::Eraser) => CursorStyle::PointingHand,
            _ => CursorStyle::Crosshair,
        }
    }

    /// The portion of the canvas that is actually visible. The canvas element
    /// fills the whole window, but the AI panel docks over its right edge, so
    /// when the panel is open the drawable region excludes the panel width.
    /// Zoom reset / fit / button-zoom should anchor on *this* rect (not the
    /// full window) so they behave relative to what the user can actually see.
    fn viewport_bounds(&self, cx: &mut Context<Self>) -> Bounds<Pixels> {
        let mut b = self.canvas_bounds;
        if self.ai_panel.is_some() {
            let panel_w = self.ai_panel_width(cx);
            b.size.width = (b.size.width - px(panel_w)).max(px(1.0));
        }
        b
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
        let file = SceneFile {
            show_grid: self.show_grid,
            background: self.canvas_background,
            ..file
        };
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
                self.show_grid = file.show_grid;
                self.canvas_background = file.background;
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

    /// Set the canvas surface color (None = default white board). Used by the
    /// zoom-bar swatches and the AI's `set_canvas_background` tool. Board
    /// state (like show_grid): persisted in the scene file, not in undo
    /// history.
    pub fn set_canvas_background(&mut self, color: Option<u32>, cx: &mut Context<Self>) {
        self.canvas_background = color;
        self.mark_dirty();
        cx.notify();
    }

    fn zoom_by(&mut self, factor: f64, cx: &mut Context<Self>) {
        let vp = self.viewport_bounds(cx);
        let center = point(
            vp.origin.x + vp.size.width * 0.5,
            vp.origin.y + vp.size.height * 0.5,
        );
        self.camera.zoom_at(factor, center, self.canvas_origin());
        cx.notify();
    }

    fn zoom_reset(&mut self, cx: &mut Context<Self>) {
        let vp = self.viewport_bounds(cx);
        let center = point(
            vp.origin.x + vp.size.width * 0.5,
            vp.origin.y + vp.size.height * 0.5,
        );
        let factor = 1.0 / self.camera.zoom;
        self.camera.zoom_at(factor, center, self.canvas_origin());
        cx.notify();
    }

    // ------------------------------------------------------------------
    // mouse handlers

    fn on_left_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Windows: the in-app menu bar spans the top MENU_BAR_HEIGHT px. Clicks
        // there belong to the menu bar (labels / drag spacer / caption buttons),
        // not the canvas, so don't start a canvas action. The caption buttons
        // stop propagation themselves, but the drag spacer can't - HTCAPTION
        // needs the NC mouse-down to reach DefWindowProcW to move the window -
        // so guard here instead.
        if cfg!(target_os = "windows") && event.position.y < px(MENU_BAR_HEIGHT) {
            return;
        }
        // A left click closes any open context menu (the backdrop normally
        // intercepts this, but guard anyway for clicks reaching the canvas).
        if self.context_menu.is_some() {
            self.context_menu = None;
            cx.notify();
        }
        // Ignore clicks that land on the AI panel - the panel owns those (its
        // input field, selectable text, buttons). Returning before the focus
        // grab below keeps the panel's widgets focused and interactive.
        if self.over_ai_panel(event.position, window, cx) {
            return;
        }
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
            // A bound label selects its container instead — labels aren't
            // independently selectable.
            if let Some(new_id) = self.commit_editing(window, cx) {
                self.tool = ActiveTool::Select;
                let target = self
                    .scene
                    .get(new_id)
                    .and_then(|e| e.container_id())
                    .unwrap_or(new_id);
                self.selection = vec![target];
                cx.notify();
                return;
            }
        }

        // Modifier + left-drag gestures work from ANY tool (except while editing
        // text, where the keystrokes belong to the input field). The active tool
        // is never changed, so releasing the mouse returns to it.
        let ctrl = event.modifiers.control || event.modifiers.platform;
        let shift = event.modifiers.shift;
        if self.editing.is_none() {
            if shift && self.tool != ActiveTool::Pen {
                // Shift + left-drag: temporary freehand stroke (Excalidraw's
                // "hold to sketch"). Reuses the Pen tool's drag path.
                self.temp_pen = true;
                self.drag = self.begin_freedraw(world);
                cx.notify();
                return;
            } else if ctrl && self.tool != ActiveTool::Hand {
                // Ctrl + left-drag: temporarily pan the canvas (like the Hand
                // tool) without leaving the current tool.
                self.temp_pan = true;
                self.drag = DragState::Panning {
                    last_screen: event.position,
                };
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
                self.drag = DragState::Drawing {
                    start: world,
                    seed: crate::scene::new_seed(),
                };
            }
            ActiveTool::Arrow | ActiveTool::Line => {
                self.drag = DragState::Drawing {
                    start: world,
                    seed: crate::scene::new_seed(),
                };
            }
            ActiveTool::Pen => {
                // Stylus eraser tip: Windows Ink routes the flipped pen
                // through the pen tool with the eraser flag set — erase
                // instead of drawing.
                if crate::platform::latest_pen_sample().is_some_and(|s| s.eraser) {
                    self.begin_erase(world, cx);
                } else {
                    self.drag = self.begin_freedraw(world);
                }
            }
            ActiveTool::Text => {
                let hit = self.scene.hit_test(world, self.hit_tolerance());
                let is_text = hit.is_some_and(|id| self.scene.get(id).is_some_and(|e| e.is_text()));
                let is_container =
                    hit.is_some_and(|id| self.scene.get(id).is_some_and(|e| e.is_container()));
                if let Some(id) = hit.filter(|_| is_text) {
                    self.start_editing(id, false, cx);
                } else if let Some(id) = hit.filter(|_| is_container) {
                    // Text tool on a shape edits its label (Excalidraw).
                    self.edit_container_label(id, cx);
                } else {
                    // Drag to size the text box first; release to edit.
                    self.drag = DragState::Drawing {
                        start: world,
                        seed: crate::scene::new_seed(),
                    };
                }
            }
            ActiveTool::Eraser => {
                self.begin_erase(world, cx);
            }
        }
        cx.notify();
    }

    /// Start an erase drag at `world`: removes the hit element (if any) and
    /// enters the continuous-erase drag state. Shared by the Eraser tool and
    /// the stylus-eraser tip (Windows Ink routes the flipped pen through the
    /// Pen tool with the eraser flag set).
    fn begin_erase(&mut self, world: WPoint, cx: &mut Context<Self>) {
        let mut removed = false;
        if let Some(id) = self.scene.hit_test(world, self.hit_tolerance()) {
            self.history.record(&self.scene);
            self.remove_element(id);
            self.selection.retain(|s| *s != id);
            removed = true;
            self.mark_dirty();
        }
        self.drag = DragState::Erasing {
            removed_any: removed,
        };
        cx.notify();
    }

    fn select_down(&mut self, event: &MouseDownEvent, world: WPoint, cx: &mut Context<Self>) {
        // 0. vertex/midpoint control handles of a single selected line/arrow
        // (screen-space hit test, ahead of the bbox resize handles they may
        // overlap).
        if let Some((id, handles)) = self.selected_line_point_handles() {
            for (target, rect) in handles {
                if rect.contains(&event.position) {
                    let original = self.scene.get(id).cloned();
                    if let Some(original) = original {
                        self.drag = DragState::EditingPoint {
                            element: id,
                            original,
                            target,
                            recorded: false,
                        };
                        cx.notify();
                        return;
                    }
                }
            }
        }

        // 1. resize handles (screen-space hit test).
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

        // 1b. Drag-to-move the existing selection from anywhere inside its
        // bbox (not just on the stroke). This matters for point elements
        // (lines/arrows) whose hit_test only matches the thin stroke: a
        // click inside the bbox but off the line would otherwise start a
        // marquee and clear the selection. Excalidraw also lets you grab
        // anywhere inside the selection box to move it.
        if !self.selection.is_empty()
            && !event.modifiers.shift
            && self.selection_bounds_world().is_some_and(|b| {
                b.inflate(self.hit_tolerance(), self.hit_tolerance())
                    .contains(world)
            })
        {
            self.drag = DragState::Moving {
                last_world: world,
                recorded: false,
            };
            cx.notify();
            return;
        }

        // 2. element hit test.
        let hit = self.scene.hit_test(world, self.hit_tolerance());
        // Remember when the raw hit is a bound label: a single click on the
        // label of an *already-selected* container edits it (Excalidraw).
        let label_hit = hit.and_then(|id| {
            let e = self.scene.get(id)?;
            let cid = e.container_id()?;
            Some((id, cid))
        });
        // Clicks on a bound label act on its container: labels aren't
        // independently selectable with the Select tool (Excalidraw);
        // double-clicking the container edits the label.
        let hit = hit.map(|id| {
            self.scene
                .get(id)
                .and_then(|e| e.container_id())
                .unwrap_or(id)
        });
        if let Some(id) = hit {
            if event.click_count == 1 && !event.modifiers.shift {
                if let Some((label_id, cid)) = label_hit {
                    if cid == id && self.selection.contains(&cid) {
                        self.selection = vec![label_id];
                        self.start_editing(label_id, false, cx);
                        return;
                    }
                }
            }
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
            // Double-click on a text element starts editing; on a shape it
            // edits the shape's bound label (creating one if needed).
            if event.click_count == 2 && !event.modifiers.shift {
                if self.scene.get(id).is_some_and(|e| e.is_text()) {
                    self.start_editing(id, false, cx);
                    return;
                }
                if self.scene.get(id).is_some_and(|e| e.is_container()) {
                    self.edit_container_label(id, cx);
                    return;
                }
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

    /// Right-click: open a context menu (layer ops / delete / delete-vertex).
    /// Right-clicking an element selects it first (Excalidraw behavior);
    /// right-clicking a vertex handle of a selected line/arrow adds a
    /// "delete vertex" item. Right-clicking empty canvas with no selection
    /// opens nothing.
    fn on_right_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.over_ai_panel(event.position, window, cx) {
            return;
        }
        let world = self.to_world(event.position);

        // Vertex handle hit on a single selected line/arrow: open the menu
        // with a "delete vertex" item, without changing the selection.
        if let Some((id, handles)) = self.selected_line_point_handles() {
            for (target, rect) in &handles {
                if let PointTarget::Vertex(index) = target {
                    if rect.contains(&event.position) {
                        let can_delete = self
                            .scene
                            .get(id)
                            .map(|el| el.absolute_points().len() > 2)
                            .unwrap_or(false);
                        if can_delete {
                            self.context_menu = Some(ContextMenuState {
                                position: event.position,
                                vertex: Some(*index),
                            });
                            cx.notify();
                        }
                        return;
                    }
                }
            }
        }

        // Otherwise: right-click selects the element under the cursor (if any
        // and not already selected, no shift), then opens the menu if there
        // is a selection.
        if let Some(id) = self.scene.hit_test(world, self.hit_tolerance()) {
            // Resolve bound labels to their container (labels aren't
            // independently selectable).
            let id = self
                .scene
                .get(id)
                .and_then(|e| e.container_id())
                .unwrap_or(id);
            if !event.modifiers.shift && !self.selection.contains(&id) {
                self.selection = vec![id];
            }
        }
        if !self.selection.is_empty() {
            self.context_menu = Some(ContextMenuState {
                position: event.position,
                vertex: None,
            });
            cx.notify();
        }
    }

    fn on_middle_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.over_ai_panel(event.position, window, cx) {
            return;
        }
        self.drag = DragState::Panning {
            last_screen: event.position,
        };
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Ignore moves over the AI panel — the panel owns hover/cursor/selection
        // there, and the canvas must not fight it (e.g. update the hover handle
        // or keep an in-progress drag updating while the pointer is over text).
        if self.over_ai_panel(event.position, window, cx) {
            return;
        }
        // Track modifier state so render() can hint the pending gesture cursor
        // (Ctrl => pan hand, Shift => pen crosshair) while hovering, before any
        // button is pressed.
        let modifiers_changed = self.modifiers != event.modifiers;
        self.modifiers = event.modifiers;
        let world = self.to_world(event.position);
        // Take the drag state out to avoid borrowing conflicts while
        // mutating the scene/history.
        let drag = std::mem::take(&mut self.drag);
        match drag {
            DragState::Idle => {
                // Hover detection: which element/handle is under the cursor?
                // Used by render() to pick a move/resize cursor that hints the
                // available action. Control-point handles of a single selected
                // line/arrow win over the bbox resize handles where they
                // overlap.
                let new_point = if self.tool == ActiveTool::Select {
                    self.selected_line_point_handles().and_then(|(_, handles)| {
                        handles
                            .into_iter()
                            .find(|(_, rect)| rect.contains(&event.position))
                            .map(|(t, _)| t)
                    })
                } else {
                    None
                };
                let new_handle = if new_point.is_none()
                    && !self.selection.is_empty()
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
                    && new_point.is_none()
                    && self.tool == ActiveTool::Select
                    && (self
                        .scene
                        .hit_test(world, self.hit_tolerance())
                        .is_some()
                        // Also treat the inside of the selection bbox as
                        // "over an element" so the move cursor shows even
                        // where a thin line's hit_test misses (the bbox is
                        // the draggable area per select_down's 1b step).
                        || (!self.selection.is_empty()
                            && self
                                .selection_bounds_world()
                                .is_some_and(|b| {
                                    b.inflate(
                                        self.hit_tolerance(),
                                        self.hit_tolerance(),
                                    )
                                    .contains(world)
                                })));
                if new_over_element != self.hover_over_element
                    || new_handle != self.hover_handle
                    || new_point != self.hover_point
                    || modifiers_changed
                {
                    self.hover_over_element = new_over_element;
                    self.hover_handle = new_handle;
                    self.hover_point = new_point;
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
            DragState::Drawing { start, seed } => {
                self.update_draft(start, world, event.modifiers.shift, seed);
                self.drag = DragState::Drawing { start, seed };
                cx.notify();
            }
            DragState::Freedraw {
                mut collector,
                seed,
            } => {
                // The collector applies decimation (2-screen-px rule), EMA
                // smoothing, and pressure (hardware stylus pressure from the
                // WM_POINTER hook when a pen is live, velocity-simulated
                // otherwise); notify only when a sample was actually captured.
                let hw = self.hw_pressure();
                if collector.push_with_pressure(world, hw) {
                    cx.notify();
                }
                self.drag = DragState::Freedraw { collector, seed };
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
                    // Bound labels follow their containers (they are not
                    // part of the selection themselves).
                    let sel = self.selection.clone();
                    for el in &mut self.scene.elements {
                        if let Some(cid) = el.container_id() {
                            if sel.contains(&cid) && !sel.contains(&el.id) {
                                el.translate(dx, dy);
                            }
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
            DragState::EditingPoint {
                element,
                original,
                target,
                mut recorded,
            } => {
                // Recompute from the stable original on every move: for a
                // midpoint drag this re-inserts the new vertex at the same
                // segment each time, so the drag is idempotent.
                let mut edited = original.clone();
                match target {
                    PointTarget::Vertex(i) => edited.set_absolute_point(i, world),
                    PointTarget::Midpoint(seg) => edited.insert_absolute_point_after(seg, world),
                }
                // Gate on an actual change so a click without movement neither
                // inserts a vertex nor pollutes the undo history.
                if edited != original {
                    if !recorded {
                        self.history.record(&self.scene);
                        recorded = true;
                        self.mark_dirty();
                    }
                    if let Some(el) = self.scene.get_mut(element) {
                        *el = edited;
                    }
                    // Bound labels re-wrap/re-center on the (possibly new)
                    // container bounds.
                    self.update_container_labels(&[element]);
                    cx.notify();
                }
                self.drag = DragState::EditingPoint {
                    element,
                    original,
                    target,
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
                // The rescale pivot is the dragged handle's opposite corner
                // (the anchor kept fixed by resize_bounds). It must NOT be
                // hardcoded to the top-left: e.g. dragging the NE handle
                // anchors the SW corner, so scaling must pivot about SW.
                // Matches the anchor computation in Handle::resize_bounds.
                let (fx, fy) = handle.fraction();
                let pivot = WPoint::new(
                    original_bounds.x + original_bounds.w * (1.0 - fx),
                    original_bounds.y + original_bounds.h * (1.0 - fy),
                );
                let is_corner = matches!(
                    handle,
                    crate::render::Handle::Nw
                        | crate::render::Handle::Ne
                        | crate::render::Handle::Se
                        | crate::render::Handle::Sw
                );
                let is_horizontal_edge =
                    matches!(handle, crate::render::Handle::E | crate::render::Handle::W);
                for original in &originals {
                    let mut scaled = original.clone();
                    if let ElementKind::Text {
                        font_size,
                        wrap_width,
                        min_height,
                        ..
                    } = &mut scaled.kind
                    {
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
                // Bound labels of resized containers re-wrap to the new
                // container width; pending_measure then re-measures and
                // re-centers them.
                let resized_ids: Vec<ElementId> = originals.iter().map(|o| o.id).collect();
                let container_bounds: Vec<(ElementId, WBounds)> = resized_ids
                    .iter()
                    .filter_map(|id| self.scene.get(*id).map(|c| (*id, c.bounds)))
                    .collect();
                for el in &mut self.scene.elements {
                    if resized_ids.contains(&el.id) {
                        continue;
                    }
                    let Some(cid) = el.container_id() else {
                        continue;
                    };
                    let Some((_, cb)) = container_bounds.iter().find(|(id, _)| *id == cid) else {
                        continue;
                    };
                    if let ElementKind::Text { wrap_width, .. } = &mut el.kind {
                        *wrap_width = Some(cb.w.max(10.0));
                        self.pending_measure.push(el.id);
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
                    self.remove_element(id);
                    self.selection.retain(|s| *s != id);
                    self.mark_dirty();
                    cx.notify();
                }
                self.drag = DragState::Erasing { removed_any };
            }
        }
    }

    /// Update the in-progress draft shape. The `seed` comes from the drag
    /// state and stays constant for the whole drag, so the rough jitter of
    /// the draft doesn't re-randomize every frame.
    fn update_draft(&mut self, start: WPoint, current: WPoint, constrain: bool, seed: u64) {
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
                WPoint::new(start.x + side * dx.signum(), start.y + side * dy.signum())
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
            // The font size is fixed (style-bar default), so the box height
            // is exactly one line — the drag only sets the wrap width.
            ActiveTool::Text => {
                let mut style = self.style.clone();
                style.stroke_style = StrokeStyle::Dashed;
                style.roughness = 0.0;
                let one_line = WBounds::new(
                    bounds.x,
                    bounds.y,
                    bounds.w,
                    self.text_font_size * LINE_HEIGHT,
                );
                let mut draft = Element::new(ElementKind::Rectangle, one_line, style);
                draft.seed = seed;
                self.draft = Some(draft);
                return;
            }
            _ => None,
        };
        if let Some(kind) = kind {
            let mut draft = Element::new(kind, bounds, self.style.clone());
            draft.seed = seed;
            self.draft = Some(draft);
        }
    }

    fn on_left_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        // A drag that ends over the AI panel is abandoned (the release point
        // maps to an occluded canvas region). Reset the drag state so the
        // canvas doesn't stay stuck mid-draw, but don't commit anything.
        if self.over_ai_panel(event.position, window, cx) {
            self.drag = DragState::Idle;
            cx.notify();
            return;
        }
        let world = self.to_world(event.position);
        match std::mem::take(&mut self.drag) {
            DragState::Drawing { start, seed } => {
                // Click without drag creates a default-sized shape; a drag
                // commits the draft. The Text tool skips this default-draft
                // creation — a plain click makes a natural-width text box.
                if self.tool != ActiveTool::Text && self.draft.is_none() {
                    let size = 120.0;
                    let end = WPoint::new(start.x + size, start.y + size * 0.75);
                    self.update_draft(start, end, false, seed);
                }
                if self.tool == ActiveTool::Text {
                    // Decide drag vs click by the actual release position, not
                    // the draft bounds (which may be absent for a click).
                    let dragged = start.distance(world) > 8.0;
                    self.history.record(&self.scene);
                    let el = if dragged {
                        // The drag fixes only the wrap width. The font size
                        // comes from the style-bar default; the box height
                        // starts at one line and grows with the content.
                        let b = self
                            .draft
                            .as_ref()
                            .map(|d| d.bounds)
                            .unwrap_or_else(|| WBounds::from_corners(start, world));
                        let mut el = self.new_text_element(WPoint::new(b.x, b.y), String::new());
                        if let ElementKind::Text { wrap_width, .. } = &mut el.kind {
                            *wrap_width = Some(b.w.max(self.text_font_size * 2.0).max(20.0));
                        }
                        el
                    } else {
                        // Plain click: natural-width text box (no wrapping).
                        self.new_text_element(start, String::new())
                    };
                    self.draft = None; // discard the dashed preview
                    let id = self.scene.add(el);
                    self.pending_measure.push(id);
                    self.mark_dirty();
                    self.selection = vec![id];
                    self.start_editing(id, true, cx);
                } else if let Some(mut el) = self.draft.take() {
                    if matches!(
                        el.kind,
                        ElementKind::Line { .. } | ElementKind::Arrow { .. }
                    ) && world.distance(start) < 1e-6
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
            DragState::Freedraw { collector, seed } => {
                // A single sample (tap without movement) commits as a round
                // ink dot — Excalidraw behavior, see dot_geometry.
                if collector.len() >= 1 {
                    self.history.record(&self.scene);
                    let stroke = collector.finish();
                    let mut el = Element::from_absolute_points(
                        |points| ElementKind::Freedraw {
                            points,
                            widths: stroke.widths,
                        },
                        stroke.points,
                        self.style.clone(),
                    );
                    // Reuse the drag seed so the committed stroke looks
                    // exactly like the draft shown while drawing.
                    el.seed = seed;
                    self.scene.add(el);
                    self.mark_dirty();
                    // Keep the pen active and don't select the stroke
                    // (Excalidraw behavior): the user can continue writing
                    // the next stroke immediately.
                }
            }
            _ => {}
        }
        // End any temporary modifier-drag gesture (Shift=sketch, Ctrl=pan): the
        // current tool was never changed, so clearing the flags returns the
        // editor to it.
        self.temp_pen = false;
        self.temp_pan = false;
        cx.notify();
    }

    fn on_middle_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.over_ai_panel(event.position, window, cx) {
            // Reset any in-progress pan so the canvas doesn't stay stuck.
            if matches!(self.drag, DragState::Panning { .. }) {
                self.drag = DragState::Idle;
                cx.notify();
            }
            return;
        }
        if matches!(self.drag, DragState::Panning { .. }) {
            self.drag = DragState::Idle;
            cx.notify();
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Let the AI panel's messages area handle its own scroll; don't zoom/pan
        // the canvas when the wheel turns over the panel.
        if self.over_ai_panel(event.position, window, cx) {
            return;
        }
        let delta = event.delta.pixel_delta(px(20.0));
        if event.modifiers.control || event.modifiers.platform {
            let factor = (-delta.y.to_f64() * 0.002).exp();
            self.camera
                .zoom_at(factor, event.position, self.canvas_origin());
        } else if event.modifiers.shift {
            // Horizontal pan. GPUI's Windows backend already transposes
            // shift+vertical-wheel into the X axis (delta.x set, delta.y=0),
            // so pan by delta.x. (delta.y is kept as a fallback for platforms
            // that don't transpose, e.g. some trackpads.)
            let dx = if delta.x.to_f64() != 0.0 {
                delta.x
            } else {
                delta.y
            };
            self.camera.pan_by_screen(dx, px(0.0));
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

    /// Fires on every modifier change (key down AND key up), so the hover cursor
    /// and toolbar highlight can react instantly to pressing/releasing Ctrl or
    /// Shift — without this, modifier state only refreshes on the next mouse
    /// move, which feels sluggish.
    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.modifiers = event.modifiers;
        cx.notify();
        // On Windows the system cursor only refreshes on WM_SETCURSOR (mouse
        // move), so pressing/releasing Ctrl/Shift wouldn't change the visible
        // cursor until the mouse next moves. Apply it directly here, in sync
        // with the highlight that the repaint already draws — both driven by
        // the same cursor_style() so they can't drift apart.
        crate::platform::refresh_cursor(window, self.cursor_style());
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
            size: size(self.camera.scale(b.w), self.camera.scale(b.h)),
        }
    }

    /// Screen-space polyline points of the single selected line/arrow plus
    /// its curved-line-type flag. None unless exactly one line/arrow is
    /// selected and no text edit is in progress.
    fn selected_line_screen_points(&self) -> Option<(ElementId, Vec<Point<Pixels>>, bool)> {
        if self.selection.len() != 1 || self.editing.is_some() {
            return None;
        }
        let el = self.scene.get(self.selection[0])?;
        if !matches!(
            el.kind,
            ElementKind::Line { .. } | ElementKind::Arrow { .. }
        ) {
            return None;
        }
        let origin = self.canvas_origin();
        Some((
            el.id,
            el.absolute_points()
                .iter()
                .map(|p| self.camera.world_to_screen(*p, origin))
                .collect(),
            el.style.line_type == LineType::Curved,
        ))
    }

    /// Screen-space control-point handle rects (vertices + segment midpoints)
    /// of the single selected line/arrow, for hit-testing and rendering.
    fn selected_line_point_handles(
        &self,
    ) -> Option<(ElementId, Vec<(PointTarget, Bounds<Pixels>)>)> {
        let (id, pts, curved) = self.selected_line_screen_points()?;
        Some((id, point_handle_rects(&pts, curved)))
    }

    /// Re-wrap and re-place the bound labels of the given containers after
    /// their bounds changed (vertex edits). Mirrors the tail of the Resizing
    /// drag arm; the render measure pass re-centers each label.
    fn update_container_labels(&mut self, container_ids: &[ElementId]) {
        let bounds: Vec<(ElementId, WBounds)> = container_ids
            .iter()
            .filter_map(|id| self.scene.get(*id).map(|c| (*id, c.bounds)))
            .collect();
        for el in &mut self.scene.elements {
            if container_ids.contains(&el.id) {
                continue;
            }
            let Some(cid) = el.container_id() else {
                continue;
            };
            let Some((_, cb)) = bounds.iter().find(|(id, _)| *id == cid) else {
                continue;
            };
            if let ElementKind::Text { wrap_width, .. } = &mut el.kind {
                *wrap_width = Some(cb.w.max(10.0));
                self.pending_measure.push(el.id);
            }
        }
    }

    /// Screen-space origin of the text being edited. A bound label is
    /// centered in its container using the *live* content size, so the text
    /// stays centered while typing. Pass `content` (width, height in screen
    /// px) when the caller already computed it to avoid re-shaping.
    fn editing_origin(
        &self,
        el: &Element,
        ed: &EditingState,
        content: Option<(Pixels, Pixels)>,
        window: &Window,
    ) -> Point<Pixels> {
        let base = |this: &Self| {
            this.camera
                .world_to_screen(WPoint::new(el.bounds.x, el.bounds.y), this.canvas_origin())
        };
        let Some(cid) = ed.container_id else {
            return base(self);
        };
        let Some(cb) = self.scene.get(cid).map(|c| c.bounds) else {
            return base(self);
        };
        let (w, h) = match content {
            Some(v) => v,
            None => {
                let text = ed.session.text();
                let color = color_u32(0x000000, 1.0);
                let shaped = self.text_cache.shaped(
                    &text,
                    ed.font_size,
                    &ed.font_family,
                    ed.wrap_width,
                    color,
                    &self.camera,
                    window,
                );
                let w = shaped
                    .lines
                    .iter()
                    .map(|l| l.width)
                    .fold(px(0.0), |a, b| a.max(b));
                (w, shaped.line_height * shaped.lines.len() as f32)
            }
        };
        let tl = self
            .camera
            .world_to_screen(WPoint::new(cb.x, cb.y), self.canvas_origin());
        let center = self.camera.world_to_screen(
            WPoint::new(cb.x + cb.w * 0.5, cb.y + cb.h * 0.5),
            self.canvas_origin(),
        );
        let x = match ed.text_align {
            TextAlign::Left => tl.x,
            TextAlign::Center => center.x - w / 2.0,
            TextAlign::Right => tl.x + self.camera.scale(cb.w) - w,
        };
        point(x, center.y - h / 2.0)
    }

    /// Box width used for line alignment while editing: the wrap width for
    /// standalone wrapped text; the content width otherwise (bound labels).
    fn editing_box_width(&self, ed: &EditingState, lines: &[ShapedTextLine]) -> Pixels {
        let content_w = lines.iter().map(|l| l.width).fold(px(0.0), |a, b| a.max(b));
        match (ed.wrap_width, ed.container_id) {
            (Some(ww), None) => self.camera.scale(ww).max(content_w),
            _ => content_w,
        }
    }

    /// Map a screen point to a char offset in the currently-edited text.
    fn char_index_for_screen(&self, screen: Point<Pixels>, window: &Window) -> Option<usize> {
        let ed = self.editing.as_ref()?;
        let el = self.scene.get(ed.element_id)?;
        let text = ed.session.text();
        let color = color_u32(0x000000, 1.0);
        let shaped = self.text_cache.shaped(
            &text,
            ed.font_size,
            &ed.font_family,
            ed.wrap_width,
            color,
            &self.camera,
            window,
        );
        if shaped.lines.is_empty() {
            return Some(0);
        }
        let lines = shaped.lines;
        let line_height = shaped.line_height;
        let origin = self.editing_origin(el, ed, None, window);
        let offsets = line_offsets(&lines, self.editing_box_width(ed, &lines), ed.text_align);
        let rel_y = (screen.y - origin.y).to_f64();
        let line_idx = ((rel_y / line_height.to_f64()).floor() as isize)
            .clamp(0, lines.len() as isize - 1) as usize;
        let rel_x = screen.x - (origin.x + offsets[line_idx]);
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
        let shaped = self.text_cache.shaped(
            &text,
            ed.font_size,
            &ed.font_family,
            ed.wrap_width,
            color,
            &self.camera,
            window,
        );
        let lines = shaped.lines;
        let line_height = shaped.line_height;
        if lines.is_empty() {
            return None;
        }
        let byte = ed.session.rope.char_to_byte(char_off);
        let origin = self.editing_origin(el, ed, None, window);
        let offsets = line_offsets(&lines, self.editing_box_width(ed, &lines), ed.text_align);
        for (i, line) in lines.iter().enumerate() {
            let in_line = byte >= line.byte_range.start
                && (byte <= line.byte_range.end || i == lines.len() - 1);
            if in_line {
                let x = line
                    .line
                    .x_for_index((byte - line.byte_range.start).min(line.byte_range.len()));
                return Some((
                    point(origin.x + offsets[i] + x, origin.y + line_height * i as f32),
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

/// Position a bound label on its container (keeping its size): horizontal
/// placement follows `align`, vertical placement is always centered.
fn place_label(b: &mut WBounds, container: WBounds, align: TextAlign) {
    b.x = match align {
        TextAlign::Left => container.x,
        TextAlign::Center => container.x + (container.w - b.w) * 0.5,
        TextAlign::Right => container.x + container.w - b.w,
    };
    b.y = container.y + (container.h - b.h) * 0.5;
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

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let ed = self.editing.as_ref()?;
        let marked = ed.session.marked.clone()?;
        let text = ed.session.text();
        let start = utf8_to_utf16(
            &text,
            crate::text::char_to_byte(&ed.session.rope, marked.start),
        );
        let end = utf8_to_utf16(
            &text,
            crate::text::char_to_byte(&ed.session.rope, marked.end),
        );
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
    lines: std::sync::Arc<Vec<ShapedTextLine>>,
    /// Per-line horizontal offset within the box (text alignment).
    line_offsets: Vec<Pixels>,
    origin: Point<Pixels>,
    line_height: Pixels,
    bounds: Bounds<Pixels>,
    background: Option<Hsla>,
}

/// Per-line x offset of each shaped line inside a box of `box_width`,
/// following the text alignment.
fn line_offsets(lines: &[ShapedTextLine], box_width: Pixels, align: TextAlign) -> Vec<Pixels> {
    lines
        .iter()
        .map(|l| {
            let slack = (box_width - l.width).max(px(0.0));
            match align {
                TextAlign::Left => px(0.0),
                TextAlign::Center => slack / 2.0,
                TextAlign::Right => slack,
            }
        })
        .collect()
}

struct EditingPaint {
    item: TextPaintItem,
    text_bounds: Bounds<Pixels>,
    /// Visible text-box frame drawn around the text being edited; None for
    /// bound labels (the container outline is the frame).
    frame_quad: Option<PaintQuad>,
    selection_quads: Vec<PaintQuad>,
    marked_quads: Vec<PaintQuad>,
    caret_quad: Option<PaintQuad>,
}

struct BoardPaint {
    /// Canvas surface color (blackboard theme); painted before everything.
    background: Option<PaintQuad>,
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

        // Board surface: a custom background color (e.g. the dark green of a
        // blackboard theme) covers the whole viewport beneath grid/content.
        // The dot grid keeps its fixed color — on dark surfaces it reads as
        // faint chalk dust.
        let background = self
            .canvas_background
            .map(|c| gpui::fill(viewport, color_u32(c, 1.0)));

        let grid = if self.show_grid {
            dot_grid(&self.camera, viewport, color_u32(GRID_COLOR, 1.0))
        } else {
            Vec::new()
        };

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
                ElementKind::Text {
                    text, font_size, ..
                } => {
                    let color = color_u32(el.style.stroke, el.style.opacity);
                    let bg = el.style.background.map(|c| color_u32(c, el.style.opacity));
                    let shaped = self.text_cache.shaped(
                        text,
                        *font_size,
                        el.font_family(),
                        el.wrap_width(),
                        color,
                        &self.camera,
                        window,
                    );
                    let screen_origin = self
                        .camera
                        .world_to_screen(WPoint::new(el.bounds.x, el.bounds.y), origin);
                    let screen_bounds = Bounds {
                        origin: screen_origin,
                        size: size(
                            self.camera.scale(el.bounds.w.max(1.0)),
                            self.camera.scale(el.bounds.h.max(1.0)),
                        ),
                    };
                    let line_offsets =
                        line_offsets(&shaped.lines, screen_bounds.size.width, el.text_align());
                    texts.push(TextPaintItem {
                        lines: shaped.lines,
                        line_offsets,
                        origin: screen_origin,
                        line_height: shaped.line_height,
                        bounds: screen_bounds,
                        background: bg,
                    });
                }
                _ => {
                    let geom = self.render_cache.geometry(el);
                    paths.extend(crate::render::rough::paint_world_geom(
                        &geom,
                        el,
                        &self.camera,
                        origin,
                    ));
                }
            }
        }

        // In-progress shapes.
        if let Some(draft) = &self.draft {
            paths.extend(paths_for_element(draft, &self.camera, origin));
        }
        if let DragState::Freedraw { collector, seed } = &self.drag {
            // A tap shows its dot immediately on pen-down.
            if collector.len() >= 1 {
                let widths = collector.widths().to_vec();
                let mut draft = Element::from_absolute_points(
                    |points| ElementKind::Freedraw { points, widths },
                    collector.points().to_vec(),
                    self.style.clone(),
                );
                // Stable seed for the whole drag: without it the rough
                // jitter re-randomizes every frame and the line "swims".
                draft.seed = *seed;
                paths.extend(paths_for_element(&draft, &self.camera, origin));
            }
        }

        // Selection overlay. Hidden while text editing: the edited element
        // is in the selection, and the editing frame already highlights it —
        // drawing both would show two overlapping boxes.
        let mut selection_outline = None;
        let mut handle_quads = Vec::new();
        if !self.selection.is_empty()
            && matches!(self.tool, ActiveTool::Select)
            && self.editing.is_none()
        {
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
                    size: size(
                        screen.size.width + pad * 2.0,
                        screen.size.height + pad * 2.0,
                    ),
                };
                let sel = color_u32(SELECTION_COLOR, 1.0);
                // While a control point is being dragged, hide both the bbox
                // outline and the resize handles: they overlap the point
                // handles and the stale bbox would be visual noise mid-drag
                // (the points themselves are the frame of reference then).
                let dragging_point = matches!(self.drag, DragState::EditingPoint { .. });
                if !dragging_point {
                    selection_outline = Some(outline(screen, sel, BorderStyle::Solid));
                }
                if !dragging_point {
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
                // Vertex + midpoint control handles of a single selected
                // line/arrow. Painted after the resize handles so they sit on
                // top where they overlap; iterated in reverse so vertices
                // (first in the vec) are painted last, above midpoints.
                if let Some((_, handles)) = self.selected_line_point_handles() {
                    // The handle to highlight: the dragged one (a midpoint
                    // drag highlights the vertex it inserted at seg+1), else
                    // the hovered one.
                    let active = match &self.drag {
                        DragState::EditingPoint { target, .. } => Some(match target {
                            PointTarget::Midpoint(seg) => PointTarget::Vertex(seg + 1),
                            t => *t,
                        }),
                        _ => self.hover_point,
                    };
                    let white = color_u32(0xffffff, 1.0);
                    for (target, rect) in handles.into_iter().rev() {
                        let is_active = active == Some(target);
                        let q = match target {
                            PointTarget::Vertex(_) => quad(
                                rect,
                                px(2.0),
                                if is_active { sel } else { white },
                                px(1.0),
                                sel,
                                BorderStyle::Solid,
                            ),
                            // Midpoints: smaller, translucent blue; solid
                            // while hovered/dragged.
                            PointTarget::Midpoint(_) => quad(
                                rect,
                                px(3.0),
                                if is_active {
                                    sel
                                } else {
                                    color_u32(SELECTION_COLOR, 0.45)
                                },
                                px(1.0),
                                sel,
                                BorderStyle::Solid,
                            ),
                        };
                        handle_quads.push(q);
                    }
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
        let editing = self.build_editing_paint(window);

        BoardPaint {
            background,
            grid,
            paths,
            texts,
            editing,
            selection_outline,
            handles: handle_quads,
            marquee,
        }
    }

    fn build_editing_paint(&self, window: &Window) -> Option<EditingPaint> {
        let ed = self.editing.as_ref()?;
        let el = self.scene.get(ed.element_id)?;
        let text = ed.session.text();
        let color = color_u32(el.style.stroke, el.style.opacity);
        let shaped = self.text_cache.shaped(
            &text,
            ed.font_size,
            &ed.font_family,
            ed.wrap_width,
            color,
            &self.camera,
            window,
        );
        let lines = shaped.lines;
        let line_height = shaped.line_height;
        // Live content size; bound labels center themselves on the container.
        let content_w = lines.iter().map(|l| l.width).fold(px(0.0), |a, b| a.max(b));
        let mut content_h = line_height * lines.len() as f32;
        if let Some(mh) = ed.min_height {
            content_h = content_h.max(self.camera.scale(mh));
        }
        let origin = self.editing_origin(el, ed, Some((content_w, content_h)), window);
        // Box width used for line alignment (see text_bounds below).
        let box_w = self.editing_box_width(ed, &lines);
        let offsets = line_offsets(&lines, box_w, ed.text_align);

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
                if overlap_start < overlap_end
                    || (overlap_start == overlap_end && sel.start == sel.end)
                {
                    let x0 = line.line.x_for_index(
                        (overlap_start - line.byte_range.start).min(line.byte_range.len()),
                    );
                    let x1 = line.line.x_for_index(
                        (overlap_end - line.byte_range.start).min(line.byte_range.len()),
                    );
                    if x1 > x0 {
                        selection_quads.push(fill(
                            Bounds {
                                origin: point(
                                    origin.x + offsets[i] + x0,
                                    origin.y + line_height * i as f32,
                                ),
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
                                origin.x + offsets[i] + x0,
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

        // Live bounds computed from the current session text (the scene
        // element's bounds only update on commit). A standalone wrapped box
        // keeps its wrap width so the wrapping boundary is visible; a bound
        // label hugs its content since it's centered on the container.
        let text_bounds = Bounds {
            origin,
            size: size(box_w.max(px(1.0)), content_h.max(px(1.0))),
        };
        // Visible text-box frame while typing, padded a little like
        // Excalidraw's editing outline. A bound label doesn't need one —
        // the container's own outline serves as the frame.
        let pad = px(4.0);
        let frame_quad = (ed.container_id.is_none()).then(|| {
            outline(
                Bounds {
                    origin: point(origin.x - pad, origin.y - pad),
                    size: size(
                        text_bounds.size.width + pad * 2.0,
                        text_bounds.size.height + pad * 2.0,
                    ),
                },
                color_u32(SELECTION_COLOR, 1.0),
                BorderStyle::Dashed,
            )
        });

        Some(EditingPaint {
            item: TextPaintItem {
                lines,
                line_offsets: offsets,
                origin,
                line_height,
                bounds: text_bounds,
                background: el.style.background.map(|c| color_u32(c, el.style.opacity)),
            },
            text_bounds,
            frame_quad,
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
                    ElementKind::Text {
                        text,
                        font_size,
                        font_family,
                        wrap_width,
                        min_height,
                        ..
                    } => Some((
                        text.clone(),
                        *font_size,
                        font_family.clone(),
                        *wrap_width,
                        *min_height,
                    )),
                    _ => None,
                });
                if let Some((text, font_size, font_family, wrap_width, min_height)) = info {
                    let (mut w, h) = measure_text(
                        &text,
                        font_size,
                        wrap_width,
                        min_height,
                        &font_family,
                        window,
                    );
                    let cid = self.scene.get(id).and_then(|el| el.container_id());
                    // Standalone wrapped boxes keep their wrap width; bound
                    // labels hug their content width (see commit_editing).
                    if cid.is_none() {
                        if let Some(ww) = wrap_width {
                            w = ww.max(w);
                        }
                    }
                    let cb = cid.and_then(|cid| self.scene.get(cid)).map(|c| c.bounds);
                    if let Some(el) = self.scene.get_mut(id) {
                        el.bounds.w = w.max(1.0);
                        el.bounds.h = h.max(1.0);
                        // A bound label keeps its alignment on the container.
                        if let Some(cb) = cb {
                            let align = el.text_align();
                            place_label(&mut el.bounds, cb, align);
                        }
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
        let cursor = self.cursor_style();

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
                    if let Some(q) = paint.background {
                        window.paint_quad(q);
                    }
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
                        if let Some(q) = &ed.frame_quad {
                            window.paint_quad(q.clone());
                        }
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

        // Windows-only in-app menu bar (None on macOS, which uses the native
        // `set_menus` bar). Computed before the chain so the builder doesn't
        // hold a borrow of `self`/`cx` alongside the chain's own use.
        let menubar = if cfg!(target_os = "windows") {
            Some(self.render_menu_bar(window, cx))
        } else {
            None
        };
        let menubar_dropdown = if cfg!(target_os = "windows") {
            self.render_menu_dropdown(cx)
        } else {
            None
        };

        div()
            .key_context(if editing { "Editor" } else { "Board" })
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
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
            .on_action(
                cx.listener(|this, _: &DeleteSelection, _window, cx| this.delete_selection(cx)),
            )
            .on_action(cx.listener(|this, _: &BringToFront, _window, cx| {
                this.reorder_layers(LayerOp::ToFront, cx)
            }))
            .on_action(cx.listener(|this, _: &SendToBack, _window, cx| {
                this.reorder_layers(LayerOp::ToBack, cx)
            }))
            .on_action(cx.listener(|this, _: &BringForward, _window, cx| {
                this.reorder_layers(LayerOp::Forward, cx)
            }))
            .on_action(cx.listener(|this, _: &SendBackward, _window, cx| {
                this.reorder_layers(LayerOp::Backward, cx)
            }))
            .on_action(cx.listener(|this, _: &CancelOp, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _window, cx| this.zoom_by(1.25, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _window, cx| this.zoom_by(0.8, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _window, cx| this.zoom_reset(cx)))
            .on_action(cx.listener(|this, _: &SelectTool, window, cx| {
                this.set_tool(ActiveTool::Select, window, cx)
            }))
            .on_action(cx.listener(|this, _: &HandTool, window, cx| {
                this.set_tool(ActiveTool::Hand, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RectTool, window, cx| {
                this.set_tool(ActiveTool::Rectangle, window, cx)
            }))
            .on_action(cx.listener(|this, _: &DiamondTool, window, cx| {
                this.set_tool(ActiveTool::Diamond, window, cx)
            }))
            .on_action(cx.listener(|this, _: &EllipseTool, window, cx| {
                this.set_tool(ActiveTool::Ellipse, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ArrowTool, window, cx| {
                this.set_tool(ActiveTool::Arrow, window, cx)
            }))
            .on_action(cx.listener(|this, _: &LineTool, window, cx| {
                this.set_tool(ActiveTool::Line, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PenTool, window, cx| {
                this.set_tool(ActiveTool::Pen, window, cx)
            }))
            .on_action(cx.listener(|this, _: &TextTool, window, cx| {
                this.set_tool(ActiveTool::Text, window, cx)
            }))
            .on_action(cx.listener(|this, _: &EraserTool, window, cx| {
                this.set_tool(ActiveTool::Eraser, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleAi, window, cx| this.toggle_ai_panel(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CheckForUpdates, _window, cx| {
                this.check_for_updates(cx, false)
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_left_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_middle_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .when_some(menubar, |d, bar| d.child(bar))
            .child(
                // Inner container: canvas + all floating chrome. It is the
                // flex item below the menu bar; `relative()` keeps the
                // absolute-positioned chrome (toolbar `top_3`, zoom bar
                // `bottom_3`, AI panel `right_0/top_0`) anchored here, and
                // `flex_1().min_h_0()` makes it fill the remaining height.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(canvas_el)
                    .child(self.render_toolbar(cx))
                    .child(self.render_style_bar(cx))
                    .child(self.render_element_info(cx))
                    .child(self.render_zoom_bar(cx))
                    .child(self.render_notice_bar())
                    .children(self.render_update_banner(cx))
                    .children(self.render_context_menu(cx))
                    .children(self.ai_panel.clone()),
            )
            // Dropdown overlay rendered last so it paints above the canvas.
            .when_some(menubar_dropdown, |d, dd| d.child(dd))
    }
}

fn paint_text_item(item: &TextPaintItem, window: &mut Window, cx: &mut App) {
    // Background fill (behind the text) if the element has a background color.
    if let Some(bg) = item.background {
        window.paint_quad(fill(item.bounds, bg));
    }
    for (i, line) in item.lines.iter().enumerate() {
        let origin = point(
            item.origin.x + item.line_offsets.get(i).copied().unwrap_or(px(0.0)),
            item.origin.y + item.line_height * i as f32,
        );
        let _ = line.line.paint(origin, item.line_height, window, cx);
    }
}

// ---------------------------------------------------------------------
// Chrome: toolbar / style bar / zoom bar / notice bar
// ---------------------------------------------------------------------

const STROKE_COLORS: [u32; 5] = [0x1e1e1e, 0xe03131, 0x2f9e44, 0x1971c2, 0xf08c00];
const BG_COLORS: [Option<u32>; 5] = [
    None,
    Some(0xffc9c9),
    Some(0xb2f2bb),
    Some(0xa5d8ff),
    Some(0xffec99),
];
const STROKE_WIDTHS: [(f64, f32); 3] = [(1.0, 1.0), (2.0, 2.0), (4.0, 4.0)];
const ROUGHNESSES: [f32; 3] = [0.0, 1.0, 2.0];
/// Font size presets: (font size in world units, glyph icon size in px).
/// The button icon is an "A" drawn at increasing sizes, Excalidraw-style.
const TEXT_SIZES: [(f64, f32); 4] = [(16.0, 10.0), (24.0, 13.0), (36.0, 16.0), (56.0, 19.0)];

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

/// A square button whose icon is a text glyph (e.g. "A" / "Aa") drawn at a
/// given pixel size, optionally in a specific font family. Used for the
/// font size / font family pickers, where the glyph itself is the preview.
fn glyph_button(
    id: impl Into<gpui::ElementId>,
    active: bool,
    glyph_px: f32,
    text: &'static str,
    font_family: Option<&'static str>,
) -> Stateful<Div> {
    let mut glyph = div()
        .text_size(px(glyph_px))
        .line_height(px(glyph_px + 4.0))
        .child(text);
    if let Some(family) = font_family {
        glyph = glyph.font_family(family);
    }
    let mut b = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size_7()
        .rounded_md()
        .cursor_pointer()
        .text_color(color_u32(
            if active { ICON_ACTIVE } else { ICON_NORMAL },
            1.0,
        ))
        .child(glyph);
    if active {
        b = b.bg(rgb(0xdce8ff));
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

/// A horizontal separator for the context menu.
fn menu_separator() -> Div {
    div().h(px(1.0)).w_full().bg(rgb(0xe3e2df))
}

// --- Windows in-app menu bar ---------------------------------------------
// GPUI's Windows backend renders nothing for `set_menus` (it only stores the
// definitions), so without a native bar the app has no File/Edit/View entries
// on Windows. These constants + helpers draw a compact in-app bar instead.
// macOS keeps using the native screen-top menu, so this code is gated at the
// call site by `cfg!(target_os = "windows")`; the helpers themselves are pure
// GPUI and compile on every platform.

/// Bar height (px). Compact but an easy click target.
const MENU_BAR_HEIGHT: f32 = 30.0;
/// Fixed width of each top-level menu label (px). Constant so the dropdown
/// can align under its label without measuring layout at runtime.
const MENU_LABEL_W: f32 = 56.0;
/// Left padding of the bar (px). Equals `px_2()`, so label `i` starts at
/// `MENU_PAD + i * MENU_LABEL_W` - the dropdown's `left` offset.
const MENU_PAD: f32 = 8.0;
/// Width of each window caption button (minimize / maximize / close). Windows
/// caption buttons are typically ~46px; matches the visual weight of the
/// 30px-tall bar.
const WIN_BTN_W: f32 = 46.0;

/// One window caption button (minimize / maximize-or-restore / close).
///
/// The click is handled directly via `on_click` calling the GPUI `Window`
/// methods (`minimize_window` / `zoom_window` / quit), the same approach
/// gpui-component's `ControlIcon` uses on Linux. We deliberately do NOT use
/// `window_control_area(Min/Max/Close)` here: that route relies on GPUI's
/// non-client `HTMINBUTTON`/`HTMAXBUTTON`/`HTCLOSE` handling, which only fires
/// when the NC mouse-down/up isn't consumed by an element - but the board's
/// `on_left_down` (on the outer div) intercepts it, so the native path never
/// triggers. `on_click` is reliable because it dispatches through the normal
/// client mouse event regardless. `on_mouse_down` stops propagation so clicking
/// a caption button doesn't also start a canvas action.
fn window_control_button(
    id: &'static str,
    icon: IconName,
    hover_bg: impl Into<Hsla>,
    hover_fg: impl Into<Hsla>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let hover_bg = hover_bg.into();
    let hover_fg = hover_fg.into();
    div()
        .id(id)
        .w(px(WIN_BTN_W))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(0x1e1e1e))
        .hover(|s| s.bg(hover_bg).text_color(hover_fg))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .child(Icon::new(icon))
}

/// The minimize / maximize-or-restore / close button group on the right of the
/// menu bar. The maximize icon swaps to a restore glyph when the window is
/// already maximized. Close quits (single-window app: closing the window quits
/// the process, matching the "退出" menu item).
fn window_controls(window: &Window) -> Div {
    let max_icon = if window.is_maximized() {
        IconName::WindowRestore
    } else {
        IconName::WindowMaximize
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .child(window_control_button(
            "win-min",
            IconName::WindowMinimize,
            rgb(0xf1f0ee),
            rgb(0x1e1e1e),
            |_, window, _| window.minimize_window(),
        ))
        .child(window_control_button(
            "win-max",
            max_icon,
            rgb(0xf1f0ee),
            rgb(0x1e1e1e),
            |_, window, _| crate::platform::toggle_maximize(window),
        ))
        // Soft red hover (light tint bg + red icon) instead of a harsh solid
        // red square - still reads as the "close" button but is gentler, and
        // matches the gray hovers of the other two in intensity. Palette
        // follows GitHub's danger colors (#ffebe9 / #cf222e).
        .child(window_control_button(
            "win-close",
            IconName::WindowClose,
            rgb(0xffebe9),
            rgb(0xcf222e),
            |_, _, cx| cx.quit(),
        ))
}

/// One row of the right-click context menu: an optional leading icon, a
/// label, an optional right-aligned shortcut hint, and a disabled state.
/// Disabled rows are greyed and don't register a click / hover.
fn context_menu_row(
    id: impl Into<gpui::ElementId>,
    icon: Option<IconName>,
    label: SharedString,
    shortcut: Option<SharedString>,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let label_color = if enabled {
        rgb(0x1e1e1e)
    } else {
        rgb(0xbbbbbb)
    };
    let icon_color = if enabled {
        rgb(0x3b3b3b)
    } else {
        rgb(0xbbbbbb)
    };
    let shortcut_color = if enabled {
        rgb(0x999999)
    } else {
        rgb(0xcccccc)
    };
    let mut row = div()
        .id(id)
        .h_7()
        .px_2()
        .rounded_md()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_sm()
        .text_color(label_color);
    if enabled {
        row = row
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0xf1f0ee)))
            .on_click(on_click);
    }
    // Fixed-width icon column keeps labels aligned whether or not a row has
    // an icon.
    let mut icon_col = div()
        .w(px(16.0))
        .flex()
        .items_center()
        .justify_center()
        .text_color(icon_color);
    if let Some(name) = icon {
        icon_col = icon_col.child(Icon::new(name));
    }
    row = row.child(icon_col);
    row = row.child(div().flex_1().child(label));
    if let Some(sc) = shortcut {
        row = row.child(div().text_xs().text_color(shortcut_color).child(sc));
    }
    row
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
                    // Do NOT regenerate the seed: roughr (and our freedraw
                    // sinusoidal offset) use it to control jitter. Changing
                    // the color or stroke width should not alter the shape.
                }
            }
            self.mark_dirty();
        }
        cx.notify();
    }

    /// Apply a shape-only style change (stroke width, roughness). Targets
    /// the selection — or the containers when only bound labels are
    /// selected, so a shape can be restyled while its label is edited.
    fn apply_shape_style(
        &mut self,
        apply: impl Fn(&mut ElementStyle) + Copy,
        cx: &mut Context<Self>,
    ) {
        apply(&mut self.style);
        let targets = self.panel_shape_ids();
        if !targets.is_empty() {
            self.history.record(&self.scene);
            for id in &targets {
                if let Some(el) = self.scene.get_mut(*id) {
                    apply(&mut el.style);
                    // Seed preserved — see apply_style_to_selection.
                }
            }
            self.mark_dirty();
        }
        cx.notify();
    }

    /// Apply a change to the font_size of the text elements the style bar
    /// controls (selected text + labels of selected containers), then
    /// re-measure their bounds. Also updates the default for new text and
    /// the live editing session, so the change is visible while typing.
    fn apply_style_to_text(&mut self, apply: impl Fn(&mut f64), cx: &mut Context<Self>) {
        apply(&mut self.text_font_size);
        if let Some(ed) = &mut self.editing {
            apply(&mut ed.font_size);
        }
        let targets = self.panel_text_ids();
        if !targets.is_empty() {
            self.history.record(&self.scene);
            for id in targets {
                if let Some(el) = self.scene.get_mut(id) {
                    if let ElementKind::Text { font_size, .. } = &mut el.kind {
                        apply(font_size);
                        self.pending_measure.push(id);
                    }
                }
            }
            self.mark_dirty();
        }
        cx.notify();
    }

    /// Set the font family for new text, the live editing session, and any
    /// selected text elements that don't already use it.
    fn set_text_font(&mut self, family: &str, cx: &mut Context<Self>) {
        self.text_font_family = family.to_string();
        if let Some(ed) = &mut self.editing {
            ed.font_family = family.to_string();
        }
        // Only touch elements that actually change, so repeated clicks on
        // the active family don't pollute the undo history.
        let targets: Vec<ElementId> = self
            .panel_text_ids()
            .into_iter()
            .filter(|id| {
                self.scene.get(*id).is_some_and(
                    |e| matches!(&e.kind, ElementKind::Text { font_family, .. } if font_family != family),
                )
            })
            .collect();
        if !targets.is_empty() {
            self.history.record(&self.scene);
            for id in targets {
                if let Some(el) = self.scene.get_mut(id) {
                    if let ElementKind::Text { font_family, .. } = &mut el.kind {
                        *font_family = family.to_string();
                        self.pending_measure.push(id);
                    }
                }
            }
            self.mark_dirty();
        }
        cx.notify();
    }

    /// Set the horizontal alignment for new text, the live editing session,
    /// and the text elements the style bar controls (selected text plus
    /// labels of selected containers).
    fn set_text_align(&mut self, align: TextAlign, cx: &mut Context<Self>) {
        self.text_align = align;
        if let Some(ed) = &mut self.editing {
            ed.text_align = align;
        }
        let targets: Vec<ElementId> = self
            .panel_text_ids()
            .into_iter()
            .filter(|id| self.scene.get(*id).is_some_and(|e| e.text_align() != align))
            .collect();
        if !targets.is_empty() {
            self.history.record(&self.scene);
            for id in targets {
                if let Some(el) = self.scene.get_mut(id) {
                    if let ElementKind::Text { text_align, .. } = &mut el.kind {
                        *text_align = align;
                        // Re-measure repositions bound labels.
                        self.pending_measure.push(id);
                    }
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
            (ActiveTool::Select, "选择", |c| {
                ic::select(c).into_any_element()
            }),
            (ActiveTool::Hand, "抓手", |c| {
                ic::hand(c).into_any_element()
            }),
            (ActiveTool::Rectangle, "矩形", |c| {
                ic::rectangle(c).into_any_element()
            }),
            (ActiveTool::Diamond, "菱形", |c| {
                ic::diamond(c).into_any_element()
            }),
            (ActiveTool::Ellipse, "椭圆", |c| {
                ic::ellipse(c).into_any_element()
            }),
            (ActiveTool::Arrow, "箭头", |c| {
                ic::arrow(c).into_any_element()
            }),
            (ActiveTool::Line, "直线", |c| {
                ic::line(c).into_any_element()
            }),
            (ActiveTool::Pen, "画笔", |c| ic::pen(c).into_any_element()),
            (ActiveTool::Text, "文本", |c| {
                ic::text(c).into_any_element()
            }),
            (ActiveTool::Eraser, "橡皮", |c| {
                ic::eraser(c).into_any_element()
            }),
        ];

        let mut bar = bar_container();
        // Whether a modifier is held that would trigger a temporary gesture —
        // used to highlight Hand (Ctrl) / Pen (Shift) even before the button is
        // pressed, for immediate visual feedback. Ignored while editing text.
        let ctrl_held =
            (self.modifiers.control || self.modifiers.platform) && self.editing.is_none();
        let shift_held = self.modifiers.shift && self.editing.is_none();
        for (tool, label, icon_fn) in tools {
            let weak = weak.clone();
            // Highlight the tool matching a live gesture: while Ctrl-drag pans
            // (Hand) or Shift-drag sketches (Pen), the current tool is unchanged
            // but those tools should look active. Also highlight on the modifier
            // being merely held, before the drag starts.
            let active = self.tool == tool
                || ((self.temp_pan || (ctrl_held && matches!(self.drag, DragState::Idle)))
                    && tool == ActiveTool::Hand)
                || ((self.temp_pen || (shift_held && matches!(self.drag, DragState::Idle)))
                    && tool == ActiveTool::Pen);
            bar = bar.child(
                bar_icon_button(
                    gpui::ElementId::Name(label.into()),
                    active,
                    icon_fn(icon_color(active)),
                )
                .on_click(move |_, window, cx| {
                    weak.update(cx, |this, cx| this.set_tool(tool, window, cx))
                        .ok();
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
                bar_icon_button("撤销", false, ic::undo(icon_color(false))).on_click(
                    move |_, window, cx| {
                        weak_undo.update(cx, |this, cx| this.undo(window, cx)).ok();
                    },
                ),
            )
            .child(
                bar_icon_button("重做", false, ic::redo(icon_color(false))).on_click(
                    move |_, window, cx| {
                        weak_redo.update(cx, |this, cx| this.redo(window, cx)).ok();
                    },
                ),
            )
            .child(div().w(px(1.0)).h_5().bg(rgb(0xe3e2df)).mx_1())
            .child(
                bar_icon_button("保存", false, ic::save(icon_color(false))).on_click(
                    move |_, _, cx| {
                        weak_save.update(cx, |this, cx| this.save(false, cx)).ok();
                    },
                ),
            )
            .child(
                bar_icon_button("打开", false, ic::open(icon_color(false))).on_click(
                    move |_, window, cx| {
                        weak_open.update(cx, |this, cx| this.open(window, cx)).ok();
                    },
                ),
            )
            .child(div().w(px(1.0)).h_5().bg(rgb(0xe3e2df)).mx_1())
            .child(
                bar_icon_button("AI", ai_active, ic::ai(icon_color(ai_active))).on_click(
                    move |_, window, cx| {
                        weak_ai
                            .update(cx, |this, cx| this.toggle_ai_panel(window, cx))
                            .ok();
                    },
                ),
            );

        // Center the toolbar over the *canvas* area. When the AI panel (which
        // docks to the right) is open, exclude its current width from the
        // centering region so the toolbar stays visually centered in the
        // remaining drawable space instead of drifting toward the panel.
        let panel_width = self.ai_panel_width(cx);
        let panel_open = self.ai_panel.is_some();

        div()
            .absolute()
            .top_3()
            .left_0()
            .when(panel_open, |d| d.right(px(panel_width)))
            .when(!panel_open, |d| d.right_0())
            .flex()
            .justify_center()
            .child(bar)
    }

    fn render_style_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let text_tool = self.tool == ActiveTool::Text;
        let show = self.tool.is_drawing() || text_tool || !self.selection.is_empty();
        // Text options show when: the Text tool is active, a container tool
        // is active (presetting the label style before drawing), the
        // selection is only text, or a single container is selected (they
        // then apply to its bound label, or the defaults for a future one).
        let only_text = !self.selection.is_empty()
            && self
                .selection
                .iter()
                .all(|id| self.scene.get(*id).is_some_and(|e| e.is_text()));
        let container_tool = matches!(
            self.tool,
            ActiveTool::Rectangle | ActiveTool::Ellipse | ActiveTool::Diamond
        );
        let single_container = self.selection.len() == 1
            && self
                .scene
                .get(self.selection[0])
                .is_some_and(|e| e.is_container());
        let show_text_options = text_tool || container_tool || only_text || single_container;
        // Shape options hide in text-only contexts — except when the
        // selected text is bound label(s): then they apply to the container,
        // so a shape shows *all* options even while its label is edited.
        let bound_labels_only = !self.selection.is_empty()
            && self.selection.iter().all(|id| {
                self.scene.get(*id).is_some_and(|e| {
                    e.container_id()
                        .is_some_and(|cid| self.scene.get(cid).is_some())
                })
            });
        let show_shape_options = (!text_tool && !only_text) || bound_labels_only;

        // Background fill only applies to closed shapes (rectangle/ellipse/
        // diamond). Lines, arrows and freedraw strokes are open polylines
        // with no fillable interior, so hide the background-color row when
        // the selection (or active tool) is purely linear.
        let linear_tool = matches!(
            self.tool,
            ActiveTool::Arrow | ActiveTool::Line | ActiveTool::Pen
        );
        let linear_sel = !self.selection.is_empty()
            && self
                .selection
                .iter()
                .filter_map(|id| self.scene.get(*id))
                .all(|e| {
                    matches!(
                        e.kind,
                        ElementKind::Line { .. }
                            | ElementKind::Arrow { .. }
                            | ElementKind::Freedraw { .. }
                    )
                });
        // When bound labels are selected, the shape targets are their
        // containers — check those instead.
        let linear_targets = if bound_labels_only {
            self.panel_shape_ids()
                .iter()
                .filter_map(|id| self.scene.get(*id))
                .all(|e| matches!(e.kind, ElementKind::Line { .. } | ElementKind::Arrow { .. }))
        } else {
            linear_sel
        };
        let show_background =
            show_shape_options && !(linear_tool && self.selection.is_empty()) && !linear_targets;
        let _ = linear_sel; // used via linear_targets when applicable

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

        // Background colors — hidden for linear elements (lines/arrows/pen)
        // which have no fillable interior.
        if show_background {
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
                    None => swatch
                        .bg(rgb(0xffffff))
                        .child(crate::icons::no_fill(color_u32(0x999999, 1.0))),
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

            // 填充样式：排线（hachure 手绘线）vs 实心（色块）。选中或预设
            // 均可切换，作用于 shape 的 background 填充。
            if show_shape_options {
                let mut fs_row = div().flex().flex_row().gap_1();
                for (ix, (label, fs)) in [
                    ("纹", crate::scene::FillStyle::Hachure),
                    ("实", crate::scene::FillStyle::Solid),
                ]
                .into_iter()
                .enumerate()
                {
                    let weak = weak.clone();
                    let active = self.style.fill_style == fs;
                    fs_row = fs_row.child(
                        div()
                            .id(gpui::ElementId::named_usize("fill-style", ix))
                            .size_5()
                            .rounded_sm()
                            .border_1()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .child(label)
                            .when(active, |d| d.border_color(rgb(SELECTION_COLOR)).border_2())
                            .when(!active, |d| d.border_color(rgb(0xcccccc)))
                            .on_click(move |_, _, cx| {
                                weak.update(cx, |this, cx| {
                                    this.apply_style_to_selection(|s| s.fill_style = fs, cx)
                                })
                                .ok();
                            }),
                    );
                }
                bar = bar.child(fs_row);
            }
        }

        // Text options: font size presets + font family + alignment, shown
        // as glyph icons. With no selection (Text tool active) the buttons
        // reflect and change the defaults applied to newly created text;
        // with a container selected they apply to its bound label.
        if show_text_options {
            use crate::icons as ic;
            // The text the buttons act on: selected text + labels of
            // selected containers.
            let panel_ids = self.panel_text_ids();
            // Current font size: first target text element, else the
            // default for new text. Dragged/resized text can have any size,
            // so highlight the *closest* preset rather than exact matches.
            let current_size = panel_ids
                .iter()
                .filter_map(|id| self.scene.get(*id))
                .find_map(|e| match &e.kind {
                    ElementKind::Text { font_size, .. } => Some(*font_size),
                    _ => None,
                })
                .unwrap_or(self.text_font_size);
            let nearest_size = TEXT_SIZES
                .iter()
                .map(|(s, _)| *s)
                .min_by(|a, b| {
                    (a - current_size)
                        .abs()
                        .partial_cmp(&(b - current_size).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(self.text_font_size);
            let mut row = div().flex().flex_row().gap_1();
            for (ix, (size, glyph_px)) in TEXT_SIZES.iter().enumerate() {
                let weak = weak.clone();
                let size = *size;
                let active = (size - nearest_size).abs() < 1e-6;
                row = row.child(
                    glyph_button(
                        gpui::ElementId::named_usize("fs", ix),
                        active,
                        *glyph_px,
                        "A",
                        None,
                    )
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |this, cx| this.apply_style_to_text(|fs| *fs = size, cx))
                            .ok();
                    }),
                );
            }
            bar = bar.child(row);

            // Font family: handwritten vs regular, each button previewed in
            // its own font.
            let family_active = |family: &str| {
                if panel_ids.is_empty() {
                    self.text_font_family == family
                } else {
                    panel_ids
                        .iter()
                        .filter_map(|id| self.scene.get(*id))
                        .any(|e| e.font_family() == family)
                }
            };
            let mut row = div().flex().flex_row().gap_1();
            for (ix, family) in [crate::render::HANDWRITTEN_FONT, crate::render::SYSTEM_FONT]
                .iter()
                .enumerate()
            {
                let weak = weak.clone();
                let family = *family;
                let active = family_active(family);
                row = row.child(
                    glyph_button(
                        gpui::ElementId::named_usize("ff", ix),
                        active,
                        15.0,
                        "Aa",
                        Some(family),
                    )
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |this, cx| this.set_text_font(family, cx))
                            .ok();
                    }),
                );
            }
            bar = bar.child(row);

            // Alignment: left / center / right.
            let align_active = |a: TextAlign| {
                if panel_ids.is_empty() {
                    self.text_align == a
                } else {
                    panel_ids
                        .iter()
                        .filter_map(|id| self.scene.get(*id))
                        .any(|e| e.text_align() == a)
                }
            };
            let mut row = div().flex().flex_row().gap_1();
            for (ix, a) in [TextAlign::Left, TextAlign::Center, TextAlign::Right]
                .iter()
                .enumerate()
            {
                let weak = weak.clone();
                let a = *a;
                let active = align_active(a);
                row = row.child(
                    bar_icon_button(
                        gpui::ElementId::named_usize("al", ix),
                        active,
                        ic::align_icon(icon_color(active), a),
                    )
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |this, cx| this.set_text_align(a, cx)).ok();
                    }),
                );
            }
            bar = bar.child(row);
        }

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
                            this.apply_shape_style(|s| s.stroke_width = width, cx)
                        })
                        .ok();
                    }),
                );
            }
            bar = bar.child(row);

            // 笔锋 (ink taper): only for the Pen tool — it controls the
            // variable-width effect on new freehand strokes. Existing tapered
            // strokes keep their baked widths.
            if matches!(self.tool, ActiveTool::Pen) {
                use crate::icons as ic;
                let weak = weak.clone();
                let active = self.pen_taper;
                bar = bar.child(
                    bar_icon_button(
                        gpui::ElementId::named_usize("pt", 0),
                        active,
                        ic::taper_icon(icon_color(active)),
                    )
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.pen_taper = !this.pen_taper;
                            cx.notify();
                        })
                        .ok();
                    }),
                );
            }

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
                            this.apply_shape_style(|s| s.roughness = roughness, cx)
                        })
                        .ok();
                    }),
                );
            }
            bar = bar.child(row);
        }

        // Line type (straight/curved): only in linear contexts — the
        // Arrow/Line tool is active (presetting the default for new
        // elements) or the selection contains a line/arrow.
        let line_context = matches!(self.tool, ActiveTool::Arrow | ActiveTool::Line)
            || self.selection.iter().any(|id| {
                self.scene.get(*id).is_some_and(|e| {
                    matches!(e.kind, ElementKind::Line { .. } | ElementKind::Arrow { .. })
                })
            });
        if show_shape_options && line_context {
            use crate::icons as ic;
            // Current value: the first selected line/arrow's, else the
            // default applied to new elements.
            let current = self
                .selection
                .iter()
                .filter_map(|id| self.scene.get(*id))
                .find(|e| matches!(e.kind, ElementKind::Line { .. } | ElementKind::Arrow { .. }))
                .map(|e| e.style.line_type)
                .unwrap_or(self.style.line_type);
            let mut row = div().flex().flex_row().gap_1();
            for (ix, lt) in [LineType::Straight, LineType::Curved]
                .into_iter()
                .enumerate()
            {
                let weak = weak.clone();
                let active = current == lt;
                row = row.child(
                    bar_icon_button(
                        gpui::ElementId::named_usize("lt", ix),
                        active,
                        ic::line_type_icon(icon_color(active), lt),
                    )
                    .on_click(move |_, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.apply_shape_style(|s| s.line_type = lt, cx)
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

    /// A floating read-only info card (top-right) that shows the selected
    /// element's type, short id, position/size, and text. Only renders when
    /// exactly one element is selected. Avoids the AI panel when it's open.
    fn render_element_info(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Show only for a single selection.
        let el_ref = if self.selection.len() == 1 {
            self.scene.get(self.selection[0])
        } else {
            None
        };
        let panel_open = self.ai_panel.is_some();
        let panel_width = self.ai_panel_width(cx);
        let el_ref = match el_ref {
            Some(e) => e,
            None => return div().absolute().top_3().into_any_element(),
        };
        // Build a human-readable type label.
        let kind_label = match &el_ref.kind {
            ElementKind::Rectangle => "矩形",
            ElementKind::Ellipse => "椭圆",
            ElementKind::Diamond => "菱形",
            ElementKind::Arrow { .. } => "箭头",
            ElementKind::Line { .. } => "直线",
            ElementKind::Text { .. } => "文本",
            ElementKind::Freedraw { .. } => "手绘",
            ElementKind::Polygon { .. } => "多边形",
        };
        let short_id = &el_ref.id.to_string()[..8];
        let b = &el_ref.bounds;
        let pos_text = format!("({:.0}, {:.0})", b.x, b.y);
        let size_text = format!("{:.0} × {:.0}", b.w, b.h);
        let text_content = el_ref.text().map(|t| t.to_string());

        let mut card = div()
            .absolute()
            .top_3()
            .flex()
            .flex_col()
            .gap_0p5()
            .px_3()
            .py_2()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xe3e2df))
            .rounded_lg()
            .shadow_lg()
            .text_xs()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(kind_label))
                    .child(
                        div()
                            .id("element-info-id")
                            .text_color(rgb(0x999999))
                            .cursor_pointer()
                            .hover(|s| s.text_color(rgb(0x1a5fd7)))
                            .child(format!("#{short_id} 📋"))
                            .on_click({
                                let full_id = el_ref.id.to_string();
                                move |_, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        full_id.clone(),
                                    ));
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .text_color(rgb(0x666666))
                    .child(div().child(pos_text))
                    .child(div().child(size_text)),
            );
        if let Some(t) = text_content {
            card = card.child(div().text_color(rgb(0x444444)).child(format!("文字: {t}")));
        }
        // Position: right-aligned, but avoid the AI panel.
        if panel_open {
            card = card.right(px(panel_width + 12.0));
        } else {
            card = card.right_3();
        }
        card.into_any_element()
    }

    /// The right-click context menu: layer ops (front/forward/backward/back),
    /// delete, and a contextual "delete vertex" when right-clicked on a vertex
    /// handle of a selected line/arrow. Dismissed by `on_mouse_down_out`
    /// (any click outside the card), Esc, or picking an item.
    fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let cm = self.context_menu.as_ref()?;
        let weak = cx.weak_entity();
        let ids = self.selection.clone();

        // Clamp the menu inside the window so it never overflows the edge.
        let menu_w = px(190.0);
        let menu_h = px(260.0);
        let win = self.canvas_bounds.size;
        let left = if cm.position.x + menu_w > win.width {
            (win.width - menu_w).max(px(0.0))
        } else {
            cm.position.x
        };
        let top = if cm.position.y + menu_h > win.height {
            (win.height - menu_h).max(px(0.0))
        } else {
            cm.position.y
        };

        let mut card = div()
            .id("context-menu")
            .absolute()
            .left(left)
            .top(top)
            .min_w(menu_w)
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xe3e2df))
            .rounded_md()
            .shadow_lg()
            .p_1()
            .flex()
            .flex_col()
            .gap_0p5()
            // Clicks on the menu itself don't bubble to the canvas (so a
            // right-click on the menu doesn't reopen it, and a left-click on
            // an item doesn't also select/marquee on the canvas).
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            // Any mouse-down outside the card closes the menu (click-away).
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.context_menu = None;
                cx.notify();
            }));

        // Contextual: delete the vertex under the cursor (selected line/arrow).
        if let Some(idx) = cm.vertex {
            let weak = weak.clone();
            card = card.child(context_menu_row(
                "cm-vertex",
                None,
                "删除顶点".into(),
                None,
                true,
                move |_, _, cx| {
                    let _ = weak.update(cx, |b, cx| b.delete_vertex(idx, cx));
                },
            ));
            card = card.child(menu_separator());
        }

        // Layer ops. Disabled (greyed, non-interactive) when the move can't apply.
        let layer_ops: [(LayerOp, &str, IconName, &str); 4] = [
            (
                LayerOp::ToFront,
                "置于顶层",
                IconName::ArrowUp,
                "Ctrl+Shift+]",
            ),
            (LayerOp::Forward, "上移一层", IconName::ChevronUp, "Ctrl+]"),
            (
                LayerOp::Backward,
                "下移一层",
                IconName::ChevronDown,
                "Ctrl+[",
            ),
            (
                LayerOp::ToBack,
                "置于底层",
                IconName::ArrowDown,
                "Ctrl+Shift+[",
            ),
        ];
        for (i, (op, label, icon, sc)) in layer_ops.into_iter().enumerate() {
            let enabled = match op {
                LayerOp::ToFront => self.scene.can_front(&ids),
                LayerOp::ToBack => self.scene.can_back(&ids),
                LayerOp::Forward => self.scene.can_forward(&ids),
                LayerOp::Backward => self.scene.can_backward(&ids),
            };
            let weak = weak.clone();
            card = card.child(context_menu_row(
                gpui::ElementId::named_usize("cm-layer", i),
                Some(icon),
                label.into(),
                Some(sc.into()),
                enabled,
                move |_, _, cx| {
                    let _ = weak.update(cx, |b, cx| b.reorder_layers(op, cx));
                },
            ));
        }

        card = card.child(menu_separator());

        // Delete selection.
        let weak = weak.clone();
        card = card.child(context_menu_row(
            "cm-delete",
            Some(IconName::Delete),
            "删除".into(),
            Some("Delete".into()),
            !ids.is_empty(),
            move |_, _, cx| {
                let _ = weak.update(cx, |b, cx| {
                    b.delete_selection(cx);
                    b.context_menu = None;
                });
            },
        ));

        Some(card.into_any_element())
    }

    /// The Windows top menu bar (文件 / 编辑 / 视图) plus a draggable spacer and
    /// window caption buttons (minimize / maximize / close). Placed as the first
    /// child of the board's outer column so it occupies layout space at the top;
    /// macOS uses the native `set_menus` bar instead and never calls this.
    fn render_menu_bar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let labels: [&str; 4] = ["文件", "编辑", "视图", "帮助"];
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .pl_2()
            .h(px(MENU_BAR_HEIGHT))
            .flex_shrink_0()
            .border_b_1()
            .border_color(rgb(0xe3e2df))
            .bg(rgb(0xffffff))
            // The bar inherits the canvas tool cursor (crosshair/ibeam/...)
            // from the board's outer div otherwise; force the normal arrow so
            // the title bar always looks like a title bar. Menu labels below
            // override this with `cursor_pointer` for their own click hint.
            .cursor(CursorStyle::Arrow);
        for (i, label) in labels.into_iter().enumerate() {
            let active = self.menubar_open == Some(i);
            bar = bar.child(
                div()
                    .id(gpui::ElementId::named_usize("menu-label", i))
                    .w(px(MENU_LABEL_W))
                    .h(px(MENU_BAR_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(rgb(0x1e1e1e))
                    .cursor_pointer()
                    .when(active, |d| d.bg(rgb(0xf1f0ee)))
                    .hover(|s| s.bg(rgb(0xf1f0ee)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menubar_open = if this.menubar_open == Some(i) {
                            None
                        } else {
                            Some(i)
                        };
                        cx.notify();
                    }))
                    .child(label),
            );
        }
        // Draggable spacer: the empty middle of the bar moves the window. We
        // don't use window_control_area(Drag) here - that route depends on a
        // current mouse_hit_test, which is often stale at click time so the hit
        // falls back to HTCLIENT and nothing drags. Instead, on mouse-down we
        // synthesize an HTCAPTION non-client click (see platform::start_window_
        // drag), which reliably starts Windows' caption drag. stop_propagation
        // keeps the board's on_left_down from also firing.
        bar = bar.child(
            div()
                .id("title-drag")
                .flex_1()
                .h_full()
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    crate::platform::begin_window_drag(window);
                    cx.stop_propagation();
                })
                .on_mouse_move(|event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        crate::platform::move_window_drag(window);
                        cx.stop_propagation();
                    }
                })
                .on_mouse_up(MouseButton::Left, |_, window, cx| {
                    crate::platform::end_window_drag(window);
                    cx.stop_propagation();
                }),
        );
        // Caption buttons on the right; the OS handles their clicks.
        bar = bar.child(window_controls(window));
        bar
    }

    /// The dropdown for the currently open menu (None when collapsed). Rendered
    /// as the *last* child of the board's outer column so it paints above the
    /// canvas. Reuses the context-menu card pattern (white card, click-away
    /// dismiss via `on_mouse_down_out`, `context_menu_row` rows).
    fn render_menu_dropdown(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let i = self.menubar_open?;
        let weak = cx.weak_entity();
        let left = px(MENU_PAD + i as f32 * MENU_LABEL_W);

        let mut card = div()
            .id("menu-dropdown")
            .absolute()
            .top(px(MENU_BAR_HEIGHT))
            .left(left)
            .w(px(180.0))
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xe3e2df))
            .rounded_md()
            .shadow_lg()
            .p_1()
            .flex()
            .flex_col()
            .gap_0p5()
            // Clicks on the card don't bubble to the canvas.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // Any mouse-down outside the card closes the menu.
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menubar_open = None;
                cx.notify();
            }));

        // Each row runs its action on the board via the captured weak handle,
        // then collapses the menu. `cx` in the row closure is `&mut App`, so
        // `cx.quit()` (no entity needed) is called directly for 退出.
        match i {
            0 => {
                // 文件
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-open",
                    Some(IconName::FolderOpen),
                    "打开场景…".into(),
                    Some("Ctrl+O".into()),
                    true,
                    move |_, window, cx| {
                        w.update(cx, |this, cx| {
                            this.open(window, cx);
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                    },
                ));
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-save",
                    Some(IconName::File),
                    "保存场景".into(),
                    Some("Ctrl+S".into()),
                    true,
                    move |_, _, cx| {
                        w.update(cx, |this, cx| {
                            this.save(false, cx);
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                    },
                ));
                card = card.child(menu_separator());
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-quit",
                    None,
                    "退出 Boundless".into(),
                    Some("Ctrl+Q".into()),
                    true,
                    move |_, _, cx| {
                        w.update(cx, |this, cx| {
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                        cx.quit();
                    },
                ));
            }
            1 => {
                // 编辑
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-undo",
                    Some(IconName::Undo),
                    "撤销".into(),
                    Some("Ctrl+Z".into()),
                    true,
                    move |_, window, cx| {
                        w.update(cx, |this, cx| {
                            this.undo(window, cx);
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                    },
                ));
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-redo",
                    Some(IconName::Redo),
                    "重做".into(),
                    Some("Ctrl+Shift+Z".into()),
                    true,
                    move |_, window, cx| {
                        w.update(cx, |this, cx| {
                            this.redo(window, cx);
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                    },
                ));
            }
            2 => {
                // 视图
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-zin",
                    Some(IconName::Plus),
                    "放大".into(),
                    Some("Ctrl+=".into()),
                    true,
                    move |_, _, cx| {
                        w.update(cx, |this, cx| {
                            this.zoom_by(1.25, cx);
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                    },
                ));
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-zout",
                    Some(IconName::Minus),
                    "缩小".into(),
                    Some("Ctrl+-".into()),
                    true,
                    move |_, _, cx| {
                        w.update(cx, |this, cx| {
                            this.zoom_by(0.8, cx);
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                    },
                ));
                card = card.child(menu_separator());
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-zreset",
                    None,
                    "重置缩放".into(),
                    Some("Ctrl+0".into()),
                    true,
                    move |_, _, cx| {
                        w.update(cx, |this, cx| {
                            this.zoom_reset(cx);
                            this.menubar_open = None;
                            cx.notify();
                        })
                        .ok();
                    },
                ));
            }
            3 => {
                // 帮助
                let w = weak.clone();
                card = card.child(context_menu_row(
                    "m-check-updates",
                    None,
                    "检查更新…".into(),
                    None,
                    true,
                    move |_, _, cx| {
                        w.update(cx, |this, cx| {
                            this.menubar_open = None;
                            this.check_for_updates(cx, false);
                            cx.notify();
                        })
                        .ok();
                    },
                ));
            }
            _ => return None,
        }

        Some(card.into_any_element())
    }

    fn render_zoom_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::icons as ic;
        let weak = cx.weak_entity();
        let percent = format!("{:.0}%", self.camera.zoom * 100.0);

        let weak_out = weak.clone();
        let weak_in = weak.clone();
        let weak_reset = weak.clone();
        let weak_fit = weak.clone();
        let weak_grid = weak.clone();
        let grid_on = self.show_grid;

        // Dock above/below the AI panel rather than under it: when the panel
        // is open, push the zoom bar to the left of it so it stays visible.
        // Uses the panel's *current* (possibly user-resized) width.
        // `right_3` = 12px inset either way.
        const INSET: f32 = 12.0;
        let panel_width = self.ai_panel_width(cx);
        let panel_open = self.ai_panel.is_some();

        div()
            .absolute()
            .bottom_3()
            .when(panel_open, |d| d.right(px(panel_width + INSET)))
            .when(!panel_open, |d| d.right(px(INSET)))
            .flex()
            .child(
                bar_container()
                    .child(bar_button("−", false).on_click(move |_, _, cx| {
                        weak_out.update(cx, |this, cx| this.zoom_by(0.8, cx)).ok();
                    }))
                    .child(
                        div()
                            .id("zoom-percent")
                            .w_12()
                            .text_center()
                            .text_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0xf1f0ee)).rounded_md())
                            // Double-click the percentage to reset zoom to 100%,
                            // replacing the dedicated reset button (Excalidraw/
                            // Figma behavior). GPUI has no on_double_click helper,
                            // so detect it via MouseDownEvent::click_count == 2.
                            .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                                if event.click_count >= 2 {
                                    weak_reset.update(cx, |this, cx| this.zoom_reset(cx)).ok();
                                }
                            })
                            .child(percent),
                    )
                    .child(bar_button("+", false).on_click(move |_, _, cx| {
                        weak_in.update(cx, |this, cx| this.zoom_by(1.25, cx)).ok();
                    }))
                    .child(
                        bar_icon_button("zoom-fit", false, ic::zoom_fit(icon_color(false)))
                            .on_click(move |_, _, cx| {
                                weak_fit
                                    .update(cx, |this, cx| {
                                        if let Some(bounds) = this.scene.content_bounds() {
                                            let viewport = this.viewport_bounds(cx).size;
                                            this.camera.zoom_to_fit(bounds, viewport);
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                            }),
                    )
                    .child(
                        bar_icon_button("toggle-grid", grid_on, ic::grid(icon_color(grid_on)))
                            .on_click(move |_, _, cx| {
                                weak_grid
                                    .update(cx, |this, cx| {
                                        this.show_grid = !this.show_grid;
                                        this.mark_dirty();
                                        cx.notify();
                                    })
                                    .ok();
                            }),
                    )
                    // Canvas surface swatches: white board / green chalkboard
                    // / black chalkboard. Same colors the AI's
                    // set_canvas_background presets use.
                    .child({
                        let mut swatches = div().flex().flex_row().gap_1().items_center();
                        for (i, (_name, color)) in [
                            ("白板", None),
                            ("墨绿黑板", Some(0x2A5240_u32)),
                            ("黑板黑", Some(0x1F1F1F_u32)),
                        ]
                        .into_iter()
                        .enumerate()
                        {
                            let weak_bg = weak.clone();
                            let active = self.canvas_background == color;
                            swatches = swatches.child(
                                div()
                                    .id(gpui::ElementId::named_usize("bg-swatch", i))
                                    .w_4()
                                    .h_4()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .border_1()
                                    .when(active, |d| d.border_2().border_color(rgb(0x2563eb)))
                                    .when(!active, |d| d.border_color(rgb(0xc9c9c9)))
                                    .bg(match color {
                                        Some(c) => color_u32(c, 1.0),
                                        None => white(),
                                    })
                                    .on_click(move |_, _, cx| {
                                        weak_bg
                                            .update(cx, |this, cx| {
                                                this.set_canvas_background(color, cx)
                                            })
                                            .ok();
                                    }),
                            );
                        }
                        swatches
                    }),
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

    /// A small bottom-center banner for the auto-update flow: download
    /// progress, a "ready to restart" prompt, or an error. Hidden when idle /
    /// checking / available (transient states).
    fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::updater::UpdateState;
        let weak = cx.weak_entity();
        let is_restart = matches!(self.update_state, UpdateState::Ready { .. });
        let (text, has_action): (String, bool) = match &self.update_state {
            UpdateState::Idle | UpdateState::Checking => return None,
            UpdateState::Downloading { fraction } => (
                format!("正在下载更新 {}%", (fraction * 100.0) as u32),
                false,
            ),
            UpdateState::Ready { version, notes, .. } => {
                let mut t = format!("新版本 v{} 已就绪，重启以应用", version);
                // Append the first line of the release notes, truncated, so the
                // banner stays compact.
                if let Some(line) = notes.lines().next() {
                    let line = line.trim();
                    if !line.is_empty() {
                        t.push_str("  ·  ");
                        let max = 60;
                        if line.len() > max {
                            t.push_str(&line[..max]);
                            t.push('…');
                        } else {
                            t.push_str(line);
                        }
                    }
                }
                (t, true)
            }
            UpdateState::UpToDate => (
                format!("已是最新版本 v{}", crate::updater::current_version()),
                true,
            ),
            UpdateState::Installing => ("正在安装，即将重启…".to_string(), false),
            UpdateState::Error { message } => (format!("更新失败：{}", message), true),
        };
        let action_label = if is_restart { "重启应用" } else { "关闭" };

        let mut card = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xe3e2df))
            .rounded_lg()
            .shadow_lg()
            .text_sm()
            .text_color(rgb(0x1e1e1e))
            .child(text);
        if has_action {
            let bg = if is_restart {
                rgb(0x1a5fd7)
            } else {
                rgb(0xf1f0ee)
            };
            let fg = if is_restart {
                rgb(0xffffff)
            } else {
                rgb(0x1e1e1e)
            };
            card = card.child(
                div()
                    .id("upd-action")
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .text_color(fg)
                    .cursor_pointer()
                    .when(is_restart, |d| d.bg(bg))
                    .when(!is_restart, |d| d.hover(|s| s.bg(rgb(0xebeaea))))
                    .on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            if is_restart {
                                this.install_and_restart(cx);
                            } else {
                                this.dismiss_update(cx);
                            }
                        });
                    })
                    .child(action_label),
            );
        }

        Some(
            div()
                .absolute()
                .bottom_3()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(card)
                .into_any_element(),
        )
    }
}
