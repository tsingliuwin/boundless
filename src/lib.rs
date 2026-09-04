//! boundless library crate: exposes the app's modules so examples (the
//! blackboard-poster evaluation harness) and integration tests can reuse
//! them. The binary in main.rs is a thin shell around `board::BoardView`.

pub mod ai;
pub mod board;
pub mod camera;
pub mod history;
pub mod icons;
pub mod ink;
pub mod platform;
pub mod render;
pub mod scene;
pub mod settings_page;
pub mod text;
pub mod tools;
pub mod updater;
pub mod workspace;
