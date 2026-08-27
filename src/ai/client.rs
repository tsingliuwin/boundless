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

/// One ordered step in an assistant turn: a chunk of reasoning/thinking, or a
/// tool call with its arguments. Steps are kept in execution order so the UI can
/// render the "think → tool → think → tool → answer" loop the way a person would
/// narrate working through a task. Each step is independently collapsible.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantStep {
    /// A chunk of the model's reasoning/thinking text.
    Reasoning { text: String },
    /// A tool call the model made, with the raw JSON arguments it supplied.
    /// `args` is preserved so the UI can show a friendly description of what
    /// the call did (coordinates, sizes, colors…). `done` tracks the execution
    /// lifecycle: `false` while the tool is running, `true` once its result
    /// arrived (matched via the `id` on `AgentEvent::ToolResult`). Legacy
    /// steps (deserialized from old saves) default to `true`.
    ///
    /// `id` is rig's internal tool-call id, used only at runtime to pair a
    /// `ToolCall` event with its `ToolResult`. It is not serialized (it has no
    /// meaning after the stream ends).
    Tool {
        name: String,
        args: serde_json::Value,
        #[serde(default = "default_tool_done")]
        done: bool,
        /// True when the tool call failed (its result is an error, not a
        /// success). Persisted so a reloaded conversation keeps the red error
        /// state. Defaults false for legacy steps.
        #[serde(default)]
        error: bool,
        #[serde(skip)]
        id: String,
        /// The tool's result text (e.g. "已添加到画布"), set when the matching
        /// ToolResult arrives. Runtime-only (not serialized). Empty for
        /// pending tools and legacy steps.
        #[serde(skip)]
        result: String,
    },
    /// A chunk of the model's visible text output (the actual reply, as
    /// opposed to reasoning). Kept as a step so it lands in the right
    /// position within the ordered sequence when the model interleaves text
    /// with tool calls across multiple turns (e.g. text → tools → text).
    Text { text: String },
}

/// Serde default for `AssistantStep::Tool::done`: a tool loaded from an old
/// save file (pre-`done` field) is treated as completed, since it was persisted
/// only after the stream finished.
fn default_tool_done() -> bool {
    true
}

/// A single stored conversation message (system / user / assistant).
///
/// Assistant turns store their ordered reasoning + tool-call steps in `steps`.
/// The legacy `reasoning` / `tool_calls` fields are kept only as a read-side
/// shim so conversations saved before the per-step rewrite still load: if
/// `steps` is empty but a legacy field has data, [`Self::normalized_steps`]
/// rebuilds a best-effort step list from them. New messages only populate
/// `steps`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Legacy: the model's reasoning text (pre-step format). Read-only shim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Legacy: tool-call names without arguments (pre-step format). Read-only
    /// shim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    /// Ordered reasoning/tool steps (assistant only). The source of truth for
    /// new messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<AssistantStep>,
}

impl ChatMessage {
    #[allow(dead_code)]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            steps: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            steps: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            steps: Vec::new(),
        }
    }

    /// The effective ordered step list for this message. Prefers the new
    /// `steps` field; if it's empty (e.g. a conversation saved before the
    /// per-step rewrite), rebuilds a best-effort list from the legacy
    /// `reasoning` + `tool_calls` fields so old history still renders.
    pub fn normalized_steps(&self) -> Vec<AssistantStep> {
        if !self.steps.is_empty() {
            return self.steps.clone();
        }
        let mut out = Vec::new();
        if let Some(text) = self.reasoning.as_ref().filter(|t| !t.is_empty()) {
            out.push(AssistantStep::Reasoning { text: text.clone() });
        }
        for name in &self.tool_calls {
            // Legacy tool_calls carried no arguments; use a null placeholder.
            // `done` defaults true — these steps only exist from completed turns.
            out.push(AssistantStep::Tool {
                name: name.clone(),
                args: serde_json::Value::Null,
                done: true,
                error: false,
                id: String::new(),
                result: String::new(),
            });
        }
        out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_message_rebuilds_steps_from_old_fields() {
        // A message saved before the per-step rewrite carries reasoning text in
        // `reasoning` and tool names (no args) in `tool_calls`, with no `steps`.
        let mut m = ChatMessage::assistant("done");
        m.reasoning = Some("思考中".into());
        m.tool_calls = vec!["draw_rectangle".into()];
        let steps = m.normalized_steps();
        assert_eq!(steps.len(), 2);
        assert!(matches!(
            steps[0],
            AssistantStep::Reasoning { ref text } if text == "思考中"
        ));
        assert!(matches!(
            steps[1],
            AssistantStep::Tool { ref name, .. } if name == "draw_rectangle"
        ));
    }

    #[test]
    fn steps_roundtrip_through_jsonl() {
        // New-format messages keep their ordered steps after serialize/deserialize.
        let mut m = ChatMessage::assistant("画好了");
        m.steps = vec![
            AssistantStep::Reasoning { text: "先画框".into() },
            AssistantStep::Tool {
                name: "draw_rectangle".into(),
                args: serde_json::json!({ "x": 10.0, "y": 20.0, "w": 100.0, "h": 50.0 }),
                done: true,
                error: false,
                id: String::new(),
                result: String::new(),
            },
            AssistantStep::Reasoning { text: "再连线".into() },
            AssistantStep::Tool {
                name: "draw_arrow".into(),
                args: serde_json::json!({ "points": [{"x":0.0,"y":0.0},{"x":1.0,"y":1.0}] }),
                done: false,
                error: false,
                id: String::new(),
                result: String::new(),
            },
        ];
        let json = serde_json::to_string(&m).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.steps, m.steps);
        // The thinking→tool→thinking→tool order is preserved.
        assert_eq!(back.steps.len(), 4);
    }

    #[test]
    fn assistant_message_has_no_steps_by_default() {
        let m = ChatMessage::assistant("hello");
        assert!(m.normalized_steps().is_empty());
    }

    #[test]
    fn tool_step_without_done_field_defaults_true() {
        // A tool step saved before the `done` field existed should deserialize
        // with done=true (the serde default), since it was only persisted after
        // the stream finished.
        let json = r#"{"role":"assistant","content":"x","steps":[
            {"kind":"tool","name":"draw_rectangle","args":{"x":1.0}}
        ]}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            &m.steps[0],
            AssistantStep::Tool { name, done: true, .. } if name == "draw_rectangle"
        ));
    }
}
