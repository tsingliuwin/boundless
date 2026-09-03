//! Scenario skills: per-scene composition specs loaded as `SKILL.md` files
//! (WorkBuddy / Agent-Skills compatible: YAML frontmatter + markdown body).
//!
//! The system prompt stays scene-agnostic; each request gets a short catalog
//! of available skills (name + description with trigger words). The model
//! routes the user's request itself, loads the matching spec via the
//! `use_skill` tool, and follows it. Adding a scene therefore means adding a
//! `SKILL.md` — no Rust changes.
//!
//! Sources, in increasing precedence (same `name` overwrites):
//! 1. Built-ins compiled in via `include_str!` (skills/ at the crate root) so
//!    the packaged app works standalone.
//! 2. `<exe_dir>/skills/*/SKILL.md` — side-by-side additions for a build.
//! 3. `~/.boundless/skills/*/SKILL.md` — user-installed skills (the future
//!    "skill market" import target).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// One parsed skill: frontmatter metadata plus the instruction body.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub display_name: String,
    /// One-line capability blurb; carries the trigger words the model routes
    /// on (e.g. "用户要求黑板报、板报、海报、宣传版面时使用：…").
    pub description: String,
    pub version: String,
    /// The markdown body after the frontmatter — the actual composition spec
    /// returned by `use_skill` and injected into the runtime context.
    pub body: String,
}

impl Skill {
    /// Parse one SKILL.md (`---` frontmatter lines, a closing `---`, then the
    /// markdown body). Returns `Err` when the block is malformed or the two
    /// fields the router needs (`name`, `description`) are missing.
    pub fn parse(raw: &str) -> Result<Skill, String> {
        let raw = raw.trim_start_matches('\u{feff}').trim_start();
        let mut lines = raw.lines();
        match lines.next() {
            Some(first) if first.trim() == "---" => {}
            _ => return Err("缺少 frontmatter 块（应以 --- 开头）".into()),
        }
        let mut fm: Vec<&str> = Vec::new();
        let mut body_lines: Vec<&str> = Vec::new();
        let mut in_frontmatter = true;
        let mut closed = false;
        for line in lines {
            if in_frontmatter {
                if line.trim() == "---" {
                    in_frontmatter = false;
                    closed = true;
                } else {
                    fm.push(line);
                }
            } else {
                body_lines.push(line);
            }
        }
        if !closed {
            return Err("frontmatter 未闭合（缺少结束 ---）".into());
        }

        let mut name = None;
        let mut display_name = None;
        let mut description = None;
        let mut version = None;
        for line in fm {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split on the first ':'; the value may itself contain colons
            // (Chinese text with colors, coordinates, …).
            let (key, value) = match line.split_once(':') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => continue,
            };
            match key {
                "name" => name = Some(value.to_string()),
                "display_name" => display_name = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                "version" => version = Some(value.to_string()),
                _ => {} // category / author / allowed-tools … are metadata only
            }
        }
        let name = name.filter(|s| !s.is_empty()).ok_or("缺少 name 字段")?;
        let description = description
            .filter(|s| !s.is_empty())
            .ok_or("缺少 description 字段（模型靠它路由）")?;
        Ok(Skill {
            name,
            display_name: display_name.unwrap_or_default(),
            description,
            version: version.unwrap_or_else(|| "0.0.0".into()),
            body: body_lines.join("\n").trim().to_string(),
        })
    }
}

/// Built-in skill files, compiled in (the label must match the frontmatter).
const BUILTIN_SOURCES: &[(&str, &str)] = &[
    ("blackboard-poster", include_str!("../../skills/blackboard-poster/SKILL.md")),
    ("ink-wash-landscape", include_str!("../../skills/ink-wash-landscape/SKILL.md")),
    ("mindmap", include_str!("../../skills/mindmap/SKILL.md")),
    ("slides", include_str!("../../skills/slides/SKILL.md")),
];

/// Parse a SKILL.md file, tagging errors with the file path.
fn parse_file(path: &PathBuf) -> Result<Skill, String> {
    let label = path.display().to_string();
    std::fs::read_to_string(path)
        .map_err(|e| format!("{label}: {e}"))
        .and_then(|raw| Skill::parse(&raw).map_err(|e| format!("{label}: {e}")))
}

/// Scan one directory for `<name>/SKILL.md` and insert each into `out`,
/// overwriting on a name collision. Invalid files are reported on stderr and
/// skipped — one broken skill must not take down the agent.
fn scan_dir(dir: &PathBuf, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("SKILL.md"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    for path in found {
        match parse_file(&path) {
            Ok(skill) => {
                out.retain(|s| s.name != skill.name);
                out.push(skill);
            }
            Err(e) => eprintln!("[skills] 跳过无效技能：{e}"),
        }
    }
}

/// Load all skills: built-ins first, then the two external directories
/// (later sources win on name collisions).
pub fn load_all() -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for (label, raw) in BUILTIN_SOURCES {
        match Skill::parse(raw) {
            Ok(s) => {
                debug_assert_eq!(s.name, *label, "内置技能文件名与 frontmatter 不一致");
                out.retain(|s| s.name != *label);
                out.push(s);
            }
            Err(e) => eprintln!("[skills] 内置技能 {label} 解析失败：{e}"),
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("skills")));
    let user_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".boundless")
        .join("skills");
    for dir in [exe_dir, Some(user_dir)].into_iter().flatten() {
        scan_dir(&dir, &mut out);
    }
    out
}

/// Look a skill up by name across all sources.
pub fn find(name: &str) -> Option<Skill> {
    load_all().into_iter().find(|s| s.name == name)
}

/// The catalog injected into the system prompt: one line per skill. The model
/// reads the trigger words in `description` and decides whether to call
/// `use_skill` for the full spec.
pub fn catalog() -> String {
    load_all()
        .iter()
        .map(|s| {
            if s.display_name.is_empty() {
                format!("- **{}**：{}\n", s.name, s.description)
            } else {
                format!("- **{}**（{}）：{}\n", s.name, s.display_name, s.description)
            }
        })
        .collect()
}

/// The full system prompt: the scene-agnostic core plus the skill-routing
/// section and catalog.
pub fn system_prompt() -> String {
    format!(
        "{}\n\n## 场景技能库（按需加载的构图规范）\n\
        以下是可用的场景技能清单。用户请求命中某个技能的适用场景时：\n\
        1. **第一步先调用 use_skill(name) 加载该场景的完整构图规范**，再开始绘制；\n\
        2. 严格按规范执行（规范里的坐标、配色、字号是硬约束）；\n\
        3. 未命中任何技能时不要调用 use_skill，直接按本提示的通用规则绘制；\n\
        4. 若运行时上下文已标注「当前活动技能规范」，规范已就位，直接按其绘制，无需重复加载。\n\n{}",
        crate::ai::agent::SYSTEM_PROMPT,
        catalog()
    )
}

/// Cross-turn handle to the currently active skill, shared between the panel
/// (which reads it to prepend the spec to the next turn's runtime context —
/// the chat history does not carry tool results) and the `use_skill` tool
/// (which writes it when the model loads a spec).
#[derive(Clone, Default)]
pub struct ActiveSkill(Arc<Mutex<Option<String>>>);

impl ActiveSkill {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the skill activated this turn (by the `use_skill` tool).
    pub fn set(&self, name: &str) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(name.to_string());
        }
    }

    /// The most recently activated skill name, if any.
    pub fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|s| s.clone())
    }

    /// Drop the activation (new session / canvas reset).
    pub fn clear(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: demo\nversion: 1.0.0\nauthor: boundless\ndescription: 用户要求流程图、架构图时使用：含中文冒号与颜色 0xRRGGBB。\n---\n\n# 规范正文\n\n1. 第一步……\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let s = Skill::parse(SAMPLE).unwrap();
        assert_eq!(s.name, "demo");
        assert_eq!(s.version, "1.0.0");
        // description survives colons inside the value
        assert!(s.description.contains("0xRRGGBB"));
        assert!(s.body.starts_with("# 规范正文"));
        assert!(s.body.contains("第一步"));
    }

    #[test]
    fn rejects_missing_frontmatter_or_fields() {
        assert!(Skill::parse("没有 frontmatter").is_err());
        assert!(Skill::parse("---\ndescription: 无 name\n---\n正文").is_err());
        assert!(Skill::parse("---\nname: x\n---\n正文").is_err());
        assert!(Skill::parse("---\nname: x\n未闭合").is_err());
    }

    #[test]
    fn builtin_skills_parse_and_catalog_is_nonempty() {
        let all = load_all();
        assert!(all.len() >= 4, "至少包含四个内置技能");
        for name in [
            "blackboard-poster",
            "ink-wash-landscape",
            "mindmap",
            "slides",
        ] {
            let s = find(name).unwrap_or_else(|| panic!("缺少内置技能 {name}"));
            assert!(!s.body.is_empty());
            assert!(s.description.contains("用户要求"));
        }
        let cat = catalog();
        for name in [
            "**mindmap**",
            "**blackboard-poster**",
            "**ink-wash-landscape**",
            "**slides**",
        ] {
            assert!(cat.contains(name), "catalog 缺少 {name}");
        }
    }

    #[test]
    fn system_prompt_carries_routing_rules() {
        let p = system_prompt();
        assert!(p.contains("use_skill"));
        assert!(p.contains("场景技能库"));
        // The catalog is embedded.
        assert!(p.contains("**mindmap**"));
    }

    #[test]
    fn active_skill_roundtrip() {
        let a = ActiveSkill::new();
        assert_eq!(a.get(), None);
        a.set("mindmap");
        assert_eq!(a.get().as_deref(), Some("mindmap"));
        a.clear();
        assert_eq!(a.get(), None);
    }
}
