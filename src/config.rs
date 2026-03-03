use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub name: String,
    pub key: String,
}

fn default_probe_interval() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchSettings {
    /// Extra arguments to pass to claude (e.g. ["--model", "opus"])
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Whether --dangerously-skip-permissions is enabled
    #[serde(default)]
    pub dangerous_mode: bool,
    /// Preferred terminal emulator (auto-detect if None)
    #[serde(default)]
    pub terminal: Option<String>,
    /// Override path to the claude binary
    #[serde(default)]
    pub claude_bin: Option<String>,
    /// Probe interval in seconds for tray mode
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
}

impl Default for LaunchSettings {
    fn default() -> Self {
        Self {
            launch_args: Vec::new(),
            dangerous_mode: false,
            terminal: None,
            claude_bin: None,
            probe_interval_secs: default_probe_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub tokens: Vec<Token>,
    #[serde(default)]
    pub settings: LaunchSettings,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config")
            });
        Ok(base.join("tokeman").join("tokens.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: Config =
            toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn add_token(&mut self, name: String, key: String) {
        self.tokens.retain(|t| t.name != name);
        self.tokens.push(Token { name, key });
    }

    pub fn remove_token(&mut self, name: &str) -> bool {
        let before = self.tokens.len();
        self.tokens.retain(|t| t.name != name);
        self.tokens.len() < before
    }
}
