//! Structured agent-run logs.
//!
//! Every agent run writes a JSONL file under `~/.boundless/agent-logs/` with
//! one event per line: `run_start`, `tool_call`, `tool_result`, `done`,
//! `error`. The log is the raw material for the evaluation loop — the
//! blackboard-poster harness replays these events to rebuild what the agent
//! actually did and feeds an automated review.
//!
//! Design: a process-global run slot (`Mutex<Option<RunState>>`) because the
//! panel runs at most one agent at a time. Logging must never break the app —
//! every failure path prints to stderr and disables the log for the rest of
//! the run. Writes are synchronous appends (one line each, flushed), which is
//! negligible next to model round-trips.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::ai::store::data_dir;

/// One event in an agent run's JSONL log. `ts` is milliseconds since the
/// Unix epoch; `seq` numbers tool calls within the run (1-based).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LogEvent {
    RunStart {
        ts: u128,
        prompt: String,
        model: String,
    },
    ToolCall {
        ts: u128,
        seq: usize,
        tool: String,
        args: serde_json::Value,
    },
    ToolResult {
        ts: u128,
        seq: usize,
        tool: String,
        ok: bool,
        outcome: String,
        duration_ms: u128,
    },
    Done {
        ts: u128,
        turns: usize,
        drew_anything: bool,
        final_text: String,
    },
    Error {
        ts: u128,
        message: String,
    },
}

struct RunState {
    file: File,
    path: PathBuf,
    started: Instant,
    seq: usize,
    pending_call: Option<(usize, String, Instant)>,
}

static RUN: Mutex<Option<RunState>> = Mutex::new(None);

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn logs_dir() -> PathBuf {
    data_dir().join("agent-logs")
}

fn write_event(state: &mut RunState, event: LogEvent) {
    let Ok(mut line) = serde_json::to_string(&event) else {
        return;
    };
    line.push('\n');
    if let Err(e) = state.file.write_all(line.as_bytes()) {
        eprintln!("agent log write failed ({:?}): {e}", state.path);
        // Disable logging for the rest of the run rather than spamming.
        *state = RunState {
            file: devnull(),
            path: state.path.clone(),
            started: state.started,
            seq: state.seq,
            pending_call: state.pending_call.take(),
        };
        return;
    }
    let _ = state.file.flush();
}

/// A sink that discards writes, used after a hard write error so the rest of
/// the run doesn't re-print the same failure.
fn devnull() -> File {
    #[cfg(target_os = "windows")]
    {
        OpenOptions::new()
            .write(true)
            .open("NUL")
            .unwrap_or_else(|_| {
                // Last resort: a temp-looking file that goes nowhere.
                fs::File::create(std::env::temp_dir().join("boundless-log-sink")).unwrap()
            })
    }
    #[cfg(not(target_os = "windows"))]
    {
        OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap_or_else(|e| panic!("cannot open /dev/null: {e}"))
    }
}

/// Begin a logged run: creates `agent-logs/run-<timestamp>.jsonl` and writes
/// the `run_start` event. Returns the log path for display; `None` when the
/// log could not be created (the run proceeds unlogged).
pub fn begin_run(prompt: &str, model: &str) -> Option<PathBuf> {
    let dir = logs_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("agent log dir create failed: {e}");
        return None;
    }
    let stamp = now_ms();
    let path = dir.join(format!("run-{stamp}.jsonl"));
    let file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("agent log open failed ({:?}): {e}", path);
            return None;
        }
    };
    let mut state = RunState {
        file,
        path: path.clone(),
        started: Instant::now(),
        seq: 0,
        pending_call: None,
    };
    write_event(
        &mut state,
        LogEvent::RunStart {
            ts: now_ms(),
            prompt: prompt.to_string(),
            model: model.to_string(),
        },
    );
    *RUN.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    Some(path)
}

/// Log one tool invocation. No-op when no run is active.
pub fn log_tool_call(tool: &str, args: &serde_json::Value) {
    let mut guard = RUN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return;
    };
    state.seq += 1;
    let seq = state.seq;
    state.pending_call = Some((seq, tool.to_string(), Instant::now()));
    write_event(
        state,
        LogEvent::ToolCall {
            ts: now_ms(),
            seq,
            tool: tool.to_string(),
            args: args.clone(),
        },
    );
}

/// Log the outcome of the most recent tool invocation. `is_error` is the
/// tool's error flag (as on `AgentEvent::ToolResult`); the event stores
/// `ok: !is_error` so a logged `true` always means success. No-op when no
/// run is active or no call is pending.
pub fn log_tool_result(is_error: bool, outcome: &str) {
    let mut guard = RUN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return;
    };
    let Some((seq, tool, called_at)) = state.pending_call.take() else {
        return;
    };
    write_event(
        state,
        LogEvent::ToolResult {
            ts: now_ms(),
            seq,
            tool,
            ok: !is_error,
            outcome: outcome.to_string(),
            duration_ms: called_at.elapsed().as_millis(),
        },
    );
}

/// Close the run with a `done` event.
pub fn end_run(drew_anything: bool, final_text: &str) {
    let mut guard = RUN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut state) = guard.take() else {
        return;
    };
    let turns = state.seq;
    write_event(
        &mut state,
        LogEvent::Done {
            ts: now_ms(),
            turns,
            drew_anything,
            final_text: final_text.to_string(),
        },
    );
}

/// Log a run-level error (kept inside the still-open run).
pub fn log_error(message: &str) {
    let mut guard = RUN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return;
    };
    write_event(
        state,
        LogEvent::Error {
            ts: now_ms(),
            message: message.to_string(),
        },
    );
}

/// The active run's log path, for the UI notice line.
pub fn current_path() -> Option<PathBuf> {
    let guard = RUN.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|s| s.path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_with_tag() {
        let e = LogEvent::ToolResult {
            ts: 42,
            seq: 3,
            tool: "draw_text".into(),
            ok: true,
            outcome: "已添加文本 id=abc".into(),
            duration_ms: 12,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains(r#""event":"tool_result""#), "{json}");
        assert!(json.contains(r#""ok":true"#), "{json}");
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["seq"], 3);
    }

    #[test]
    fn run_lifecycle_writes_ordered_events() {
        // begin_run/end_run on the global slot — safe in tests because they
        // are sequential. Use a marker prompt to identify this run's file.
        let path = begin_run("日志自检", "test-model").expect("log should start");
        // Success path: is_error=false → the event records ok=true.
        log_tool_call("draw_text", &serde_json::json!({"x": 1.0, "text": "你好"}));
        log_tool_result(false, "已添加文本 id=00000000");
        end_run(true, "完成");
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4, "run_start + tool_call + tool_result + done");
        assert!(lines[0].contains("run_start"));
        assert!(lines[1].contains("tool_call"));
        assert!(lines[2].contains("tool_result"));
        assert!(lines[3].contains("done"));
        // The log dir is real user data (~/.boundless/agent-logs) — this test
        // must not leave a fake "test-model" run behind for later analysis.
        let _ = fs::remove_file(&path);
    }
}
