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
}

impl SceneFile {
    pub fn new(scene: &Scene, camera: Camera) -> Self {
        Self {
            r#type: SCENE_TYPE.to_string(),
            version: SCENE_VERSION,
            camera,
            elements: scene.elements.clone(),
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
}
