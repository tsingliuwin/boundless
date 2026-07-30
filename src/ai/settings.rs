//! AI provider settings: config file + environment overrides.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AiSettings {
    /// e.g. "https://api.openai.com/v1" — any OpenAI-compatible endpoint.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
        }
    }
}

fn config_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("boundless").join("config.json")
}

impl AiSettings {
    /// Load settings: config file, with environment variables taking precedence.
    pub fn load() -> Self {
        let mut settings = Self::load_from_file().unwrap_or_default();
        if let Ok(v) = std::env::var("OPENAI_BASE_URL") {
            if !v.is_empty() {
                settings.base_url = v;
            }
        }
        if let Ok(v) = std::env::var("OPENAI_API_KEY") {
            if !v.is_empty() {
                settings.api_key = v;
            }
        }
        if let Ok(v) = std::env::var("BOUNDLESS_MODEL") {
            if !v.is_empty() {
                settings.model = v;
            }
        }
        settings
    }

    fn load_from_file() -> Option<Self> {
        let path = config_path();
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joining() {
        let mut s = AiSettings::default();
        assert_eq!(
            s.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        s.base_url = "http://localhost:11434/v1/".to_string();
        assert_eq!(
            s.chat_completions_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }
}
