//! What a service sent that this build does not know: a rate-limit header, a
//! key of a usage body, a value of a field that holds one of a fixed set of
//! words. Each parser keeps the names it knows as constants beside itself
//! and collects the rest here, into [`crate::quota::Reading::unrecognized`].
//!
//! An item is a path and, for a value, the value: `usage.limits[].flavor`,
//! `usage.limits[].kind=weekly_turbo`, `header anthropic-ratelimit-unified-8h-reset`.
//! Only names and the words of enumerated fields are kept, never the content
//! of a free-form field.
//!
//! [`Ledger`] is the file the daemon keeps of the items it has seen
//! (`drift.json`), so each is logged once. This module is its only writer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fs::write_atomic;
use crate::paths;
use crate::quota::{Reading, Service};

/// The longest name or word kept; a longer one is cut.
const MAX_WORD: usize = 64;

/// A name or word as it is kept: printable ASCII without spaces, cut to
/// [`MAX_WORD`]. Anything else becomes `?`, so an item is always one token
/// and never carries text a server put in a field.
fn word(raw: &str) -> String {
    raw.chars()
        .take(MAX_WORD)
        .map(|c| {
            if c.is_ascii_graphic() && c != '=' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// The unrecognised items of one read, sorted and unique.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unrecognized(BTreeSet<String>);

impl Unrecognized {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `item` as it stands.
    pub fn add(&mut self, item: impl Into<String>) {
        self.0.insert(item.into());
    }

    /// The keys of the object `value` that are not in `known`, as
    /// `{at}.{key}`. A value that is not an object has no keys.
    pub fn keys(&mut self, at: &str, value: &Value, known: &[&str]) {
        for key in value
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys())
        {
            if !known.contains(&key.as_str()) {
                self.add(format!("{at}.{}", word(key)));
            }
        }
    }

    /// [`Self::keys`] for every row of the array `value`, as `{at}[].{key}`.
    pub fn row_keys(&mut self, at: &str, value: &Value, known: &[&str]) {
        let at = format!("{at}[]");
        for row in value.as_array().into_iter().flatten() {
            self.keys(&at, row, known);
        }
    }

    /// The string `value` when it is not one of `known`, as `{at}={value}`.
    /// Null, absent and non-string values are not words.
    pub fn value(&mut self, at: &str, value: Option<&Value>, known: &[&str]) {
        if let Some(text) = value.and_then(Value::as_str) {
            self.text(at, text, known);
        }
    }

    /// [`Self::value`] for a word already in hand.
    pub fn text(&mut self, at: &str, text: &str, known: &[&str]) {
        if !known.contains(&text) {
            self.add(format!("{at}={}", word(text)));
        }
    }

    /// [`Self::value`] for every string of the array `value`.
    pub fn values(&mut self, at: &str, value: Option<&Value>, known: &[&str]) {
        for item in value.and_then(Value::as_array).into_iter().flatten() {
            self.value(at, Some(item), known);
        }
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = String>) {
        self.0.extend(items);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_vec(self) -> Vec<String> {
        self.0.into_iter().collect()
    }
}

/// Add `items` to a sorted, unique list.
pub fn merge(into: &mut Vec<String>, items: impl IntoIterator<Item = String>) {
    let mut all: BTreeSet<String> = std::mem::take(into).into_iter().collect();
    all.extend(items);
    *into = all.into_iter().collect();
}

/// When and where the daemon first saw an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// Epoch seconds.
    pub first_seen: i64,
    /// The account whose read carried it.
    pub account: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct LedgerFile {
    /// The tokeman version whose known sets the items were judged against.
    #[serde(default)]
    build: String,
    #[serde(default)]
    items: BTreeMap<Service, BTreeMap<String, Seen>>,
}

/// One item of the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub service: Service,
    pub item: String,
    #[serde(flatten)]
    pub seen: Seen,
}

/// The items the daemon has seen and logged. What another build recorded is
/// not this build's: its known sets differ, so those items are left out when
/// read and dropped at the next write.
#[derive(Debug, Clone)]
pub struct Ledger {
    path: PathBuf,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::at(paths::drift_ledger())
    }
}

impl Ledger {
    const BUILD: &'static str = env!("CARGO_PKG_VERSION");

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The file as this build recorded it; a missing or unreadable file, or
    /// another build's, reads as empty.
    fn load(&self) -> LedgerFile {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LedgerFile>(&bytes).ok())
            .filter(|file| file.build == Self::BUILD)
            .unwrap_or_else(|| LedgerFile {
                build: Self::BUILD.to_owned(),
                items: BTreeMap::new(),
            })
    }

    /// Every item on record, by service and then by name. No write.
    pub fn entries(&self) -> Vec<Entry> {
        self.load()
            .items
            .into_iter()
            .flat_map(|(service, items)| {
                items.into_iter().map(move |(item, seen)| Entry {
                    service,
                    item,
                    seen,
                })
            })
            .collect()
    }

    /// Put `items` of one read on record and return the ones that were not
    /// there yet. The file is written only when something is new.
    pub fn record(
        &self,
        service: Service,
        account: &str,
        items: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<String>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut file = self.load();
        let known = file.items.entry(service).or_default();
        let new: Vec<String> = items
            .iter()
            .filter(|item| !known.contains_key(*item))
            .cloned()
            .collect();
        if new.is_empty() {
            return Ok(new);
        }
        for item in &new {
            known.insert(
                item.clone(),
                Seen {
                    first_seen: now,
                    account: account.to_owned(),
                },
            );
        }
        let mut bytes = serde_json::to_vec_pretty(&file)?;
        bytes.push(b'\n');
        write_atomic(&self.path, &bytes, 0o600)?;
        Ok(new)
    }
}

/// Put what `readings` did not recognise on `ledger`, and say each item
/// that is new to it in one line for the log.
pub fn new_items(ledger: &Ledger, readings: &[Reading], now: i64) -> Vec<String> {
    let mut lines = Vec::new();
    for reading in readings {
        match ledger.record(
            reading.service,
            &reading.account,
            &reading.unrecognized,
            now,
        ) {
            Ok(new) => lines.extend(new.into_iter().map(|item| {
                format!(
                    "drift: {} sent something this build does not know: {item} (read of {}); \
                     see `tokeman doctor`",
                    reading.service.name(),
                    reading.account
                )
            })),
            Err(error) => lines.push(format!("ERROR drift ledger not written: {error:#}")),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_values_and_rows_outside_the_known_sets_are_collected_as_paths() {
        let body = json!({
            "known": 1,
            "zinc_parrot": {"secret": "do not keep"},
            "rows": [
                {"kind": "weekly_all", "flavor": 3},
                {"kind": "weekly_turbo"},
                {"kind": null},
                "not an object"
            ],
            "tags": ["a", "b", 7]
        });
        let mut seen = Unrecognized::new();
        seen.keys("usage", &body, &["known", "rows", "tags"]);
        seen.row_keys("usage.rows", &body["rows"], &["kind"]);
        for row in body["rows"].as_array().unwrap() {
            seen.value("usage.rows[].kind", row.get("kind"), &["weekly_all"]);
        }
        seen.values("usage.tags", body.get("tags"), &["a"]);
        assert_eq!(
            seen.into_vec(),
            [
                "usage.rows[].flavor",
                "usage.rows[].kind=weekly_turbo",
                "usage.tags=b",
                "usage.zinc_parrot",
            ]
        );
    }

    #[test]
    fn an_item_is_one_bounded_token_whatever_the_server_put_in_it() {
        let mut seen = Unrecognized::new();
        seen.text("usage.kind", "two words\nand=more", &[]);
        seen.text("usage.kind", &"x".repeat(500), &[]);
        let items = seen.into_vec();
        assert_eq!(items[0], "usage.kind=two?words?and?more");
        assert_eq!(items[1].len(), "usage.kind=".len() + MAX_WORD);
    }

    #[test]
    fn merging_keeps_a_list_sorted_and_unique() {
        let mut list = vec!["b".to_owned(), "d".to_owned()];
        merge(&mut list, ["c".to_owned(), "b".to_owned(), "a".to_owned()]);
        assert_eq!(list, ["a", "b", "c", "d"]);
    }

    #[test]
    fn the_ledger_reports_an_item_as_new_once_and_keeps_where_it_was_first_seen() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("drift.json"));
        assert!(ledger.entries().is_empty());
        assert!(
            ledger
                .record(Service::Claude, "first@example.com", &[], 10)
                .unwrap()
                .is_empty()
        );
        assert!(!dir.path().join("drift.json").exists(), "nothing to write");

        let items = ["usage.a".to_owned(), "usage.b".to_owned()];
        let new = ledger
            .record(Service::Claude, "first@example.com", &items, 10)
            .unwrap();
        assert_eq!(new, items);
        let again = [items[1].clone(), "usage.c".to_owned()];
        let new = ledger
            .record(Service::Claude, "second@example.com", &again, 20)
            .unwrap();
        assert_eq!(new, ["usage.c"]);
        // The same name on another service is another item.
        let new = ledger
            .record(Service::Codex, "work", &items[..1], 30)
            .unwrap();
        assert_eq!(new, ["usage.a"]);

        let entries = ledger.entries();
        assert_eq!(entries.len(), 4);
        let b = entries
            .iter()
            .find(|entry| entry.service == Service::Claude && entry.item == "usage.b")
            .unwrap();
        assert_eq!(b.seen.first_seen, 10);
        assert_eq!(b.seen.account, "first@example.com");
    }

    #[test]
    fn each_new_item_of_a_read_is_one_log_line_and_a_repeat_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("drift.json"));
        let mut reading = Reading::empty(Service::Kimi, "first@example.com", 100);
        reading.unrecognized = vec!["usages.usages.limit_day".to_owned()];
        let quiet = Reading::empty(Service::Zai, "glm", 100);
        let lines = new_items(&ledger, &[reading.clone(), quiet.clone()], 100);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("drift: kimi sent something"));
        assert!(lines[0].contains("usages.usages.limit_day"));
        assert!(new_items(&ledger, &[reading, quiet], 200).is_empty());
    }

    #[test]
    fn what_another_build_recorded_is_not_read_and_is_dropped_at_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drift.json");
        std::fs::write(
            &path,
            r#"{"build": "0.0.1", "items": {"claude": {"usage.old": {"first_seen": 1, "account": "a"}}}}"#,
        )
        .unwrap();
        let ledger = Ledger::at(path.clone());
        assert!(ledger.entries().is_empty());
        let new = ledger
            .record(Service::Claude, "a", &["usage.old".to_owned()], 5)
            .unwrap();
        assert_eq!(new, ["usage.old"], "judged again by this build");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(Ledger::BUILD));
        assert!(!text.contains("0.0.1"));
        assert_eq!(ledger.entries()[0].seen.first_seen, 5);
    }
}
