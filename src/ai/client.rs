//! OpenAI-compatible chat client with SSE streaming.
//!
//! Requests run on a dedicated background tokio runtime; stream events are
//! delivered through an executor-agnostic `futures` channel that a GPUI task
//! can poll.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Context as _};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::settings::AiSettings;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
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

/// Events produced by a streaming completion.
#[derive(Clone, Debug, PartialEq)]
pub enum AiStreamEvent {
    Delta(String),
    Done,
    Error(String),
}

/// Handle to a running request; drop the flag to cancel.
pub struct AiRequest {
    pub events: UnboundedReceiver<AiStreamEvent>,
    pub cancel: Arc<AtomicBool>,
}

fn tokio_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for AI client")
    })
}

/// Start a streaming chat completion.
pub fn chat_stream(
    settings: AiSettings,
    messages: Vec<ChatMessage>,
) -> AiRequest {
    let (tx, rx) = unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_task = cancel.clone();
    tokio_runtime().spawn(async move {
        if let Err(e) = run_stream(settings, messages, tx.clone(), cancel_task).await {
            let _ = tx.unbounded_send(AiStreamEvent::Error(format!("{e:#}")));
        }
    });
    AiRequest { events: rx, cancel }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

async fn run_stream(
    settings: AiSettings,
    messages: Vec<ChatMessage>,
    tx: UnboundedSender<AiStreamEvent>,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    if settings.api_key.is_empty() {
        return Err(anyhow!(
            "未配置 API Key：请在 AI 面板设置中填写，或设置环境变量 OPENAI_API_KEY"
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let response = client
        .post(settings.chat_completions_url())
        .bearer_auth(&settings.api_key)
        .json(&ChatRequest {
            model: &settings.model,
            messages: &messages,
            stream: true,
        })
        .send()
        .await
        .context("请求发送失败")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body = body.chars().take(500).collect::<String>();
        return Err(anyhow!("API 返回 {}: {}", status.as_u16(), body));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.unbounded_send(AiStreamEvent::Done);
            return Ok(());
        }
        let chunk = chunk.context("读取响应流失败")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // SSE events are separated by newlines; keep the last partial line.
        while let Some(pos) = buffer.find('\n') {
            let line: String = buffer.drain(..=pos).collect();
            match parse_sse_line(line.trim_end_matches(['\r', '\n'])) {
                SseEvent::Delta(text) => {
                    let _ = tx.unbounded_send(AiStreamEvent::Delta(text));
                }
                SseEvent::Done => {
                    let _ = tx.unbounded_send(AiStreamEvent::Done);
                    return Ok(());
                }
                SseEvent::Ignore => {}
            }
        }
    }
    // Stream ended without [DONE].
    let _ = tx.unbounded_send(AiStreamEvent::Done);
    Ok(())
}

/// Result of parsing a single SSE line.
#[derive(Debug, PartialEq)]
pub enum SseEvent {
    Delta(String),
    Done,
    Ignore,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
}

/// Parse one SSE line. Tolerant of keep-alive comments, empty lines and
/// non-data payloads, as "OpenAI-compatible" providers vary.
pub fn parse_sse_line(line: &str) -> SseEvent {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return SseEvent::Ignore;
    }
    let Some(data) = line.strip_prefix("data:") else {
        return SseEvent::Ignore;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return SseEvent::Done;
    }
    match serde_json::from_str::<StreamChunk>(data) {
        Ok(chunk) => chunk
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.delta.content)
            .filter(|s| !s.is_empty())
            .map(SseEvent::Delta)
            .unwrap_or(SseEvent::Ignore),
        Err(_) => SseEvent::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_lines() {
        let line = r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"你好"}}]}"#;
        assert_eq!(parse_sse_line(line), SseEvent::Delta("你好".to_string()));
    }

    #[test]
    fn parses_done_and_ignores_noise() {
        assert_eq!(parse_sse_line("data: [DONE]"), SseEvent::Done);
        assert_eq!(parse_sse_line(""), SseEvent::Ignore);
        assert_eq!(parse_sse_line(": keep-alive"), SseEvent::Ignore);
        assert_eq!(parse_sse_line("event: message"), SseEvent::Ignore);
        assert_eq!(parse_sse_line("data: {invalid json"), SseEvent::Ignore);
        // role-only first chunk has no content
        let line = r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert_eq!(parse_sse_line(line), SseEvent::Ignore);
        // empty content
        let line = r#"data: {"choices":[{"delta":{"content":""}}]}"#;
        assert_eq!(parse_sse_line(line), SseEvent::Ignore);
    }

    #[test]
    fn parses_finish_chunk() {
        let line = r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        assert_eq!(parse_sse_line(line), SseEvent::Ignore);
    }
}
