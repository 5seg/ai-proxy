use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    #[serde(default = "default_upstream_base_url")]
    pub upstream_base_url: String,
    #[serde(default)]
    pub upstream_api_key: String,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub id: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        if config.models.is_empty() {
            bail!("config {} must define at least one model", path.display());
        }
        Ok(config)
    }
}

fn default_listen() -> String {
    "0.0.0.0:8787".to_string()
}

fn default_db_path() -> PathBuf {
    match env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".local/share/ai-proxy/usage.db"),
        None => PathBuf::from("ai-proxy.db"),
    }
}

fn default_upstream_base_url() -> String {
    "https://api.commandcode.ai/provider/v1".to_string()
}
