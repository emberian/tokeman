//! The `[settings]` and `[rotation]` tables of tokens.toml.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// How `tokeman launch` and the trays start Claude.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchSettings {
    /// Extra arguments for claude, e.g. `["--model", "opus"]`.
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Add `--dangerously-skip-permissions`.
    #[serde(default)]
    pub dangerous_mode: bool,
    /// Terminal emulator to open; auto-detected when unset.
    #[serde(default)]
    pub terminal: Option<String>,
    /// Path to the claude binary; `claude` on PATH when unset.
    #[serde(default)]
    pub claude_bin: Option<String>,
    /// Probe interval for the trays and the TUI.
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
}

fn default_probe_interval() -> u64 {
    30
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

impl LaunchSettings {
    /// Every key `tokeman settings set` accepts.
    pub const KEYS: [&'static str; 5] = [
        "launch_args",
        "dangerous_mode",
        "terminal",
        "claude_bin",
        "probe_interval_secs",
    ];

    /// The binary and arguments to launch with. `TOKEMAN_CLAUDE_BIN` overrides
    /// the configured binary.
    pub fn command(&self, extra_args: impl IntoIterator<Item = String>) -> (String, Vec<String>) {
        let binary = std::env::var("TOKEMAN_CLAUDE_BIN")
            .ok()
            .or_else(|| self.claude_bin.clone())
            .unwrap_or_else(|| "claude".into());
        let mut args = self.launch_args.clone();
        args.extend(extra_args);
        const SKIP: &str = "--dangerously-skip-permissions";
        if self.dangerous_mode && !args.iter().any(|arg| arg == SKIP) {
            args.push(SKIP.into());
        }
        (binary, args)
    }

    /// Set one field from its command-line spelling. An empty value clears
    /// `terminal` and `claude_bin` and empties `launch_args`, which is split
    /// on whitespace.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let value = value.trim();
        match key {
            "launch_args" => {
                self.launch_args = value.split_whitespace().map(str::to_owned).collect()
            }
            "dangerous_mode" => {
                self.dangerous_mode = match value {
                    "true" | "on" | "1" => true,
                    "false" | "off" | "0" => false,
                    _ => bail!("dangerous_mode takes true or false, not {value:?}"),
                }
            }
            "terminal" => self.terminal = (!value.is_empty()).then(|| value.to_owned()),
            "claude_bin" => self.claude_bin = (!value.is_empty()).then(|| value.to_owned()),
            "probe_interval_secs" => {
                self.probe_interval_secs = match value.parse() {
                    Ok(secs) if secs >= 1 => secs,
                    _ => bail!("probe_interval_secs takes a whole number of seconds, at least 1"),
                }
            }
            _ => bail!("unknown setting {key:?}; one of {}", Self::KEYS.join(", ")),
        }
        Ok(())
    }

    /// `(key, value)` pairs in the order of [`Self::KEYS`], for display.
    pub fn entries(&self) -> [(&'static str, String); 5] {
        [
            ("launch_args", self.launch_args.join(" ")),
            ("dangerous_mode", self.dangerous_mode.to_string()),
            ("terminal", self.terminal.clone().unwrap_or_default()),
            ("claude_bin", self.claude_bin.clone().unwrap_or_default()),
            ("probe_interval_secs", self.probe_interval_secs.to_string()),
        ]
    }
}

/// The rotation policy's numbers. Every floor is a fraction of capacity
/// *remaining*. See `policy` for what each one means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySettings {
    /// The default leaves at these floors when somewhere worth landing exists.
    #[serde(default = "d_normal_5h")]
    pub normal_min_5h_remaining: f64,
    #[serde(default = "d_normal_7d")]
    pub normal_min_7d_remaining: f64,
    /// With nowhere to land, the default drains in place down to these.
    #[serde(default = "d_sip_5h")]
    pub sip_min_5h_remaining: f64,
    #[serde(default = "d_sip_7d")]
    pub sip_min_7d_remaining: f64,
    /// A replacement must offer at least this much; a switch costs every
    /// live session its prompt cache and has to buy a long dwell.
    #[serde(default = "d_land_5h")]
    pub land_min_5h_remaining: f64,
    #[serde(default = "d_land_7d")]
    pub land_min_7d_remaining: f64,
    #[serde(default = "d_normal_interval")]
    pub normal_probe_interval_secs: u64,
    #[serde(default = "d_sip_interval")]
    pub sip_probe_interval_secs: u64,
    /// How often, per account without profile reads, to spend one probe on
    /// the startup model to read the limits only its responses carry. 0 disables.
    #[serde(default = "d_sample_secs")]
    pub target_model_sample_secs: u64,
}

fn d_normal_5h() -> f64 {
    0.07
}
fn d_normal_7d() -> f64 {
    0.05
}
fn d_sip_5h() -> f64 {
    0.02
}
fn d_sip_7d() -> f64 {
    0.03
}
fn d_land_5h() -> f64 {
    0.25
}
fn d_land_7d() -> f64 {
    0.10
}
fn d_normal_interval() -> u64 {
    120
}
fn d_sip_interval() -> u64 {
    20
}
fn d_sample_secs() -> u64 {
    1800
}

impl Default for PolicySettings {
    fn default() -> Self {
        Self {
            normal_min_5h_remaining: d_normal_5h(),
            normal_min_7d_remaining: d_normal_7d(),
            sip_min_5h_remaining: d_sip_5h(),
            sip_min_7d_remaining: d_sip_7d(),
            land_min_5h_remaining: d_land_5h(),
            land_min_7d_remaining: d_land_7d(),
            normal_probe_interval_secs: d_normal_interval(),
            sip_probe_interval_secs: d_sip_interval(),
            target_model_sample_secs: d_sample_secs(),
        }
    }
}

impl PolicySettings {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("normal_min_5h_remaining", self.normal_min_5h_remaining),
            ("normal_min_7d_remaining", self.normal_min_7d_remaining),
            ("sip_min_5h_remaining", self.sip_min_5h_remaining),
            ("sip_min_7d_remaining", self.sip_min_7d_remaining),
            ("land_min_5h_remaining", self.land_min_5h_remaining),
            ("land_min_7d_remaining", self.land_min_7d_remaining),
        ] {
            if !(0.0..1.0).contains(&value) {
                bail!("rotation.{name} must be at least 0 and less than 1");
            }
        }
        if self.sip_min_5h_remaining > self.normal_min_5h_remaining
            || self.sip_min_7d_remaining > self.normal_min_7d_remaining
        {
            bail!("rotation.sip_min_* must not exceed the normal floors");
        }
        if self.land_min_5h_remaining < self.normal_min_5h_remaining
            || self.land_min_7d_remaining < self.normal_min_7d_remaining
        {
            bail!("rotation.land_min_* must not be below the normal floors");
        }
        if self.normal_probe_interval_secs == 0 || self.sip_probe_interval_secs == 0 {
            bail!("rotation probe intervals must be at least one second");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_parses_each_key_and_rejects_the_rest() {
        let mut settings = LaunchSettings::default();
        settings.set("launch_args", "  --model opus  ").unwrap();
        assert_eq!(settings.launch_args, ["--model", "opus"]);
        settings.set("launch_args", "").unwrap();
        assert!(settings.launch_args.is_empty());
        settings.set("dangerous_mode", "true").unwrap();
        assert!(settings.dangerous_mode);
        assert!(settings.set("dangerous_mode", "maybe").is_err());
        settings.set("terminal", "iTerm2").unwrap();
        assert_eq!(settings.terminal.as_deref(), Some("iTerm2"));
        settings.set("terminal", "").unwrap();
        assert_eq!(settings.terminal, None);
        settings.set("probe_interval_secs", "45").unwrap();
        assert_eq!(settings.probe_interval_secs, 45);
        assert!(settings.set("probe_interval_secs", "0").is_err());
        assert!(settings.set("colour", "red").is_err());
    }

    #[test]
    fn dangerous_mode_adds_the_flag_once() {
        let settings = LaunchSettings {
            dangerous_mode: true,
            launch_args: vec!["--dangerously-skip-permissions".into()],
            ..LaunchSettings::default()
        };
        let (_, args) = settings.command(["-p".to_string()]);
        assert_eq!(args, ["--dangerously-skip-permissions", "-p"]);
    }

    #[test]
    fn floors_must_nest() {
        let mut policy = PolicySettings::default();
        assert!(policy.validate().is_ok());
        policy.sip_min_5h_remaining = 0.5;
        assert!(policy.validate().is_err());
        policy = PolicySettings {
            land_min_7d_remaining: 0.01,
            ..PolicySettings::default()
        };
        assert!(policy.validate().is_err());
    }
}
