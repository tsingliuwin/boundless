//! The scene: an ordered collection of elements plus file (de)serialization.

pub mod element;

pub use element::*;

use crate::camera::Camera;
use serde::{Deserialize, Serialize};

pub const SCENE_TYPE: &str = "boundless-scene";
pub const SCENE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default)]
pub struct Scene {
    /// z-order: later elements are painted on top.
    pub elements: Vec<Element>,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, element: Element) -> ElementId {
        let id = element.id;
        self.elements.push(element);
        id
    }

    pub fn remove(&mut self, id: ElementId) -> Option<Element> {
        let idx = self.elements.iter().position(|e| e.id == id)?;
        Some(self.elements.remove(idx))
    }

    pub fn get(&self, id: ElementId) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }

    pub fn get_mut(&mut self, id: ElementId) -> Option<&mut Element> {
        self.elements.iter_mut().find(|e| e.id == id)
    }

    /// Resolve an element by the short id prefix the AI tools use (draw tools
    /// report only the first 8 chars back to the model). Accepts either the
    /// 8-char prefix or the full UUID string (a full id is a prefix of itself).
    /// Returns the first match - UUID v4 collisions on 8 hex chars are
    /// astronomically unlikely for a whiteboard's element count.
    pub fn find_by_id_prefix(&self, prefix: &str) -> Option<ElementId> {
        self.elements
            .iter()
            .find(|e| e.id.to_string().starts_with(prefix))
            .map(|e| e.id)
    }

    /// Topmost element at the given world point.
    pub fn hit_test(&self, p: WPoint, tol: f64) -> Option<ElementId> {
        self.elements
            .iter()
            .rev()
            .find(|e| e.hit_test(p, tol))
            .map(|e| e.id)
    }

    /// All elements fully or partially inside the given bounds.
    pub fn elements_in(&self, bounds: &WBounds) -> Vec<ElementId> {
        self.elements
            .iter()
            .filter(|e| bounds.intersects(&e.bounds))
            .map(|e| e.id)
            .collect()
    }

    pub fn content_bounds(&self) -> Option<WBounds> {
        let mut iter = self.elements.iter();
        let first = iter.next()?;
        Some(iter.fold(first.bounds, |acc, e| acc.union(&e.bounds)))
    }

    pub fn restore(&mut self, elements: Vec<Element>) {
        self.elements = elements;
    }

    // -----------------------------------------------------------------
    // Z-order (layer) operations on a selection set. The Vec index is the
    // z-order (later = on top). All four preserve the selected elements'
    // relative order and operate on the group as a whole.
    // -----------------------------------------------------------------

    /// Move all selected elements to the top (end of the Vec), preserving
    /// their relative order. Stable sort: non-selected (false=0) first,
    /// selected (true=1) last = on top.
    pub fn move_to_front(&mut self, ids: &[ElementId]) {
        self.elements.sort_by_key(|e| ids.contains(&e.id));
    }

    /// Move all selected elements to the bottom (start of the Vec).
    pub fn send_to_back(&mut self, ids: &[ElementId]) {
        self.elements.sort_by_key(|e| !ids.contains(&e.id));
    }

    /// Move the selected group one layer toward the top. Process from the
    /// highest index down so each swap frees the slot for the next, keeping
    /// the group contiguous-ish and ordered.
    pub fn bring_forward(&mut self, ids: &[ElementId]) {
        for i in (0..self.elements.len().saturating_sub(1)).rev() {
            if ids.contains(&self.elements[i].id) && !ids.contains(&self.elements[i + 1].id) {
                self.elements.swap(i, i + 1);
            }
        }
    }

    /// Move the selected group one layer toward the bottom.
    pub fn send_backward(&mut self, ids: &[ElementId]) {
        for i in 1..self.elements.len() {
            if ids.contains(&self.elements[i].id) && !ids.contains(&self.elements[i - 1].id) {
                self.elements.swap(i, i - 1);
            }
        }
    }

    /// Can the selected group move to the front? (Not if every selected
    /// element is already at the top, i.e. the tail of the Vec is all
    /// selected with no non-selected after the last selected.)
    pub fn can_front(&self, ids: &[ElementId]) -> bool {
        self.elements
            .iter()
            .rev()
            .take_while(|e| ids.contains(&e.id))
            .count()
            < ids
                .iter()
                .filter(|id| self.elements.iter().any(|e| e.id == **id))
                .count()
    }

    /// Can the selected group move to the back?
    pub fn can_back(&self, ids: &[ElementId]) -> bool {
        self.elements
            .iter()
            .take_while(|e| ids.contains(&e.id))
            .count()
            < ids
                .iter()
                .filter(|id| self.elements.iter().any(|e| e.id == **id))
                .count()
    }

    /// Can the selected group move one layer up? (Some selected element has a
    /// non-selected element immediately above it.)
    pub fn can_forward(&self, ids: &[ElementId]) -> bool {
        (0..self.elements.len().saturating_sub(1))
            .any(|i| ids.contains(&self.elements[i].id) && !ids.contains(&self.elements[i + 1].id))
    }

    /// Can the selected group move one layer down?
    pub fn can_backward(&self, ids: &[ElementId]) -> bool {
        (1..self.elements.len())
            .any(|i| ids.contains(&self.elements[i].id) && !ids.contains(&self.elements[i - 1].id))
    }
}

/// Serde default for `SceneFile::show_grid`: scenes default to a clean
/// (no-dots) canvas, matching Excalidraw's default.
fn default_false() -> bool {
    false
}

/// On-disk scene format (`.boundless`, JSON).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneFile {
    pub r#type: String,
    pub version: u32,
    #[serde(default)]
    pub camera: Camera,
    #[serde(default)]
    pub elements: Vec<Element>,
    /// Whether the dot grid is visible. Defaults to true for scenes saved
    /// before this field existed.
    #[serde(default = "default_false")]
    pub show_grid: bool,
    /// Canvas background color (0xRRGGBB), e.g. the dark green of a
    /// blackboard theme. None = the default white board.
    #[serde(default)]
    pub background: Option<u32>,
}

impl SceneFile {
    pub fn new(scene: &Scene, camera: Camera) -> Self {
        Self {
            r#type: SCENE_TYPE.to_string(),
            version: SCENE_VERSION,
            camera,
            elements: scene.elements.clone(),
            show_grid: false,
            background: None,
        }
    }

    pub fn parse(json: &str) -> anyhow::Result<Self> {
        let file: SceneFile = serde_json::from_str(json)?;
        anyhow::ensure!(
            file.r#type == SCENE_TYPE,
            "不是 boundless 场景文件 (type = {:?})",
            file.r#type
        );
        anyhow::ensure!(
            file.version <= SCENE_VERSION,
            "场景文件版本 {} 高于当前支持的 {}",
            file.version,
            SCENE_VERSION
        );
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_file_roundtrip_and_validation() {
        let mut scene = Scene::new();
        scene.add(Element::new(
            ElementKind::Diamond,
            WBounds::new(0.0, 0.0, 40.0, 40.0),
            ElementStyle::default(),
        ));
        let file = SceneFile::new(&scene, Camera::default());
        let json = serde_json::to_string_pretty(&file).unwrap();
        let parsed = SceneFile::parse(&json).unwrap();
        assert_eq!(parsed.elements.len(), 1);

        assert!(SceneFile::parse(r#"{"type":"other","version":1}"#).is_err());
        assert!(SceneFile::parse(r#"{"type":"boundless-scene","version":99}"#).is_err());
    }

    #[test]
    fn hit_test_returns_topmost() {
        let mut scene = Scene::new();
        let style = ElementStyle {
            background: Some(0xff0000),
            ..Default::default()
        };
        let bottom = scene.add(Element::new(
            ElementKind::Rectangle,
            WBounds::new(0.0, 0.0, 100.0, 100.0),
            style.clone(),
        ));
        let top = scene.add(Element::new(
            ElementKind::Rectangle,
            WBounds::new(10.0, 10.0, 100.0, 100.0),
            style,
        ));
        assert_eq!(scene.hit_test(WPoint::new(50.0, 50.0), 1.0), Some(top));
        assert_eq!(scene.hit_test(WPoint::new(5.0, 5.0), 1.0), Some(bottom));
        assert_eq!(scene.hit_test(WPoint::new(500.0, 500.0), 1.0), None);
    }

    #[test]
    fn find_by_id_prefix_matches_short_prefix_and_full_id() {
        // The AI draw tools report only the first 8 chars of an element's UUID
        // back to the model, so update/delete must resolve by that prefix.
        let mut scene = Scene::new();
        let id = scene.add(Element::new(
            ElementKind::Rectangle,
            WBounds::new(0.0, 0.0, 10.0, 10.0),
            ElementStyle::default(),
        ));
        let full = id.to_string();
        let prefix = &full[..8];

        assert_eq!(scene.find_by_id_prefix(prefix), Some(id));
        assert_eq!(scene.find_by_id_prefix(&full), Some(id)); // full id is a prefix of itself
        assert_eq!(scene.find_by_id_prefix("deadbeef"), None); // no match
    }

    fn scene_with_ids(n: usize) -> (Scene, Vec<ElementId>) {
        let mut scene = Scene::new();
        let mut ids = Vec::new();
        for _ in 0..n {
            ids.push(scene.add(Element::new(
                ElementKind::Rectangle,
                WBounds::new(0.0, 0.0, 10.0, 10.0),
                ElementStyle::default(),
            )));
        }
        (scene, ids)
    }

    #[test]
    fn move_to_front_and_send_to_back_preserve_relative_order() {
        let (mut scene, ids) = scene_with_ids(4); // [a, b, c, d] bottom->top
                                                  // Bring b and d (non-contiguous) to the front: order should be
                                                  // [a, c, b, d] (non-selected keep order, selected keep order).
        scene.move_to_front(&[ids[1], ids[3]]);
        assert_eq!(
            scene.elements.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![ids[0], ids[2], ids[1], ids[3]]
        );
        // Send b and d to the back: [b, d, a, c].
        scene.send_to_back(&[ids[1], ids[3]]);
        assert_eq!(
            scene.elements.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![ids[1], ids[3], ids[0], ids[2]]
        );
    }

    #[test]
    fn bring_forward_and_send_backward_shift_group_one_layer() {
        let (mut scene, ids) = scene_with_ids(5); // [a, b, c, d, e]
                                                  // Select b and d. bring_forward once: each swaps up if the slot above
                                                  // is free. b->above is c (free) => b swaps with c. d->above is e
                                                  // (free) => d swaps with e. Result: [a, c, b, e, d].
        scene.bring_forward(&[ids[1], ids[3]]);
        assert_eq!(
            scene.elements.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![ids[0], ids[2], ids[1], ids[4], ids[3]]
        );
        // send_backward on b, d: b->below is a (free) => swap. d->below is e
        // (free) => swap. Result: [b, a, d, c, e]... process low->high:
        // i=1 (b): below a free => swap => [a,b,c,e,d] wait recompute.
        // Start: [a, c, b, e, d]. send_backward([b, d]):
        //   i=1 c? not selected. i=2 b: below=c free => swap => [a, b, c, e, d]
        //   i=3 e? not selected. i=4 d: below=e free => swap => [a, b, c, d, e]
        // Back to original order.
        scene.send_backward(&[ids[1], ids[3]]);
        assert_eq!(
            scene.elements.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![ids[0], ids[1], ids[2], ids[3], ids[4]]
        );
    }

    #[test]
    fn can_front_back_forward_backward_at_boundaries() {
        let (mut scene, ids) = scene_with_ids(4); // [a, b, c, d]

        // Bottom element a: can_back false, can_backward false; can_front/forward true.
        assert!(!scene.can_back(&[ids[0]]) && !scene.can_backward(&[ids[0]]));
        assert!(scene.can_front(&[ids[0]]) && scene.can_forward(&[ids[0]]));

        // Top element d: can_front false, can_forward false; can_back/backward true.
        assert!(!scene.can_front(&[ids[3]]) && !scene.can_forward(&[ids[3]]));
        assert!(scene.can_back(&[ids[3]]) && scene.can_backward(&[ids[3]]));

        // After moving a to front, a is on top: can_front now false.
        scene.move_to_front(&[ids[0]]);
        assert!(!scene.can_front(&[ids[0]]) && !scene.can_forward(&[ids[0]]));
    }
}
