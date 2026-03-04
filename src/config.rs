use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default)]
    pub harness_enabled: bool,
    #[serde(skip)]
    pub api_key: String,
}

fn default_model() -> String {
    "claude-opus-4-20250514".to_string()
}

fn default_max_iterations() -> u32 {
    50
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            max_iterations: default_max_iterations(),
            harness_enabled: false,
            api_key: String::new(),
        }
    }
}

/// Return ~/.breeze, creating it if needed.
pub fn breeze_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("Could not determine home directory")?
        .join(".breeze");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    Ok(dir)
}

/// Resolve the API key: ANTHROPIC_API_KEY env → ~/.breeze/api_key file → error.
fn resolve_api_key() -> Result<String> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }

    let key_file = breeze_dir()?.join("api_key");
    if key_file.exists() {
        let key = std::fs::read_to_string(&key_file)
            .with_context(|| format!("Failed to read {}", key_file.display()))?
            .trim()
            .to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }

    bail!(
        "No API key found. Set ANTHROPIC_API_KEY or write your key to ~/.breeze/api_key"
    )
}

/// Load config from ~/.breeze/config.toml (or defaults) and resolve the API key.
pub fn load_config() -> Result<Config> {
    let dir = breeze_dir()?;
    let config_path = dir.join("config.toml");

    let mut config: Config = if config_path.exists() {
        let text = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?
    } else {
        Config::default()
    };

    config.api_key = resolve_api_key()?;
    Ok(config)
}
