use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const ENV_PREFIX: &str = "AI_PROXY_";

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
    #[serde(default = "default_models")]
    pub models: Vec<ModelConfig>,
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,
    #[serde(default = "default_capture_ttl_hours")]
    pub capture_ttl_hours: i64,
    #[serde(default = "default_capture_max_bytes")]
    pub capture_max_bytes: usize,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_read_timeout_secs")]
    pub read_timeout_secs: u64,
    #[serde(default = "default_models_ttl_secs")]
    pub models_ttl_secs: u64,
    /// この IP から来たリクエストだけ X-Real-IP / X-Forwarded-For を信じる。
    /// nginx などの手前のプロキシを並べる。
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub id: String,
}

pub struct Setting {
    pub name: String,
    pub value: String,
    pub source: &'static str,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        load(path).map(|(config, _)| config)
    }

    pub fn describe(path: &Path) -> Result<Vec<Setting>> {
        load(path).map(|(_, settings)| settings)
    }
}

fn load(path: &Path) -> Result<(Config, Vec<Setting>)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let document: Option<toml::Value> = match &text {
        Some(text) => Some(
            toml::from_str(text).with_context(|| format!("failed to parse {}", path.display()))?,
        ),
        None => None,
    };
    let mut config: Config = match &text {
        Some(text) => {
            toml::from_str(text).with_context(|| format!("failed to parse {}", path.display()))?
        }
        None => Config {
            listen: default_listen(),
            db_path: default_db_path(),
            upstream_base_url: default_upstream_base_url(),
            upstream_api_key: String::new(),
            models: default_models(),
            retention_days: default_retention_days(),
            capture_ttl_hours: default_capture_ttl_hours(),
            capture_max_bytes: default_capture_max_bytes(),
            max_body_bytes: default_max_body_bytes(),
            connect_timeout_secs: default_connect_timeout_secs(),
            read_timeout_secs: default_read_timeout_secs(),
            models_ttl_secs: default_models_ttl_secs(),
            trusted_proxies: default_trusted_proxies(),
        },
    };

    let mut sources: Vec<(String, &'static str)> = Vec::new();
    let in_file = |key: &str| {
        document
            .as_ref()
            .and_then(|document| document.get(key))
            .is_some()
    };

    override_string(&mut config.listen, "LISTEN", "listen", in_file("listen"), &mut sources)?;
    if let Some(value) = env::var_os("AI_PROXY_DB_PATH") {
        config.db_path = PathBuf::from(value);
        sources.push(("db_path".to_string(), "env"));
    } else if in_file("db_path") {
        sources.push(("db_path".to_string(), "file"));
    }
    override_string(
        &mut config.upstream_base_url,
        "UPSTREAM_BASE_URL",
        "upstream_base_url",
        in_file("upstream_base_url"),
        &mut sources,
    )?;
    override_string(
        &mut config.upstream_api_key,
        "UPSTREAM_API_KEY",
        "upstream_api_key",
        in_file("upstream_api_key"),
        &mut sources,
    )?;
    if let Some(value) = env_parse::<String>("AI_PROXY_MODELS")? {
        config.models = value
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| ModelConfig {
                id: id.to_string(),
            })
            .collect();
        sources.push(("models".to_string(), "env"));
    }
    override_parsed(
        &mut config.retention_days,
        "RETENTION_DAYS",
        "retention_days",
        in_file("retention_days"),
        &mut sources,
    )?;
    override_parsed(
        &mut config.capture_ttl_hours,
        "CAPTURE_TTL_HOURS",
        "capture_ttl_hours",
        in_file("capture_ttl_hours"),
        &mut sources,
    )?;
    override_parsed(
        &mut config.capture_max_bytes,
        "CAPTURE_MAX_BYTES",
        "capture_max_bytes",
        in_file("capture_max_bytes"),
        &mut sources,
    )?;
    override_parsed(
        &mut config.max_body_bytes,
        "MAX_BODY_BYTES",
        "max_body_bytes",
        in_file("max_body_bytes"),
        &mut sources,
    )?;
    override_parsed(
        &mut config.connect_timeout_secs,
        "CONNECT_TIMEOUT_SECS",
        "connect_timeout_secs",
        in_file("connect_timeout_secs"),
        &mut sources,
    )?;
    override_parsed(
        &mut config.read_timeout_secs,
        "READ_TIMEOUT_SECS",
        "read_timeout_secs",
        in_file("read_timeout_secs"),
        &mut sources,
    )?;
    override_parsed(
        &mut config.models_ttl_secs,
        "MODELS_TTL_SECS",
        "models_ttl_secs",
        in_file("models_ttl_secs"),
        &mut sources,
    )?;
    if let Some(value) = env_parse::<String>("AI_PROXY_TRUSTED_PROXIES")? {
        config.trusted_proxies = split_list(&value);
        sources.push(("trusted_proxies".to_string(), "env"));
    } else if in_file("trusted_proxies") {
        sources.push(("trusted_proxies".to_string(), "file"));
    }

    if config.models.is_empty() {
        bail!("at least one model must be configured");
    }
    if config.retention_days < 1 {
        bail!("retention_days must be at least 1");
    }
    if config.capture_ttl_hours < 1 {
        bail!("capture_ttl_hours must be at least 1");
    }
    if config.max_body_bytes < 1024 {
        bail!("max_body_bytes must be at least 1024");
    }
    for proxy in &config.trusted_proxies {
        if proxy.parse::<std::net::IpAddr>().is_err() {
            bail!("trusted_proxies entry is not an IP address: {proxy}");
        }
    }

    let settings = vec![
        Setting {
            name: "listen".to_string(),
            value: config.listen.clone(),
            source: source_of("listen", &sources),
        },
        Setting {
            name: "db_path".to_string(),
            value: config.db_path.display().to_string(),
            source: source_of("db_path", &sources),
        },
        Setting {
            name: "upstream_base_url".to_string(),
            value: config.upstream_base_url.clone(),
            source: source_of("upstream_base_url", &sources),
        },
        Setting {
            name: "upstream_api_key".to_string(),
            value: mask(&config.upstream_api_key),
            source: source_of("upstream_api_key", &sources),
        },
        Setting {
            name: "models".to_string(),
            value: config
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            source: source_of("models", &sources),
        },
        Setting {
            name: "retention_days".to_string(),
            value: config.retention_days.to_string(),
            source: source_of("retention_days", &sources),
        },
        Setting {
            name: "capture_ttl_hours".to_string(),
            value: config.capture_ttl_hours.to_string(),
            source: source_of("capture_ttl_hours", &sources),
        },
        Setting {
            name: "capture_max_bytes".to_string(),
            value: config.capture_max_bytes.to_string(),
            source: source_of("capture_max_bytes", &sources),
        },
        Setting {
            name: "max_body_bytes".to_string(),
            value: config.max_body_bytes.to_string(),
            source: source_of("max_body_bytes", &sources),
        },
        Setting {
            name: "connect_timeout_secs".to_string(),
            value: config.connect_timeout_secs.to_string(),
            source: source_of("connect_timeout_secs", &sources),
        },
        Setting {
            name: "read_timeout_secs".to_string(),
            value: config.read_timeout_secs.to_string(),
            source: source_of("read_timeout_secs", &sources),
        },
        Setting {
            name: "models_ttl_secs".to_string(),
            value: config.models_ttl_secs.to_string(),
            source: source_of("models_ttl_secs", &sources),
        },
        Setting {
            name: "trusted_proxies".to_string(),
            value: config.trusted_proxies.join(","),
            source: source_of("trusted_proxies", &sources),
        },
    ];

    Ok((config, settings))
}

fn source_of(name: &str, sources: &[(String, &'static str)]) -> &'static str {
    sources
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, source)| *source)
        .unwrap_or("default")
}

fn override_string(
    slot: &mut String,
    env_name: &str,
    field: &str,
    in_file: bool,
    sources: &mut Vec<(String, &'static str)>,
) -> Result<()> {
    if let Some(value) = env_parse::<String>(&format!("{ENV_PREFIX}{env_name}"))? {
        *slot = value;
        sources.push((field.to_string(), "env"));
    } else if in_file {
        sources.push((field.to_string(), "file"));
    }
    Ok(())
}

fn override_parsed<T: std::str::FromStr>(
    slot: &mut T,
    env_name: &str,
    field: &str,
    in_file: bool,
    sources: &mut Vec<(String, &'static str)>,
) -> Result<()>
where
    T::Err: std::fmt::Display,
{
    if let Some(value) = env_parse::<T>(&format!("{ENV_PREFIX}{env_name}"))? {
        *slot = value;
        sources.push((field.to_string(), "env"));
    } else if in_file {
        sources.push((field.to_string(), "file"));
    }
    Ok(())
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<T>()
                .map(Some)
                .map_err(|err| anyhow::anyhow!("invalid value in {name}: {err}"))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} is not valid UTF-8"),
    }
}

fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let head = chars.first().copied();
    let tail: String = match chars.len() {
        0 => String::new(),
        1..=8 => chars[chars.len() - 1].to_string(),
        _ => chars[chars.len() - 4..].iter().collect(),
    };
    match head {
        None => "(unset)".to_string(),
        Some(first) => format!("{first}...{tail} ({} chars)", chars.len()),
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

fn default_models() -> Vec<ModelConfig> {
    vec![ModelConfig {
        id: "deepseek/deepseek-v4.1-flash".to_string(),
    }]
}

fn default_retention_days() -> i64 {
    30
}

fn default_capture_ttl_hours() -> i64 {
    24
}

fn default_capture_max_bytes() -> usize {
    64 * 1024
}

fn default_max_body_bytes() -> usize {
    64 * 1024 * 1024
}

fn default_connect_timeout_secs() -> u64 {
    10
}

fn default_read_timeout_secs() -> u64 {
    300
}

fn default_models_ttl_secs() -> u64 {
    300
}

fn default_trusted_proxies() -> Vec<String> {
    vec!["127.0.0.1".to_string(), "::1".to_string()]
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}
