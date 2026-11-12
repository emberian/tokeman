//! The claim journal: every reset claim (a Claude grant, a Codex reset
//! credit) sent without a definite answer yet, keyed by service and account.
//!
//! A claim carries a request id the server treats as an idempotency key. The
//! id is recorded here before the request goes out and dropped only once an
//! answer is read, so a retry after a lost answer resends the same id and
//! cannot spend a second grant or credit. While an entry stands, the account
//! may only resend that claim: naming another grant or credit is refused.
//!
//! One file (`claims.json`, 0600), written only here, each change a locked
//! read-modify-write. The two layouts earlier builds wrote are read where
//! they lie: `reset-claims/<account>.pending` (Claude, one file per account,
//! the account name with every character outside `[A-Za-z0-9-_.@]` replaced
//! by `_`) and `codex-reset-pending.json` (Codex, one map). A read sees their
//! entries without writing; the first change folds them into `claims.json`
//! and then renames each old file to `<name>.imported`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fs::{FileLock, read_if_present, write_atomic};
use crate::paths;
use crate::quota::Service;

/// A claim sent without a definite answer yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub service: Service,
    pub account: String,
    /// The grant or credit claimed; `None` left the choice to the backend.
    #[serde(default)]
    pub item: Option<String>,
    /// Sent with the claim and resent verbatim by every retry.
    pub request_id: String,
    /// Epoch seconds the claim was first sent.
    pub sent_at: i64,
}

/// A claim that names another grant or credit than the one still unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub pending: Pending,
    pub requested: String,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a claim of {} on {} has no known outcome; {} cannot be claimed until it is answered",
            self.pending
                .item
                .as_deref()
                .unwrap_or("the backend's choice"),
            self.pending.account,
            self.requested
        )
    }
}

impl std::error::Error for Conflict {}

/// An entry as stored. `sanitized` marks one taken from a
/// `reset-claims/<account>.pending` file, whose `account` is the file's name
/// and stands for every account name that sanitizes to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    pending: Pending,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    sanitized: bool,
}

impl Entry {
    fn is_for(&self, service: Service, account: &str) -> bool {
        self.pending.service == service
            && (self.pending.account == account
                || (self.sanitized && self.pending.account == sanitize(account)))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    pending: Vec<Entry>,
}

/// The name `reset-claims/` gave an account's file.
fn sanitize(account: &str) -> String {
    account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.@".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `reset-claims/<account>.pending`.
#[derive(Deserialize)]
struct LegacyClaude {
    grant_id: String,
    request_id: String,
    sent_at: i64,
}

/// One entry of `codex-reset-pending.json`.
#[derive(Deserialize)]
struct LegacyCodex {
    redeem_request_id: String,
    credit_id: Option<String>,
    started_at: i64,
}

#[derive(Deserialize)]
struct LegacyCodexFile {
    #[serde(default)]
    pending: BTreeMap<String, LegacyCodex>,
}

/// What the old layouts hold now.
#[derive(Default)]
struct Legacy {
    entries: Vec<Entry>,
    /// The old files that were read, to move aside once folded in.
    sources: Vec<PathBuf>,
    /// Old files that do not parse: each still stands for a claim in flight.
    unreadable: Vec<(Entry, PathBuf)>,
}

/// The journal. This type is the only writer of its file.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
    lock: PathBuf,
    legacy_claude: PathBuf,
    legacy_codex: PathBuf,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            path: paths::claims_journal(),
            lock: paths::claims_lock(),
            legacy_claude: paths::legacy_reset_claims_dir(),
            legacy_codex: paths::legacy_codex_reset_journal(),
        }
    }
}

impl Journal {
    /// A journal under `dir`, laid out like the config directory.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            path: dir.join("claims.json"),
            lock: dir.join("claims.lock"),
            legacy_claude: dir.join("reset-claims"),
            legacy_codex: dir.join("codex-reset-pending.json"),
        }
    }

    /// An unreadable journal is an error, never an empty one: treating it as
    /// empty would mint a second id for a claim that may have gone through.
    fn read(&self) -> Result<File> {
        match read_if_present(&self.path)? {
            None => Ok(File::default()),
            Some(text) => serde_json::from_str(&text).with_context(|| {
                format!(
                    "{} is unreadable; fix or remove it by hand once no claim is in flight",
                    self.path.display()
                )
            }),
        }
    }

    fn write(&self, file: &File) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(file)?;
        bytes.push(b'\n');
        write_atomic(&self.path, &bytes, 0o600)
    }

    fn legacy(&self) -> Result<Legacy> {
        let mut legacy = Legacy::default();
        let mut files: Vec<PathBuf> = match std::fs::read_dir(&self.legacy_claude) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "pending"))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading {}", self.legacy_claude.display()));
            }
        };
        files.sort();
        for path in files {
            let Some(account) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let entry = |item: Option<String>, request_id: String, sent_at: i64| Entry {
                pending: Pending {
                    service: Service::Claude,
                    account: account.to_owned(),
                    item,
                    request_id,
                    sent_at,
                },
                sanitized: true,
            };
            let text = std::fs::read(&path).with_context(|| format!("reading {}", path.display()));
            match serde_json::from_slice::<LegacyClaude>(&text?) {
                Ok(old) => {
                    legacy
                        .entries
                        .push(entry(Some(old.grant_id), old.request_id, old.sent_at));
                    legacy.sources.push(path);
                }
                Err(_) => legacy
                    .unreadable
                    .push((entry(None, String::new(), 0), path)),
            }
        }
        if let Some(text) = read_if_present(&self.legacy_codex)? {
            let old: LegacyCodexFile = serde_json::from_str(&text).with_context(|| {
                format!(
                    "{} is unreadable; fix or remove it by hand once no claim is in flight",
                    self.legacy_codex.display()
                )
            })?;
            legacy
                .entries
                .extend(old.pending.into_iter().map(|(account, claim)| Entry {
                    pending: Pending {
                        service: Service::Codex,
                        account,
                        item: claim.credit_id,
                        request_id: claim.redeem_request_id,
                        sent_at: claim.started_at,
                    },
                    sanitized: false,
                }));
            legacy.sources.push(self.legacy_codex.clone());
        }
        Ok(legacy)
    }

    /// The journal with the old layouts' entries merged in. An entry of
    /// `claims.json` wins over an old one for the same account.
    fn merged(&self) -> Result<(File, Legacy)> {
        let mut file = self.read()?;
        let legacy = self.legacy()?;
        for old in &legacy.entries {
            let known = file.pending.iter().any(|entry| {
                entry.pending.service == old.pending.service
                    && (entry.is_for(old.pending.service, &old.pending.account)
                        || (old.sanitized
                            && sanitize(&entry.pending.account) == old.pending.account))
            });
            if !known {
                file.pending.push(old.clone());
            }
        }
        Ok((file, legacy))
    }

    /// An old file that does not parse still blocks its account.
    fn refuse_unreadable(legacy: &Legacy, service: Service, account: &str) -> Result<()> {
        match legacy
            .unreadable
            .iter()
            .find(|(entry, _)| entry.is_for(service, account))
        {
            Some((_, path)) => bail!(
                "unreadable pending claim {}; it stands for a claim in flight",
                path.display()
            ),
            None => Ok(()),
        }
    }

    /// Write the merged journal, then move the old files aside. The new file
    /// is on disk before any old one is renamed.
    fn commit(&self, file: &File, legacy: &Legacy) -> Result<()> {
        self.write(file)?;
        for source in &legacy.sources {
            set_aside(source)?;
        }
        Ok(())
    }

    fn locked(&self) -> Result<FileLock> {
        FileLock::acquire_announced(&self.lock, "another tokeman reset claim")
    }

    /// The account's unanswered claim. Reads only: no file, lock or
    /// directory is created.
    pub fn pending(&self, service: Service, account: &str) -> Result<Option<Pending>> {
        let (file, legacy) = self.merged()?;
        Self::refuse_unreadable(&legacy, service, account)?;
        Ok(found(&file, service, account))
    }

    /// The claim to send for `account`: the pending one if there is one,
    /// else a new id, recorded before it is returned. `true` when new. With
    /// a claim pending, naming another `item` is refused with a [`Conflict`];
    /// naming the same one, or none, resends it.
    pub fn begin(
        &self,
        service: Service,
        account: &str,
        item: Option<&str>,
        now: i64,
    ) -> Result<(Pending, bool)> {
        let _lock = self.locked()?;
        let (mut file, legacy) = self.merged()?;
        Self::refuse_unreadable(&legacy, service, account)?;
        if let Some(pending) = found(&file, service, account) {
            if let Some(requested) = item
                && pending.item.as_deref() != Some(requested)
            {
                bail!(Conflict {
                    pending,
                    requested: requested.to_owned(),
                });
            }
            if !legacy.sources.is_empty() {
                self.commit(&file, &legacy)?;
            }
            return Ok((pending, false));
        }
        let pending = Pending {
            service,
            account: account.to_owned(),
            item: item.map(str::to_owned),
            request_id: request_id()?,
            sent_at: now,
        };
        file.pending.push(Entry {
            pending: pending.clone(),
            sanitized: false,
        });
        self.commit(&file, &legacy)?;
        Ok((pending, true))
    }

    /// Drop the pending claim once the server answered it. A `request_id`
    /// that is not the pending one changes nothing.
    pub fn finish(&self, service: Service, account: &str, request_id: &str) -> Result<()> {
        let _lock = self.locked()?;
        let (mut file, legacy) = self.merged()?;
        let before = file.pending.len();
        file.pending.retain(|entry| {
            !(entry.is_for(service, account) && entry.pending.request_id == request_id)
        });
        if file.pending.len() != before || !legacy.sources.is_empty() {
            self.commit(&file, &legacy)?;
        }
        Ok(())
    }

    /// Forget the account's pending claim because the user established its
    /// outcome elsewhere. An old file that does not parse is moved aside too.
    pub fn clear(&self, service: Service, account: &str) -> Result<Option<Pending>> {
        let _lock = self.locked()?;
        let (mut file, legacy) = self.merged()?;
        let cleared = found(&file, service, account);
        file.pending.retain(|entry| !entry.is_for(service, account));
        if cleared.is_some() || !legacy.sources.is_empty() {
            self.commit(&file, &legacy)?;
        }
        for (entry, path) in &legacy.unreadable {
            if entry.is_for(service, account) {
                set_aside(path)?;
            }
        }
        Ok(cleared)
    }
}

/// The account's entry, named as the caller named the account.
fn found(file: &File, service: Service, account: &str) -> Option<Pending> {
    file.pending
        .iter()
        .find(|entry| entry.is_for(service, account))
        .map(|entry| Pending {
            account: account.to_owned(),
            ..entry.pending.clone()
        })
}

/// Rename an old-layout file to `<name>.imported`.
fn set_aside(path: &Path) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".imported");
    match std::fs::rename(path, path.with_file_name(name)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("moving {} aside", path.display()))
        }
        _ => Ok(()),
    }
}

/// A random UUID v4: the form Codex uses for its idempotency keys, and
/// within what Claude's claim endpoint accepts (`[A-Za-z0-9_-]{1,64}`).
pub fn request_id() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .context("could not read /dev/urandom for a request id")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// `reset-claims/<name>.pending` as 0.3.x wrote it (`to_vec_pretty`).
    pub(crate) fn legacy_claude_fixture(dir: &Path, file_stem: &str, grant: &str, id: &str) {
        let claims = dir.join("reset-claims");
        std::fs::create_dir_all(&claims).unwrap();
        std::fs::write(
            claims.join(format!("{file_stem}.pending")),
            format!(
                "{{\n  \"grant_id\": \"{grant}\",\n  \"request_id\": \"{id}\",\n  \"sent_at\": 1762876800\n}}"
            ),
        )
        .unwrap();
    }

    /// `codex-reset-pending.json` as 0.3.x wrote it (`to_string_pretty`).
    pub(crate) fn legacy_codex_fixture(dir: &Path) {
        std::fs::write(
            dir.join("codex-reset-pending.json"),
            r#"{
  "pending": {
    "one": {
      "redeem_request_id": "0b0e3c1a-5a52-4c8e-9d0a-7f3e2b1c4d5e",
      "credit_id": "RateLimitResetCredit_a",
      "started_at": 1762876900
    },
    "two": {
      "redeem_request_id": "11111111-2222-4333-8444-555555555555",
      "credit_id": null,
      "started_at": 1762877000
    }
  }
}"#,
        )
        .unwrap();
    }

    #[test]
    fn request_ids_are_v4_uuids_and_fit_claudes_format() {
        let id = request_id().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(id, request_id().unwrap());
    }

    #[test]
    fn one_claim_per_service_and_account_until_it_is_answered() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path());
        assert_eq!(journal.pending(Service::Codex, "a").unwrap(), None);
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "a read creates nothing"
        );
        let (first, fresh) = journal.begin(Service::Codex, "a", None, 10).unwrap();
        assert!(fresh);
        let (again, fresh) = journal.begin(Service::Codex, "a", None, 20).unwrap();
        assert!(!fresh);
        assert_eq!(again, first, "a retry reuses the id and its start time");
        // The same account name on another service is another claim.
        let (claude, fresh) = journal.begin(Service::Claude, "a", Some("g1"), 30).unwrap();
        assert!(fresh);
        assert_ne!(claude.request_id, first.request_id);
        // Another item while one is pending is refused, not minted.
        let error = journal
            .begin(Service::Claude, "a", Some("g2"), 40)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Conflict>(),
            Some(&Conflict {
                pending: claude.clone(),
                requested: "g2".into()
            })
        );
        // Naming a credit when the pending claim left the choice open is
        // another claim too.
        assert!(journal.begin(Service::Codex, "a", Some("c1"), 45).is_err());
        // Naming none, or the same one, resends the pending claim.
        assert_eq!(
            journal.begin(Service::Claude, "a", None, 50).unwrap().0,
            claude
        );
        assert_eq!(
            journal
                .begin(Service::Claude, "a", Some("g1"), 55)
                .unwrap()
                .0,
            claude
        );
        // Finishing with a stale id leaves the entry.
        journal.finish(Service::Codex, "a", "not-the-id").unwrap();
        assert_eq!(
            journal.pending(Service::Codex, "a").unwrap(),
            Some(first.clone())
        );
        journal
            .finish(Service::Codex, "a", &first.request_id)
            .unwrap();
        assert_eq!(journal.pending(Service::Codex, "a").unwrap(), None);
        assert_eq!(
            journal.clear(Service::Claude, "a").unwrap(),
            Some(claude.clone())
        );
        assert_eq!(journal.clear(Service::Claude, "a").unwrap(), None);
        let mode = std::fs::metadata(dir.path().join("claims.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // A damaged journal stops claims instead of minting a new id.
        std::fs::write(dir.path().join("claims.json"), "{oops").unwrap();
        assert!(journal.begin(Service::Codex, "c", None, 60).is_err());
        assert!(journal.pending(Service::Codex, "c").is_err());
    }

    #[test]
    fn the_file_layout_is_one_list_of_entries() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path());
        let (pending, _) = journal
            .begin(Service::Claude, "a@b.c", Some("g1"), 7)
            .unwrap();
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("claims.json")).unwrap())
                .unwrap();
        assert_eq!(
            file,
            serde_json::json!({"pending": [{
                "service": "claude", "account": "a@b.c", "item": "g1",
                "request_id": pending.request_id, "sent_at": 7
            }]})
        );
    }

    #[test]
    fn both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change() {
        let dir = tempfile::tempdir().unwrap();
        legacy_claude_fixture(
            dir.path(),
            "me@example.com",
            "fable52-launch",
            "0123456789abcdef0123456789abcdef",
        );
        // `team/a b` was written as `team_a_b.pending`.
        legacy_claude_fixture(
            dir.path(),
            "team_a_b",
            "g-team",
            "feedfacefeedfacefeedfacefeedface",
        );
        legacy_codex_fixture(dir.path());
        let journal = Journal::at(dir.path());

        let claude = journal
            .pending(Service::Claude, "me@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(
            claude,
            Pending {
                service: Service::Claude,
                account: "me@example.com".into(),
                item: Some("fable52-launch".into()),
                request_id: "0123456789abcdef0123456789abcdef".into(),
                sent_at: 1_762_876_800,
            }
        );
        let team = journal
            .pending(Service::Claude, "team/a b")
            .unwrap()
            .unwrap();
        assert_eq!(team.account, "team/a b");
        assert_eq!(team.item.as_deref(), Some("g-team"));
        let codex = journal.pending(Service::Codex, "one").unwrap().unwrap();
        assert_eq!(codex.item.as_deref(), Some("RateLimitResetCredit_a"));
        assert_eq!(codex.request_id, "0b0e3c1a-5a52-4c8e-9d0a-7f3e2b1c4d5e");
        assert_eq!(codex.sent_at, 1_762_876_900);
        assert_eq!(
            journal
                .pending(Service::Codex, "two")
                .unwrap()
                .unwrap()
                .item,
            None
        );
        // An old Claude file never answers for a Codex account of that name.
        assert_eq!(
            journal.pending(Service::Codex, "me@example.com").unwrap(),
            None
        );
        assert!(
            !dir.path().join("claims.json").exists(),
            "reads do not write"
        );
        assert!(!dir.path().join("claims.lock").exists());

        // Another grant or credit is refused; the same one resends the old id.
        for (service, account, other) in [
            (Service::Claude, "me@example.com", "other-grant"),
            (Service::Claude, "team/a b", "other-grant"),
            (Service::Codex, "one", "RateLimitResetCredit_b"),
            (Service::Codex, "two", "RateLimitResetCredit_b"),
        ] {
            let error = journal.begin(service, account, Some(other), 9).unwrap_err();
            assert!(error.downcast_ref::<Conflict>().is_some(), "{error}");
        }
        assert!(!dir.path().join("claims.json").exists());
        let (resent, fresh) = journal
            .begin(Service::Claude, "me@example.com", Some("fable52-launch"), 9)
            .unwrap();
        assert!(!fresh);
        assert_eq!(resent, claude);

        // That change folded every old entry in and moved the old files aside.
        let claims = dir.path().join("reset-claims");
        assert!(!claims.join("me@example.com.pending").exists());
        assert!(claims.join("me@example.com.pending.imported").exists());
        assert!(claims.join("team_a_b.pending.imported").exists());
        assert!(!dir.path().join("codex-reset-pending.json").exists());
        assert!(
            dir.path()
                .join("codex-reset-pending.json.imported")
                .exists()
        );
        assert_eq!(
            journal.pending(Service::Claude, "team/a b").unwrap(),
            Some(team.clone())
        );
        assert_eq!(journal.pending(Service::Codex, "one").unwrap(), Some(codex));
        assert_eq!(
            journal
                .begin(Service::Codex, "two", None, 11)
                .unwrap()
                .0
                .request_id,
            "11111111-2222-4333-8444-555555555555"
        );
        // An answer drops a folded-in entry like any other.
        journal
            .finish(Service::Claude, "team/a b", &team.request_id)
            .unwrap();
        assert_eq!(journal.pending(Service::Claude, "team/a b").unwrap(), None);
    }

    #[test]
    fn an_entry_of_the_new_file_wins_over_an_old_file_for_the_same_account() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path());
        let (new, _) = journal
            .begin(Service::Claude, "me@example.com", Some("g-new"), 5)
            .unwrap();
        legacy_claude_fixture(dir.path(), "me@example.com", "g-old", "0ld");
        assert_eq!(
            journal.pending(Service::Claude, "me@example.com").unwrap(),
            Some(new.clone())
        );
        // The next change keeps the new entry and still moves the old file aside.
        journal.begin(Service::Codex, "x", None, 6).unwrap();
        assert!(
            dir.path()
                .join("reset-claims/me@example.com.pending.imported")
                .exists()
        );
        assert_eq!(
            journal.pending(Service::Claude, "me@example.com").unwrap(),
            Some(new)
        );
    }

    #[test]
    fn an_old_file_that_does_not_parse_blocks_its_account_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let claims = dir.path().join("reset-claims");
        std::fs::create_dir_all(&claims).unwrap();
        std::fs::write(claims.join("a_b.pending"), "not json").unwrap();
        let journal = Journal::at(dir.path());
        assert!(journal.pending(Service::Claude, "a/b").is_err());
        assert!(journal.begin(Service::Claude, "a/b", Some("g"), 1).is_err());
        // Other accounts are not held up by it.
        assert_eq!(journal.pending(Service::Claude, "other").unwrap(), None);
        assert_eq!(journal.clear(Service::Claude, "a/b").unwrap(), None);
        assert!(claims.join("a_b.pending.imported").exists());
        assert_eq!(journal.pending(Service::Claude, "a/b").unwrap(), None);

        // The old Codex map is one file: unreadable, it is an error.
        std::fs::write(dir.path().join("codex-reset-pending.json"), "{oops").unwrap();
        assert!(journal.pending(Service::Codex, "one").is_err());
        assert!(journal.begin(Service::Codex, "one", None, 1).is_err());
    }
}
