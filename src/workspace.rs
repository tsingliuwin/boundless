//! Workspace: the directory tree that holds the user's boards.
//!
//! A workspace is a plain directory. Boards are `.boundless` scene files at
//! any depth; subdirectories are the user's folder structure. Per-workspace
//! data (chat sessions) lives in `<workspace>/.boundless/`, so pointing the
//! app at a different directory moves the conversation history with it.
//!
//! The default workspace is `~/.boundless/workspace`. The active workspace
//! (and the last-open board, for resume-on-start) is persisted in
//! `~/.boundless/app.json`; global app-level config (`config.json`, agent
//! logs, panic log) stays under `~/.boundless` regardless of workspace.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::ai::store;
use crate::camera::Camera;
use crate::scene::{Scene, SceneFile};

/// Name of the per-workspace data directory (chat sessions etc.).
const DATA_DIR_NAME: &str = ".boundless";
/// Board file extension. Same format the classic 打开/保存 dialogs use.
pub const BOARD_EXT: &str = "boundless";

/// The active workspace.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

/// `~/.boundless/app.json` — cross-restart app state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AppState {
    /// Active workspace root. Absent = the default workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    /// Workspace-relative path of the board to reopen on start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_board: Option<String>,
    /// Conversation UI open on start. None (old files) = open — the
    /// conversation is on by default; only an explicit close turns it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_open: Option<bool>,
    /// Conversation display mode: true = compact bottom bar, false = docked
    /// panel. None = compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_compact: Option<bool>,
}

fn app_state_path() -> PathBuf {
    store::data_dir().join("app.json")
}

/// One entry of the explorer tree.
#[derive(Clone, Debug)]
pub enum Node {
    /// A subdirectory of the workspace (never the data dir).
    Folder {
        name: String,
        rel: String,
        children: Vec<Node>,
    },
    /// A `.boundless` board file. `name` is the file stem (display name),
    /// `rel` the workspace-relative path with `/` separators.
    Board { name: String, rel: String },
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Node::Folder { name, .. } | Node::Board { name, .. } => name,
        }
    }

    pub fn rel(&self) -> &str {
        match self {
            Node::Folder { rel, .. } | Node::Board { rel, .. } => rel,
        }
    }
}

impl Workspace {
    /// Wrap a directory as a workspace (used by workspace switching; startup
    /// goes through [`Workspace::load`] instead).
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// The default workspace root: `~/.boundless/workspace`.
    pub fn default_root() -> PathBuf {
        store::data_dir().join("workspace")
    }

    /// Load the persisted app state and resolve the active workspace.
    /// Falls back to the default workspace when nothing (valid) is stored.
    pub fn load() -> Self {
        let stored = fs::read_to_string(app_state_path())
            .ok()
            .and_then(|json| serde_json::from_str::<AppState>(&json).ok());
        let custom = stored
            .as_ref()
            .and_then(|s| s.workspace.clone())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let is_default = custom.is_none();
        let root = custom.unwrap_or_else(Self::default_root);
        if is_default {
            Self::migrate_legacy_chat(&root);
        }
        Self { root }
    }

    /// One-time carry-over: conversations written before workspaces existed
    /// live in the global `~/.boundless/chat`. When starting on the default
    /// workspace, copy them into the workspace data dir (only when it holds
    /// no sessions yet) so an upgrade doesn't look like lost history.
    fn migrate_legacy_chat(root: &Path) {
        let legacy = store::data_dir().join("chat");
        let target = root.join(DATA_DIR_NAME).join("chat");
        let has_files = |dir: &Path| {
            fs::read_dir(dir)
                .map(|mut d| d.any(|e| e.map(|e| e.path().is_file()).unwrap_or(false)))
                .unwrap_or(false)
        };
        if !has_files(&legacy) || has_files(&target) {
            return;
        }
        if fs::create_dir_all(&target).is_err() {
            return;
        }
        if let Ok(entries) = fs::read_dir(&legacy) {
            for entry in entries.flatten() {
                let from = entry.path();
                if from.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(name) = from.file_name() {
                        let _ = fs::copy(&from, target.join(name));
                    }
                }
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Per-workspace data directory: `<root>/.boundless`.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join(DATA_DIR_NAME)
    }

    /// Make this workspace active: route the session store into
    /// `<root>/.boundless`, create the directories, and persist the selection
    /// in app.json. Re-activating the same workspace keeps its stored
    /// `last_board`; switching resets it. Call once at startup and on every
    /// workspace switch.
    pub fn activate(&self) -> Result<()> {
        fs::create_dir_all(self.data_dir().join("chat"))
            .with_context(|| format!("创建工作区数据目录失败: {}", self.data_dir().display()))?;
        store::set_workspace_data_dir(Some(self.data_dir()));
        let mut state = Self::read_app_state();
        let same = state.workspace.as_deref() == Some(self.root.to_string_lossy().as_ref());
        if !same {
            state.last_board = None;
        }
        state.workspace = Some(self.root.to_string_lossy().to_string());
        Self::write_app_state(&state);
        Ok(())
    }

    /// Remember the board to reopen on the next start. Best-effort: failures
    /// are silent (the field is a convenience, not data).
    pub fn set_last_board(&self, rel: Option<&str>) {
        let mut state = Self::read_app_state();
        // Only meaningful together with the matching workspace.
        state.workspace = Some(self.root.to_string_lossy().to_string());
        state.last_board = rel.map(str::to_string);
        Self::write_app_state(&state);
    }

    /// The board to reopen on start, if it still exists.
    pub fn last_board(&self) -> Option<PathBuf> {
        let state = Self::read_app_state();
        let rel = state.last_board?;
        if state.workspace.as_deref() != Some(self.root.to_string_lossy().as_ref()) {
            return None;
        }
        let abs = self.root.join(&rel);
        abs.is_file().then_some(abs)
    }

    /// Conversation UI preferences: (open on start, compact mode). Both
    /// default to on/compact for files that predate the fields.
    pub fn chat_prefs(&self) -> (bool, bool) {
        let state = Self::read_app_state();
        (
            state.chat_open.unwrap_or(true),
            state.chat_compact.unwrap_or(true),
        )
    }

    /// Persist the conversation UI preferences (open state and mode).
    pub fn set_chat_prefs(&self, open: bool, compact: bool) {
        let mut state = Self::read_app_state();
        state.chat_open = Some(open);
        state.chat_compact = Some(compact);
        Self::write_app_state(&state);
    }

    fn read_app_state() -> AppState {
        fs::read_to_string(app_state_path())
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    fn write_app_state(state: &AppState) {
        if let Ok(json) = serde_json::to_string_pretty(state) {
            let _ = fs::write(app_state_path(), json);
        }
    }

    /// Scan the workspace into an explorer tree. Folders sort before boards,
    /// both case-insensitively by name. Hidden entries (leading `.`) and the
    /// data dir are skipped. Depth is unbounded; whiteboard trees are shallow.
    pub fn scan(&self) -> Vec<Node> {
        self.scan_dir(self.root(), "")
    }

    fn scan_dir(&self, abs: &Path, rel_prefix: &str) -> Vec<Node> {
        let entries = match fs::read_dir(abs) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut folders: Vec<Node> = Vec::new();
        let mut boards: Vec<Node> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let rel = if rel_prefix.is_empty() {
                name.clone()
            } else {
                format!("{rel_prefix}/{name}")
            };
            if path.is_dir() {
                let children = self.scan_dir(&path, &rel);
                folders.push(Node::Folder {
                    name,
                    rel,
                    children,
                });
            } else if path.extension().and_then(|e| e.to_str()) == Some(BOARD_EXT) {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or(name);
                boards.push(Node::Board { name, rel });
            }
        }
        let by_name = |a: &Node, b: &Node| a.name().to_lowercase().cmp(&b.name().to_lowercase());
        folders.sort_by(by_name);
        boards.sort_by(by_name);
        folders.extend(boards);
        folders
    }

    /// Rename a board file or folder (given by workspace-relative `rel`) to
    /// the bare `new_name` (no separators; boards get `.boundless` appended
    /// automatically and a trailing `.boundless` typed by the user is
    /// stripped). Returns the new absolute path. Refuses unknown targets,
    /// empty/illegal names, and conflicts.
    pub fn rename(&self, rel: &str, new_name: &str) -> Result<PathBuf> {
        let abs = self.root.join(rel);
        anyhow::ensure!(
            abs.starts_with(&self.root) && (abs.is_file() || abs.is_dir()),
            "重命名目标不存在: {rel}"
        );
        let is_dir = abs.is_dir();
        let mut name = new_name.trim().to_string();
        if !is_dir {
            // The UI shows the stem; silently accept a typed extension too.
            if let Some(stripped) = name.strip_suffix(BOARD_EXT) {
                name = stripped.strip_suffix('.').unwrap_or(stripped).to_string();
            }
        }
        anyhow::ensure!(!name.is_empty(), "名称不能为空");
        anyhow::ensure!(
            !name.starts_with('.')
                && !name
                    .chars()
                    .any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')),
            "名称含有非法字符"
        );
        let mut target_name = name;
        if !is_dir {
            target_name.push('.');
            target_name.push_str(BOARD_EXT);
        }
        let target = abs
            .parent()
            .expect("rel paths always have a parent")
            .join(&target_name);
        anyhow::ensure!(
            !target.exists(),
            "同名{}已存在",
            if is_dir { "文件夹" } else { "白板" }
        );
        fs::rename(&abs, &target)
            .with_context(|| format!("重命名失败: {rel} → {}", target_name))?;
        Ok(target)
    }

    /// Pick a free board path in `folder_rel` (None = the workspace root):
    /// `未命名.boundless`, `未命名 2.boundless`, … Used by 创建白板 and by
    /// saving an untitled board straight into the workspace.
    pub fn free_board_path(&self, folder_rel: Option<&str>) -> Result<PathBuf> {
        let dir = self.resolve_dir(folder_rel)?;
        Ok(free_path(&dir, "未命名", BOARD_EXT))
    }

    /// Create a board file with an empty scene in `folder_rel` (None = the
    /// workspace root) and return its absolute path.
    pub fn create_board(&self, folder_rel: Option<&str>) -> Result<PathBuf> {
        let path = self.free_board_path(folder_rel)?;
        let empty = SceneFile::new(&Scene::new(), Camera::default());
        let json = serde_json::to_string_pretty(&empty).context("序列化空场景失败")?;
        fs::write(&path, json).with_context(|| format!("创建白板失败: {}", path.display()))?;
        Ok(path)
    }

    /// Create a folder (collision-avoiding `新建文件夹`, `新建文件夹 2`, …)
    /// under `parent_rel` (None = the workspace root) and return its path.
    pub fn create_folder(&self, parent_rel: Option<&str>) -> Result<PathBuf> {
        let dir = self.resolve_dir(parent_rel)?;
        let path = free_path(&dir, "新建文件夹", "");
        fs::create_dir_all(&path).with_context(|| format!("创建文件夹失败: {}", path.display()))?;
        Ok(path)
    }

    /// Absolute path of a workspace-relative folder, refusing escapes and the
    /// data dir. None = the workspace root itself.
    fn resolve_dir(&self, folder_rel: Option<&str>) -> Result<PathBuf> {
        match folder_rel {
            None => Ok(self.root.clone()),
            Some(rel) => {
                let abs = self.root.join(rel);
                anyhow::ensure!(
                    abs.starts_with(&self.root)
                        && abs.is_dir()
                        && abs.file_name().map(|n| n != DATA_DIR_NAME).unwrap_or(false),
                    "非法的文件夹位置: {rel}"
                );
                Ok(abs)
            }
        }
    }
}

/// First non-conflicting path in `dir`: `base.ext`, `base 2.ext`, …
/// (empty ext for directories).
fn free_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut candidate = match ext {
        "" => dir.join(base),
        _ => dir.join(format!("{base}.{ext}")),
    };
    let mut n = 2;
    while candidate.exists() {
        candidate = match ext {
            "" => dir.join(format!("{base} {n}")),
            _ => dir.join(format!("{base} {n}.{ext}")),
        };
        n += 1;
    }
    candidate
}

/// Normalize a path under the workspace into the canonical session-binding
/// key: workspace-relative with `/` separators. None when the path is not
/// inside the workspace.
pub fn rel_key(workspace_root: &Path, abs: &Path) -> Option<String> {
    abs.strip_prefix(workspace_root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ws() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boundless-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_orders_folders_first_and_skips_data_dir() {
        let root = temp_ws();
        fs::create_dir_all(root.join("b 主题")).unwrap();
        fs::create_dir_all(root.join("A 组")).unwrap();
        fs::create_dir_all(root.join(".boundless").join("chat")).unwrap();
        fs::write(root.join("z.boundless"), "{}").unwrap();
        fs::write(root.join("a.boundless"), "{}").unwrap();
        fs::write(root.join("notes.txt"), "x").unwrap();

        let ws = Workspace { root: root.clone() };
        let nodes = ws.scan();
        assert_eq!(
            nodes.len(),
            4,
            "two folders + two boards; txt file and data dir are not listed"
        );
        assert!(
            matches!(&nodes[0], Node::Folder { name, .. } if name == "A 组"),
            "folders first, case-insensitive: {nodes:?}"
        );
        assert!(matches!(&nodes[1], Node::Folder { name, .. } if name == "b 主题"));
        assert!(matches!(&nodes[2], Node::Board { name, .. } if name == "a"));
        assert!(matches!(&nodes[3], Node::Board { name, .. } if name == "z"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn create_board_picks_free_name_and_writes_scene() {
        let root = temp_ws();
        let ws = Workspace { root: root.clone() };
        let p1 = ws.create_board(None).unwrap();
        let p2 = ws.create_board(None).unwrap();
        assert!(p1.file_name().unwrap() == "未命名.boundless");
        assert!(p2.file_name().unwrap() == "未命名 2.boundless");
        let json = fs::read_to_string(&p1).unwrap();
        assert!(SceneFile::parse(&json).is_ok(), "empty scene parses");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn create_folder_in_root_and_nested() {
        let root = temp_ws();
        let ws = Workspace { root: root.clone() };
        let f1 = ws.create_folder(None).unwrap();
        let f2 = ws.create_folder(Some("新建文件夹")).unwrap();
        assert!(f1.file_name().unwrap() == "新建文件夹");
        assert!(f2.parent().unwrap() == f1);
        assert!(f2.file_name().unwrap() == "新建文件夹");
        // Escaping rel paths are refused.
        assert!(ws.create_folder(Some("../outside")).is_err());
        assert!(ws.create_folder(Some(".boundless")).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rel_key_normalizes_separators() {
        let root = temp_ws();
        let abs = root.join("a\\b").with_extension("boundless");
        // On Windows the join above already uses `\`; on Unix `/`. Both must
        // normalize to forward slashes.
        let key = rel_key(&root, &abs).unwrap();
        assert!(!key.contains('\\'));
        assert!(key.ends_with("b.boundless"));
        assert!(rel_key(&root, Path::new("/elsewhere/x.boundless")).is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
