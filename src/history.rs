//! Snapshot-based undo/redo: simple and robust for a whiteboard.

use crate::scene::{Element, Scene};

const HISTORY_LIMIT: usize = 100;

#[derive(Default)]
pub struct History {
    undo_stack: Vec<Vec<Element>>,
    redo_stack: Vec<Vec<Element>>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the current scene state BEFORE a mutation is applied.
    pub fn record(&mut self, scene: &Scene) {
        self.undo_stack.push(scene.elements.clone());
        if self.undo_stack.len() > HISTORY_LIMIT {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    #[allow(dead_code)]
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    #[allow(dead_code)]
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self, scene: &mut Scene) -> bool {
        if let Some(previous) = self.undo_stack.pop() {
            self.redo_stack
                .push(std::mem::replace(&mut scene.elements, previous));
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self, scene: &mut Scene) -> bool {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack
                .push(std::mem::replace(&mut scene.elements, next));
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{ElementKind, ElementStyle, WBounds};

    fn rect() -> Element {
        Element::new(
            ElementKind::Rectangle,
            WBounds::new(0.0, 0.0, 10.0, 10.0),
            ElementStyle::default(),
        )
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut scene = Scene::new();
        let mut history = History::new();

        history.record(&scene);
        scene.add(rect());
        assert_eq!(scene.elements.len(), 1);

        history.record(&scene);
        scene.add(rect());
        assert_eq!(scene.elements.len(), 2);

        assert!(history.undo(&mut scene));
        assert_eq!(scene.elements.len(), 1);
        assert!(history.undo(&mut scene));
        assert_eq!(scene.elements.len(), 0);
        assert!(!history.undo(&mut scene));

        assert!(history.redo(&mut scene));
        assert!(history.redo(&mut scene));
        assert_eq!(scene.elements.len(), 2);
        assert!(!history.redo(&mut scene));
    }

    #[test]
    fn new_mutation_clears_redo() {
        let mut scene = Scene::new();
        let mut history = History::new();
        history.record(&scene);
        scene.add(rect());
        assert!(history.undo(&mut scene));
        history.record(&scene);
        scene.add(rect());
        assert!(!history.can_redo());
    }
}
