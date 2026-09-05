//! Persistent storage for AI conversations.
//!
//! Chat sessions live under the *workspace* data dir (see
//! `crate::workspace`): `<workspace>/.boundless/chat/<session-id>.jsonl`, one
//! `ChatMessage` per line (JSONL). Append-only writes are O(1) and survive
//! partial writes: at worst the last line is truncated, never the whole file.
//! The first line may be a session header binding the conversation to a
//! board; message parsing skips it (it isn't a `ChatMessage`).
//!
//! App-level files stay under `~/.boundless/`: `config.json` (provider
//! settings), `app.json` (workspace selection), panic/agent logs.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::client::ChatMessage;

/// Maximum chars of a message used as the session list preview.
const PREVIEW_CHARS: usize = 30;

/// Root data directory: `~/.boundless`. Created on first access. Holds only
/// app-level state — the session store follows the active workspace instead.
pub fn data_dir() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".boundless")
}

/// The active workspace's data dir. `None` = no workspace active (tests,
/// early startup) and the legacy global dir is used.
static WORKSPACE_DATA_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Route the session store into a workspace data dir (`<ws>/.boundless`).
/// Call at startup and on workspace switches.
pub fn set_workspace_data_dir(dir: Option<PathBuf>) {
    *WORKSPACE_DATA_DIR.write().unwrap() = dir;
}

/// Data dir the chat sessions live under: the workspace's `.boundless` when a
/// workspace is active, else the global `~/.boundless`.
pub fn workspace_data_dir() -> PathBuf {
    WORKSPACE_DATA_DIR
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(data_dir)
}

/// Chat sessions directory: `<workspace-data>/chat`.
pub fn chat_dir() -> PathBuf {
    workspace_data_dir().join("chat")
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
    /// Workspace-relative board path this conversation belongs to, when the
    /// session was started with a board open. Sessions written before boards
    /// existed (or on an untitled board) have no binding.
    pub board: Option<String>,
}

/// First-line header of a session file: binds the conversation to a board.
/// Written once, before the first message, when the session starts with a
/// board open. `load_messages` skips it (not a `ChatMessage`).
#[derive(Serialize, Deserialize)]
struct SessionHeader {
    #[serde(rename = "boundless-session")]
    board: String,
}

/// Generate a new, unused session id (UUIDv4). Does not create a file - the
/// file is created lazily when the first message is appended.
pub fn create_session() -> String {
    Uuid::new_v4().to_string()
}

/// Append one message to a session's JSONL file, creating the file (and the
/// chat directory) if necessary. Append-only, so concurrent-ish appends within
/// a single process are safe and O(1). `board` (the workspace-relative board
/// path) is persisted in the header on first write, binding the conversation
/// to that board.
pub fn append_message(id: &str, msg: &ChatMessage, board: Option<&str>) -> Result<()> {
    append_message_in(&chat_dir(), id, msg, board)
}

fn append_message_in(
    chat_dir: &std::path::Path,
    id: &str,
    msg: &ChatMessage,
    board: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(chat_dir).context("创建会话目录失败")?;
    let path = chat_dir.join(format!("{id}.jsonl"));
    // The header is written once, before the first message, so the board
    // binding survives even though appends are lazy.
    let needs_header = board.is_some() && !path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开会话文件失败: {id}"))?;
    if let Some(board) = board.filter(|_| needs_header) {
        let header = SessionHeader {
            board: board.to_string(),
        };
        writeln!(
            file,
            "{}",
            serde_json::to_string(&header).context("序列化会话头失败")?
        )
        .context("写入会话头失败")?;
    }
    let line = serde_json::to_string(msg).context("序列化消息失败")?;
    writeln!(file, "{line}").context("写入会话文件失败")?;
    Ok(())
}

/// Load all messages of a session, in order. Lines that fail to parse are
/// skipped (tolerant of a truncated final line from a crash mid-write, and of
/// the session header line).
pub fn load_messages(id: &str) -> Result<Vec<ChatMessage>> {
    load_messages_in(&chat_dir(), id)
}

fn load_messages_in(chat_dir: &std::path::Path, id: &str) -> Result<Vec<ChatMessage>> {
    load_messages_with_board_in(chat_dir, id).map(|(msgs, _)| msgs)
}

/// Messages plus the board binding from the session header (None when the
/// file has no header — legacy sessions / untitled boards).
fn load_messages_with_board_in(
    chat_dir: &std::path::Path,
    id: &str,
) -> Result<(Vec<ChatMessage>, Option<String>)> {
    let path = chat_dir.join(format!("{id}.jsonl"));
    let file = match OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(e) => return Err(e).with_context(|| format!("打开会话文件失败: {id}")),
    };
    let reader = BufReader::new(file);
    let mut board = None;
    let mut messages = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.context("读取会话文件失败")?;
        if line.trim().is_empty() {
            continue;
        }
        if i == 0 {
            // The very first line may be the session header.
            if let Ok(header) = serde_json::from_str::<SessionHeader>(&line) {
                board = Some(header.board);
                continue;
            }
        }
        match serde_json::from_str::<ChatMessage>(&line) {
            Ok(msg) => messages.push(msg),
            Err(_) => continue, // skip corrupt/partial line
        }
    }
    Ok((messages, board))
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
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let (messages, board) = load_messages_with_board_in(chat_dir, &id).unwrap_or_default();
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
            board,
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

/// Rewrite session headers after a board (or folder) rename, so conversation
/// history follows the renamed target. `prefix=false` re-points sessions
/// bound to exactly `old_rel`; `prefix=true` also matches everything under
/// the old folder path. Best-effort: failures are silent (a missed header
/// only means the old conversation stays listed under the old name bucket).
pub fn rebind_session_boards(old_rel: &str, new_rel: &str, prefix: bool) {
    rebind_session_boards_in(&chat_dir(), old_rel, new_rel, prefix)
}

fn rebind_session_boards_in(dir: &std::path::Path, old_rel: &str, new_rel: &str, prefix: bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix_marker = format!("{old_rel}/");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Some((first, rest)) = content.split_once('\n') else {
            continue;
        };
        let Ok(header) = serde_json::from_str::<SessionHeader>(first) else {
            continue;
        };
        let new_board = if prefix && header.board.starts_with(&prefix_marker) {
            format!("{new_rel}{}", &header.board[old_rel.len()..])
        } else if header.board == old_rel {
            new_rel.to_string()
        } else {
            continue;
        };
        let Ok(new_first) = serde_json::to_string(&SessionHeader { board: new_board }) else {
            continue;
        };
        let _ = fs::write(&path, format!("{new_first}\n{rest}"));
    }
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
        append_message_in(&dir, &id, &ChatMessage::user("你好"), None).expect("append 1");
        append_message_in(&dir, &id, &ChatMessage::assistant("你好！"), None).expect("append 2");
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
        append_message_in(&dir, &a, &ChatMessage::user("第一条消息"), None).unwrap();
        // small delay so b has a strictly later mtime
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = create_session();
        append_message_in(&dir, &b, &ChatMessage::user("第二条消息"), None).unwrap();

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
        append_message_in(&dir, &id, &ChatMessage::user("x"), None).unwrap();
        assert!(dir.join(format!("{id}.jsonl")).exists());
        delete_session_in(&dir, &id).unwrap();
        assert!(!dir.join(format!("{id}.jsonl")).exists());
        // deleting again is a no-op
        delete_session_in(&dir, &id).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_board_binding_roundtrip() {
        let dir = temp_chat_dir();
        // A session started with a board open: header on first append,
        // binding survives reload, header line is not a message.
        let id = create_session();
        append_message_in(
            &dir,
            &id,
            &ChatMessage::user("你好"),
            Some("docs/计划.boundless"),
        )
        .unwrap();
        append_message_in(&dir, &id, &ChatMessage::assistant("好的"), None).unwrap();
        let sessions = list_sessions_in(&dir);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].board.as_deref(), Some("docs/计划.boundless"));
        let msgs = load_messages_in(&dir, &id).unwrap();
        assert_eq!(msgs.len(), 2, "header line must not count as a message");
        assert_eq!(msgs[0].content, "你好");
        // An unbound session (untitled board) has no binding.
        let id2 = create_session();
        append_message_in(&dir, &id2, &ChatMessage::user("无白板"), None).unwrap();
        let sessions = list_sessions_in(&dir);
        let unbound = sessions.iter().find(|s| s.id == id2).unwrap();
        assert!(unbound.board.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebind_session_boards_updates_headers() {
        let dir = temp_chat_dir();
        let a = create_session();
        append_message_in(
            &dir,
            &a,
            &ChatMessage::user("第一条"),
            Some("旧文件夹/板.boundless"),
        )
        .unwrap();
        let b = create_session();
        append_message_in(
            &dir,
            &b,
            &ChatMessage::user("第二条"),
            Some("别处.boundless"),
        )
        .unwrap();

        // Folder rename with prefix: matching sessions (and only those)
        // follow the new path.
        rebind_session_boards_in(&dir, "旧文件夹", "新文件夹", true);
        let sessions = list_sessions_in(&dir);
        let sa = sessions.iter().find(|s| s.id == a).unwrap();
        let sb = sessions.iter().find(|s| s.id == b).unwrap();
        assert_eq!(sa.board.as_deref(), Some("新文件夹/板.boundless"));
        assert_eq!(sb.board.as_deref(), Some("别处.boundless"));
        // Messages survive the header rewrite.
        let msgs = load_messages_in(&dir, &a).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "第一条");

        // Exact board rename.
        rebind_session_boards_in(
            &dir,
            "新文件夹/板.boundless",
            "新文件夹/改名.boundless",
            false,
        );
        let sessions = list_sessions_in(&dir);
        let sa = sessions.iter().find(|s| s.id == a).unwrap();
        assert_eq!(sa.board.as_deref(), Some("新文件夹/改名.boundless"));
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
