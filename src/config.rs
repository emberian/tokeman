use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub name: String,
    pub key: String,
    /// Optional profile-scoped OAuth access token used only for the read-only
    /// `/api/oauth/usage` endpoint. Claude setup tokens generally lack the
    /// `user:profile` scope required to expose model-specific weekly buckets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_key: Option<String>,
}

fn default_probe_interval() -> u64 {
    30
}

fn default_normal_min_5h() -> f64 {
    0.10
}

fn default_normal_min_7d() -> f64 {
    0.05
}

fn default_sip_min_5h() -> f64 {
    0.02
}

fn default_sip_min_7d() -> f64 {
    0.03
}

fn default_normal_rotation_interval() -> u64 {
    120
}

fn default_sip_rotation_interval() -> u64 {
    20
}

/// Policy for hot-rotating Claude Code's shared OAuth token.
///
/// Values are fractions of capacity *remaining*, not utilization. Once no token
/// satisfies both normal floors, tokeman enters sip-and-drain mode and samples
/// more frequently while allowing each token to drain to the lower floors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationSettings {
    #[serde(default = "default_normal_min_5h")]
    pub normal_min_5h_remaining: f64,
    #[serde(default = "default_normal_min_7d")]
    pub normal_min_7d_remaining: f64,
    #[serde(default = "default_sip_min_5h")]
    pub sip_min_5h_remaining: f64,
    #[serde(default = "default_sip_min_7d")]
    pub sip_min_7d_remaining: f64,
    #[serde(default = "default_normal_rotation_interval")]
    pub normal_probe_interval_secs: u64,
    #[serde(default = "default_sip_rotation_interval")]
    pub sip_probe_interval_secs: u64,
}

impl Default for RotationSettings {
    fn default() -> Self {
        Self {
            normal_min_5h_remaining: default_normal_min_5h(),
            normal_min_7d_remaining: default_normal_min_7d(),
            sip_min_5h_remaining: default_sip_min_5h(),
            sip_min_7d_remaining: default_sip_min_7d(),
            normal_probe_interval_secs: default_normal_rotation_interval(),
            sip_probe_interval_secs: default_sip_rotation_interval(),
        }
    }
}

impl RotationSettings {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("normal_min_5h_remaining", self.normal_min_5h_remaining),
            ("normal_min_7d_remaining", self.normal_min_7d_remaining),
            ("sip_min_5h_remaining", self.sip_min_5h_remaining),
            ("sip_min_7d_remaining", self.sip_min_7d_remaining),
        ] {
            if !(0.0..1.0).contains(&value) {
                bail!("rotation.{name} must be at least 0 and less than 1");
            }
        }
        if self.sip_min_5h_remaining > self.normal_min_5h_remaining {
            bail!("rotation.sip_min_5h_remaining must not exceed the normal floor");
        }
        if self.sip_min_7d_remaining > self.normal_min_7d_remaining {
            bail!("rotation.sip_min_7d_remaining must not exceed the normal floor");
        }
        if self.normal_probe_interval_secs == 0 || self.sip_probe_interval_secs == 0 {
            bail!("rotation probe intervals must be at least one second");
        }
        Ok(())
    }
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
    #[serde(default)]
    pub rotation: RotationSettings,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
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
        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.rotation.validate()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        self.rotation.validate()?;
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            set_private_directory_permissions(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        let parent = path
            .parent()
            .context("tokeman config path has no parent directory")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
        temp.write_all(contents.as_bytes())?;
        temp.as_file().sync_all()?;
        set_private_permissions(temp.path())?;
        temp.persist(&path)
            .map_err(|e| e.error)
            .with_context(|| format!("failed to atomically replace {}", path.display()))?;
        set_private_permissions(&path)?;
        Ok(())
    }

    pub fn add_token(&mut self, name: String, key: String) {
        self.tokens.retain(|t| t.name != name);
        self.tokens.push(Token {
            name,
            key,
            usage_key: None,
        });
    }

    pub fn set_usage_key(&mut self, name: &str, key: String) -> bool {
        let Some(token) = self.tokens.iter_mut().find(|token| token.name == name) else {
            return false;
        };
        token.usage_key = Some(key);
        true
    }

    pub fn remove_token(&mut self, name: &str) -> bool {
        let before = self.tokens.len();
        self.tokens.retain(|t| t.name != name);
        self.tokens.len() < before
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
