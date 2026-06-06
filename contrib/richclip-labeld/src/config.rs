use anyhow::{Context, Result};
use directories::ProjectDirs;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const RESERVED_CHAT_COMPLETION_BODY_KEYS: &[&str] = &["model", "messages", "temperature"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub prompt: PromptConfig,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_config_path);
        if !path.exists() {
            if path == default_config_path() {
                return Ok(Self::default());
            }
            anyhow::bail!("config file not found at {path:?}");
        }

        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config at {path:?}"))?;
        toml::from_str(&text).with_context(|| format!("failed to parse config at {path:?}"))
    }

    pub fn validate(&self) -> Result<()> {
        if self.model.model.trim().is_empty() {
            anyhow::bail!("model.model must not be empty");
        }
        if self.model.timeout_seconds == 0 {
            anyhow::bail!("model.timeout_seconds must be greater than 0");
        }
        if self.prompt.label.trim().is_empty() {
            anyhow::bail!("prompt.label must not be empty");
        }

        let url = Url::parse(&self.model.url)
            .with_context(|| format!("invalid model.url: {}", self.model.url))?;
        match url.scheme() {
            "http" | "https" => {}
            scheme => anyhow::bail!("model.url must use http or https, got {scheme}"),
        }

        self.model.validate_extra_body()?;
        Ok(())
    }
}

pub fn default_config_path() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("richclip").join("labeld.toml");
    }

    if let Some(dirs) = ProjectDirs::from("", "", "richclip") {
        return dirs.config_dir().join("labeld.toml");
    }

    PathBuf::from("richclip-labeld.toml")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub url: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub extra_body: Map<String, Value>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:11434/v1".to_string(),
            model: "your-vision-model".to_string(),
            timeout_seconds: 60,
            api_key: None,
            api_key_env: None,
            extra_body: Map::new(),
        }
    }
}

impl ModelConfig {
    pub fn resolved_api_key(&self) -> Option<String> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.api_key_env
                    .as_deref()
                    .and_then(|name| env::var(name).ok())
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
    }

    pub fn validate_extra_body(&self) -> Result<()> {
        if let Some(key) = self
            .extra_body
            .keys()
            .find(|key| RESERVED_CHAT_COMPLETION_BODY_KEYS.contains(&key.as_str()))
        {
            anyhow::bail!(
                "model.extra_body must not override reserved chat-completion field {key:?}"
            );
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    pub label: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            label: "Return one short factual label for this clipboard image. Use plain text only, no quotes, no prefixes, and no trailing sentence punctuation.".to_string(),
        }
    }
}
