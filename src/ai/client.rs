//! Shared types for the AI subsystem plus the shared tokio runtime.
//!
//! The heavy lifting (OpenAI-compatible client, streaming, tool-calling loop)
//! is now done by [`rig`](https://crates.io/crates/rig-core) — see
//! [`super::agent`]. This module keeps the small, UI-facing data types the rest
//! of the app builds on:
//! - [`ChatMessage`]: a stored conversation turn (role + text).
//! - [`tokio_runtime`]: the single background runtime rig's agent runs on.
//!
//! Previously this file held a hand-rolled SSE client over `reqwest`; it was
//! replaced by rig, which also gives us multi-turn tool calling the old client
//! could not do.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

/// A single stored conversation message (system / user / assistant).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    #[allow(dead_code)]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// The single, lazily-initialized tokio runtime that runs all AI work (rig
/// agent streams and tool calls). A dedicated multi-thread runtime keeps this
/// off the GPUI main thread; results are ferried back via futures channels that
/// a GPUI task polls.
pub fn tokio_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for AI client")
    })
}

/// Kept for compatibility; the panel now drives the agent through
/// [`super::agent`] but a cancel handle is still a useful shared type.
#[allow(dead_code)]
pub type CancelHandle = Arc<std::sync::atomic::AtomicBool>;
