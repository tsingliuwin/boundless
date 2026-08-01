//! Persistent storage for AI conversations.
//!
//! All AI data lives under `~/.boundless/` (Windows: `C:\Users\<user>\.boundless`):
//! - `config.json` - provider settings (base URL / API key / model)
//! - `chat/<session-id>.jsonl` - one file per conversation, one `ChatMessage`
//!   per line (JSONL). Append-only writes are O(1) and survive partial writes:
//!   at worst the last line is truncated, never the whole file.
//!
//! Session IDs are UUIDv4 (file names aren't meant to be human-readable; the UI
//! shows the first user message as a preview). Files are created lazily on the
//! first appended message, so "new session" needs no I/O.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use uuid::Uuid;

use super::client::ChatMessage;

/// Maximum chars of a message used as the session list preview.
const PREVIEW_CHARS: usize = 30;

/// Root data directory: `~/.boundless`. Created on first access.
pub fn data_dir() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".boundless")
}

/// Chat sessions directory: `~/.boundless/chat`.
pub fn chat_dir() -> PathBuf {
    data_dir().join("chat")
}

/// Metadata about a stored session, used to render the session list.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    /// Short preview (first user message, truncated) for the list.
    pub preview: String,
    /// File modification time, for "newest first" ordering.
    pub mtime: SystemTime,
    /// Number of messages stored.
    pub count: usize,
}

/// Generate a new, unused session id (UUIDv4). Does not create a file - the
/// file is created lazily when the first message is appended.
pub fn create_session() -> String {
    Uuid::new_v4().to_string()
}

/// Append one message to a session's JSONL file, creating the file (and the
/// chat directory) if necessary. Append-only, so concurrent-ish appends within
/// a single process are safe and O(1).
pub fn append_message(id: &str, msg: &ChatMessage) -> Result<()> {
    append_message_in(&chat_dir(), id, msg)
}

fn append_message_in(chat_dir: &std::path::Path, id: &str, msg: &ChatMessage) -> Result<()> {
    fs::create_dir_all(chat_dir).context("创建会话目录失败")?;
    let line = serde_json::to_string(msg).context("序列化消息失败")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(chat_dir.join(format!("{id}.jsonl")))
        .with_context(|| format!("打开会话文件失败: {id}"))?;
    writeln!(file, "{line}").context("写入会话文件失败")?;
    Ok(())
}

/// Load all messages of a session, in order. Lines that fail to parse are
/// skipped (tolerant of a truncated final line from a crash mid-write).
pub fn load_messages(id: &str) -> Result<Vec<ChatMessage>> {
    load_messages_in(&chat_dir(), id)
}

fn load_messages_in(chat_dir: &std::path::Path, id: &str) -> Result<Vec<ChatMessage>> {
    let path = chat_dir.join(format!("{id}.jsonl"));
    let file = match OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("打开会话文件失败: {id}")),
    };
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    for line in reader.lines() {
        let line = line.context("读取会话文件失败")?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ChatMessage>(&line) {
            Ok(msg) => messages.push(msg),
            Err(_) => continue, // skip corrupt/partial line
        }
    }
    Ok(messages)
}

/// List all stored sessions, newest first. Files that can't be read are
/// skipped. Each session's preview comes from its first user message.
pub fn list_sessions() -> Vec<SessionMeta> {
    list_sessions_in(&chat_dir())
}

fn list_sessions_in(chat_dir: &std::path::Path) -> Vec<SessionMeta> {
    let dir = match fs::read_dir(chat_dir) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut sessions = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        let messages = load_messages_in(chat_dir, &id).unwrap_or_default();
        let preview = messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| truncate_preview(&m.content))
            .unwrap_or_else(|| "（空会话）".to_string());
        let count = messages.len();
        sessions.push(SessionMeta {
            id,
            preview,
            mtime,
            count,
        });
    }
    // Newest first.
    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    sessions
}

/// Delete a session file. Missing file is not an error.
pub fn delete_session(id: &str) -> Result<()> {
    delete_session_in(&chat_dir(), id)
}

fn delete_session_in(chat_dir: &std::path::Path, id: &str) -> Result<()> {
    let path = chat_dir.join(format!("{id}.jsonl"));
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("删除会话文件失败: {id}"))?;
    }
    Ok(())
}

/// Truncate `s` to `PREVIEW_CHARS` chars, appending an ellipsis if cut, and
/// collapsing newlines so multi-line messages preview on one line.
fn truncate_preview(s: &str) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= PREVIEW_CHARS {
        one_line
    } else {
        let head: String = chars[..PREVIEW_CHARS].iter().collect();
        format!("{head}…")
    }
}

/// One-time migration: if the new `~/.boundless/config.json` doesn't exist but
/// the legacy `config_dir/boundless/config.json` does, copy it over so users
/// don't have to re-enter their API key after this path change.
pub fn migrate_legacy_config() {
    let new_path = data_dir().join("config.json");
    if new_path.exists() {
        return;
    }
    let legacy = dirs::config_dir()
        .map(|d| d.join("boundless").join("config.json"))
        .filter(|p| p.exists());
    if let Some(legacy) = legacy {
        if fs::create_dir_all(data_dir()).is_ok() {
            let _ = fs::copy(&legacy, &new_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway chat dir under the system temp, isolating tests from
    /// the real `~/.boundless`. We exercise the `_in` variants directly since
    /// `data_dir()` reads the live home dir and can't be reliably redirected
    /// mid-process on Windows.
    fn temp_chat_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boundless-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn append_load_roundtrip() {
        let dir = temp_chat_dir();
        let id = create_session();
        append_message_in(&dir, &id, &ChatMessage::user("你好")).expect("append 1");
        append_message_in(&dir, &id, &ChatMessage::assistant("你好！")).expect("append 2");
        let msgs = load_messages_in(&dir, &id).expect("load");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], ChatMessage::user("你好"));
        assert_eq!(msgs[1], ChatMessage::assistant("你好！"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_sessions_newest_first() {
        let dir = temp_chat_dir();
        let a = create_session();
        append_message_in(&dir, &a, &ChatMessage::user("第一条消息")).unwrap();
        // small delay so b has a strictly later mtime
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = create_session();
        append_message_in(&dir, &b, &ChatMessage::user("第二条消息")).unwrap();

        let sessions = list_sessions_in(&dir);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, b, "newest first");
        assert_eq!(sessions[1].id, a);
        assert!(sessions[0].preview.contains("第二条"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_session_removes_file() {
        let dir = temp_chat_dir();
        let id = create_session();
        append_message_in(&dir, &id, &ChatMessage::user("x")).unwrap();
        assert!(dir.join(format!("{id}.jsonl")).exists());
        delete_session_in(&dir, &id).unwrap();
        assert!(!dir.join(format!("{id}.jsonl")).exists());
        // deleting again is a no-op
        delete_session_in(&dir, &id).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_session_is_empty() {
        let dir = temp_chat_dir();
        let msgs = load_messages_in(&dir, "nonexistent-id").expect("should not error");
        assert!(msgs.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_preview_handles_multiline_and_length() {
        assert_eq!(truncate_preview("短"), "短");
        assert_eq!(truncate_preview("a\nb\nc"), "a b c");
        let long = "x".repeat(100);
        let p = truncate_preview(&long);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
    }
}
