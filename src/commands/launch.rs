//! `tokeman <service> launch`: the tool that spends a service's quota,
//! started on that service's best account. Claude Code is launched by
//! [`crate::launch::run`], which moves the shared default first; the others
//! are seated through the child's environment and exec'd by `main`.

use anyhow::{Context, Result};

use super::services;
use super::view::Best;
use crate::account::Fleet;
use crate::launch::Launch;
use crate::providers::antigravity::run;
use crate::quota::Service;

/// The winner of `best` and the room on its tightest counted limit.
fn winner(best: &Best) -> Result<(&str, f64)> {
    let (name, room) = best
        .ranked
        .first()
        .with_context(|| format!("no {} account to launch on", best.service.label()))?;
    Ok((name, *room))
}

/// Codex on the best viable profile: `CODEX_HOME` is that profile's
/// directory. `TOKEMAN_CODEX_BIN` overrides the binary.
pub fn codex(fleet: &Fleet, best: &Best, args: Vec<String>) -> Result<Launch> {
    let (name, room) = winner(best)?;
    let account = fleet
        .codex_account(name)
        .with_context(|| format!("no codex account named {name}"))?;
    let program = std::env::var_os("TOKEMAN_CODEX_BIN").unwrap_or_else(|| "codex".into());
    Ok(Launch::new(Service::Codex, name, Some(room), program, args)
        .set("CODEX_HOME", account.home()))
}

/// agy on the best account, the way `tokeman agy run <account>` starts it.
pub async fn agy(fleet: &Fleet, best: &Best, args: Vec<String>) -> Result<(run::Launch, f64)> {
    let (name, room) = winner(best)?;
    Ok((services::agy_run(fleet, name, args).await?, room))
}

/// How a vendor serves Claude Code: its Anthropic-compatible endpoint, the
/// variable Claude Code reads the key from, and the vendor's models for
/// Claude Code's tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: &'static str,
    pub key_var: &'static str,
    /// Every tier but the small one.
    pub model: &'static str,
    /// The tier Claude Code uses for background work.
    pub small_model: &'static str,
}

/// z.ai's GLM Coding Plan (docs.z.ai/devpack/tool/claude): a bearer token.
pub const ZAI: Endpoint = Endpoint {
    base_url: "https://api.z.ai/api/anthropic",
    key_var: "ANTHROPIC_AUTH_TOKEN",
    model: "glm-5.3",
    small_model: "glm-5.3-flash",
};

/// Kimi Code (kimi.com/code/docs, third-party-tools/claude-code): an API
/// key. `kimi-for-coding` is the model every membership tier has.
pub const KIMI: Endpoint = Endpoint {
    base_url: "https://api.kimi.com/coding/",
    key_var: "ANTHROPIC_API_KEY",
    model: "kimi-for-coding",
    small_model: "kimi-for-coding",
};

/// The variables Claude Code reads a model for one of its tiers from.
const MODEL_VARS: [&str; 5] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];
const SMALL_MODEL_VAR: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";
/// Credentials and an older model variable that would compete with the
/// vendor's if the child inherited them.
const COMPETING: [&str; 4] = [
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_SMALL_FAST_MODEL",
];

/// The endpoint of a service whose plan serves Claude Code.
pub fn endpoint(service: Service) -> Option<Endpoint> {
    match service {
        Service::Zai => Some(ZAI),
        Service::Kimi => Some(KIMI),
        Service::Claude | Service::Codex | Service::Antigravity => None,
    }
}

/// Claude Code on a GLM or Kimi account: the vendor's endpoint, the
/// account's key and the vendor's models, all in the child's environment.
/// Claude Code's settings.json is not touched, so every other session keeps
/// the Anthropic default. `model` names the vendor model for every tier.
pub fn vendor(
    fleet: &Fleet,
    best: &Best,
    model: Option<&str>,
    args: Vec<String>,
) -> Result<Launch> {
    let endpoint = endpoint(best.service)
        .with_context(|| format!("{} does not serve Claude Code", best.service.label()))?;
    let (name, room) = winner(best)?;
    let account = fleet
        .service_account(name)
        .filter(|account| account.service == best.service)
        .with_context(|| format!("no {} account named '{name}'", best.service.label()))?;
    let key = account.api_key()?;
    let (binary, args) = fleet.launch.command(args);
    let mut launch = Launch::new(best.service, name, Some(room), binary, args);
    for name in COMPETING {
        launch = launch.unset(name);
    }
    launch = launch
        .set("ANTHROPIC_BASE_URL", endpoint.base_url)
        .set_secret(endpoint.key_var, key);
    for var in MODEL_VARS {
        launch = launch.set(var, model.unwrap_or(endpoint.model));
    }
    Ok(launch.set(SMALL_MODEL_VAR, model.unwrap_or(endpoint.small_model)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ServiceAccount;
    use crate::testing::codex_account;

    fn best(service: Service, ranked: &[(&str, f64)]) -> Best {
        Best {
            service,
            readings: Vec::new(),
            ranked: ranked
                .iter()
                .map(|(name, room)| ((*name).to_owned(), *room))
                .collect(),
            current: None,
            home: None,
            scope: None,
        }
    }

    fn value<'a>(launch: &'a Launch, name: &str) -> Option<&'a str> {
        launch
            .env()
            .iter()
            .find(|var| var.name == name)
            .and_then(|var| var.value.to_str())
    }

    #[test]
    fn codex_starts_on_the_winning_profiles_home() {
        let fleet = Fleet {
            codex: vec![
                codex_account("spare", "/homes/spare"),
                codex_account("work", "/homes/work"),
            ],
            ..Fleet::default()
        };
        let launch = codex(
            &fleet,
            &best(Service::Codex, &[("work", 0.6), ("spare", 0.2)]),
            vec!["exec".into(), "--json".into()],
        )
        .unwrap();
        assert_eq!(launch.account, "work");
        assert_eq!(launch.room, Some(0.6));
        assert_eq!(value(&launch, "CODEX_HOME"), Some("/homes/work"));
        assert_eq!(launch.args(), ["exec", "--json"]);
        assert!(launch.env().iter().all(|var| !var.secret));
        // Nothing ranked, or a winner that left the fleet, is an error.
        assert!(codex(&fleet, &best(Service::Codex, &[]), Vec::new()).is_err());
        assert!(codex(&fleet, &best(Service::Codex, &[("gone", 1.0)]), Vec::new()).is_err());
    }

    #[test]
    fn a_vendor_launch_carries_the_endpoint_key_and_models_and_drops_anthropic_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("key");
        std::fs::write(&key_file, "  vendor-key\n").unwrap();
        let mut fleet = Fleet {
            services: vec![
                ServiceAccount::with_key_file(Service::Zai, "glm", &key_file.display().to_string()),
                ServiceAccount::with_key_file(
                    Service::Kimi,
                    "kimi",
                    &key_file.display().to_string(),
                ),
            ],
            ..Fleet::default()
        };
        fleet.launch.claude_bin = Some("/opt/claude".into());
        fleet.launch.launch_args = vec!["--verbose".into()];

        let glm = vendor(
            &fleet,
            &best(Service::Zai, &[("glm", 0.9)]),
            None,
            vec!["-p".into()],
        )
        .unwrap();
        assert_eq!(glm.args(), ["--verbose", "-p"]);
        assert_eq!(
            value(&glm, "ANTHROPIC_BASE_URL"),
            Some("https://api.z.ai/api/anthropic")
        );
        assert_eq!(value(&glm, "ANTHROPIC_AUTH_TOKEN"), Some("vendor-key"));
        assert_eq!(value(&glm, "ANTHROPIC_API_KEY"), None);
        let secrets: Vec<&str> = glm
            .env()
            .iter()
            .filter(|var| var.secret)
            .map(|var| var.name)
            .collect();
        assert_eq!(secrets, ["ANTHROPIC_AUTH_TOKEN"]);
        for var in MODEL_VARS {
            assert_eq!(value(&glm, var), Some("glm-5.3"), "{var}");
        }
        assert_eq!(
            value(&glm, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("glm-5.3-flash")
        );
        // The session's Anthropic credential and any other key stay behind.
        for gone in ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
            assert!(glm.removed().contains(&gone), "{gone}");
        }

        let kimi = vendor(
            &fleet,
            &best(Service::Kimi, &[("kimi", 0.5)]),
            Some("k3.1-pro"),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            value(&kimi, "ANTHROPIC_BASE_URL"),
            Some("https://api.kimi.com/coding/")
        );
        assert_eq!(value(&kimi, "ANTHROPIC_API_KEY"), Some("vendor-key"));
        assert_eq!(value(&kimi, "ANTHROPIC_AUTH_TOKEN"), None);
        assert_eq!(value(&kimi, "ANTHROPIC_MODEL"), Some("k3.1-pro"));
        assert_eq!(
            value(&kimi, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("k3.1-pro")
        );

        // A GLM winner is not looked up among Kimi's accounts, and the
        // services with a tool of their own have no endpoint.
        assert!(
            vendor(
                &fleet,
                &best(Service::Kimi, &[("glm", 1.0)]),
                None,
                Vec::new()
            )
            .is_err()
        );
        assert!(
            vendor(
                &fleet,
                &best(Service::Claude, &[("a", 1.0)]),
                None,
                Vec::new()
            )
            .is_err()
        );
        std::fs::write(&key_file, "\n").unwrap();
        assert!(
            vendor(
                &fleet,
                &best(Service::Zai, &[("glm", 0.9)]),
                None,
                Vec::new()
            )
            .is_err()
        );
    }
}
