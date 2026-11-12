//! `tokeman history compact`: the one-time rewrite of the tables builds
//! before 0.4 wrote (`snapshots`, `codex_readings`, `service_readings`) into
//! `readings`, under the growth rule [`Store::insert`](super::Store::insert)
//! applies to a live read.
//!
//! The database is held exclusively for the whole run, so no other process
//! reads or writes it meanwhile. The steps, each of which a later run picks
//! up where an interrupted one stopped:
//!
//! 1. The file is copied to `snapshots.db.pre-0.4` beside it, before
//!    anything is changed.
//! 2. Each old table is walked in batches, one transaction each, into the
//!    staging table `compact_rows`; `compact_progress` records, in the same
//!    transaction, the last row taken.
//! 3. Every account's old rows are read again and compared with what was
//!    staged (`verify`). A difference throws the staged rows away and
//!    leaves the old tables as they were.
//! 4. One transaction appends the rows `readings` already has to the staged
//!    ones, renames the staging table to `readings` and drops the old tables.
//! 5. `VACUUM`, then `compact_progress` is dropped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::legacy::{self, OldTable};
use super::{HOLD_SECS, Store, body, has_table, keep};
use crate::notice::notice;
use crate::paths;
use crate::quota::Reading;

/// Old rows taken per transaction.
const BATCH: usize = 20_000;

/// How long to wait for another process to let go of the database.
const WAIT: Duration = Duration::from_secs(15);

struct Tuning {
    batch: usize,
    wait: Duration,
}

const TUNING: Tuning = Tuning {
    batch: BATCH,
    wait: WAIT,
};

/// Free space asked for before starting, as a multiple of the database: the
/// kept original, the staged rows, and the copy `VACUUM` writes.
const ROOM: u64 = 3;

/// What a run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// No old table: there was nothing to do.
    Nothing,
    /// A dry run: what a run would keep. Nothing was written.
    Rehearsed,
    Compacted,
}

/// One old table's rows, and what became of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Table {
    pub table: String,
    pub rows: u64,
    /// Rows that do not read, left out as every earlier read left them out.
    pub unreadable: u64,
    /// Rows written to `readings`.
    pub kept: u64,
    /// Accounts whose history was compared before and after.
    pub accounts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub outcome: Outcome,
    pub path: PathBuf,
    pub tables: Vec<Table>,
    /// The database as it was, when this run or an earlier one kept it.
    pub original: Option<PathBuf>,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Where the database is kept as it was.
pub fn original_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.pre-0.4", path.display()))
}

pub fn run(dry_run: bool) -> Result<Report> {
    run_at(&paths::snapshots_db(), dry_run)
}

pub fn run_at(path: &Path, dry_run: bool) -> Result<Report> {
    if dry_run {
        rehearse(path)
    } else {
        compact(path, &TUNING, &mut |_, _| Ok(()))
    }
}

/// A point between two steps, each of which is on disk when it is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    OriginalKept,
    Batch,
    Rewritten,
    Swapped,
    Vacuumed,
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |file| file.len())
}

fn nothing(path: &Path) -> Report {
    let bytes = size(path);
    Report {
        outcome: Outcome::Nothing,
        path: path.to_owned(),
        tables: Vec::new(),
        original: None,
        bytes_before: bytes,
        bytes_after: bytes,
    }
}

/// What a run would keep, from a read-only walk of the old tables. Writes
/// nothing, takes no lock, and counts from the first row whatever an
/// interrupted run has staged.
fn rehearse(path: &Path) -> Result<Report> {
    if !path.exists() {
        return Ok(nothing(path));
    }
    let store = Store::open_read_only_at(path)?;
    let mut tables = Vec::new();
    for source in legacy::present(&store.conn)? {
        let mut progress = Progress::new(source);
        let mut walk = Walk::default();
        while take_batch(&store.conn, source, &mut progress, &mut walk, None, BATCH)? {}
        tables.push(progress.table(0));
    }
    Ok(Report {
        outcome: if tables.is_empty() {
            Outcome::Nothing
        } else {
            Outcome::Rehearsed
        },
        tables,
        ..nothing(path)
    })
}

/// `between` is called at every [`Step`] with the connection that holds the
/// database; an error from it ends the run there, as a kill would.
fn compact(
    path: &Path,
    tuning: &Tuning,
    between: &mut dyn FnMut(Step, &Connection) -> Result<()>,
) -> Result<Report> {
    if !path.exists() {
        return Ok(nothing(path));
    }
    // Declared before the connection, so dropped after it on every path.
    let mut opened = Opened::default();
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open database at {}", path.display()))?;
    take_exclusively(&conn, tuning.wait)?;
    let started = has_table(&conn, PROGRESS)?;
    let old = legacy::present(&conn)?;
    if old.is_empty() && !started {
        return Ok(nothing(path));
    }
    let original = original_of(path);
    if !old.is_empty() {
        room_for(path)?;
        keep_original(&conn, &mut opened, path, &original, started)?;
        between(Step::OriginalKept, &conn)?;
        // The progress table first, and whole: from here the kept original
        // is known to be this compaction's.
        conn.execute_batch(&format!("BEGIN; {SCHEMA} COMMIT;"))?;
        // With a write-ahead log the rows written here are not in the main
        // file until a checkpoint, and its presence tells a read-only open
        // (`Store::open_read_only`) not to read that file as immutable.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        for source in &old {
            rewrite(&conn, source, tuning.batch, between)?;
        }
        between(Step::Rewritten, &conn)?;
        let accounts = match verify(&conn, &old) {
            Ok(accounts) => accounts,
            Err(error) => {
                // The progress table stays: it says the kept original is
                // this run's, and the next run starts from the first row.
                conn.execute_batch(&format!(
                    "BEGIN; DROP TABLE {STAGING}; DELETE FROM {PROGRESS}; COMMIT;"
                ))?;
                return Err(error.context(
                    "the rewritten history is not the old one; the old tables are as they were \
                     and nothing was dropped",
                ));
            }
        };
        swap(&conn, &old, &accounts)?;
        between(Step::Swapped, &conn)?;
    }
    notice("history: reclaiming the space the old tables held");
    conn.execute_batch("VACUUM")?;
    between(Step::Vacuumed, &conn)?;
    let tables = Progress::all(&conn)?;
    conn.execute_batch(&format!("DROP TABLE {PROGRESS}"))?;
    conn.close().map_err(|(_, error)| error)?;
    crate::fs::set_mode(path, 0o600)?;
    Ok(Report {
        outcome: Outcome::Compacted,
        path: path.to_owned(),
        tables,
        bytes_before: if original.exists() {
            size(&original)
        } else {
            size(path)
        },
        bytes_after: size(path),
        original: original.exists().then_some(original),
    })
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Hold the database file exclusively until the connection closes: another
/// process that opens it meanwhile is refused by sqlite, and one that has it
/// open now makes this fail.
fn take_exclusively(conn: &Connection, wait: Duration) -> Result<()> {
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    let take = || conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;");
    conn.busy_timeout(Duration::ZERO)?;
    match take() {
        Ok(()) => return Ok(()),
        Err(error) if is_busy(&error) => {}
        Err(error) => return Err(error.into()),
    }
    notice("waiting for the history database (another tokeman has it open)");
    conn.busy_timeout(wait)?;
    match take() {
        Ok(()) => Ok(()),
        Err(error) if is_busy(&error) => bail!(
            "the history database is in use by another tokeman process (the background service \
             mid-read, or a `--watch`); nothing was changed. Run this again"
        ),
        Err(error) => Err(error.into()),
    }
}

fn room_for(path: &Path) -> Result<()> {
    let needed = size(path).saturating_mul(ROOM);
    let dir = path.parent().unwrap_or(Path::new("."));
    let free = fs2::available_space(dir)
        .with_context(|| format!("failed to read the free space of {}", dir.display()))?;
    if free < needed {
        bail!(
            "compacting needs about {} MB free in {} and {} MB are; nothing was changed",
            needed / 1_000_000,
            dir.display(),
            free / 1_000_000
        );
    }
    Ok(())
}

/// The descriptors this process opened on the database file itself. The
/// lock sqlite holds on a file is the process's, and the process gives it up
/// by closing any descriptor of that file, so each one opened while the
/// database is held stays open until the connection is closed.
#[derive(Default)]
struct Opened(Vec<std::fs::File>);

impl Opened {
    fn open(&mut self, path: &Path) -> Result<&std::fs::File> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        self.0.push(file);
        Ok(&self.0[self.0.len() - 1])
    }
}

/// Whether `other` holds exactly the bytes of `database`, read from its start.
fn same_bytes(mut database: &std::fs::File, other: &Path) -> Result<bool> {
    use std::io::Read;
    if database.metadata()?.len() != size(other) {
        return Ok(false);
    }
    let mut other = std::fs::File::open(other)?;
    let mut blocks = [vec![0u8; 1 << 16], vec![0u8; 1 << 16]];
    loop {
        let [ours, theirs] = &mut blocks;
        let read = database.read(ours)?;
        if read == 0 {
            return Ok(true);
        }
        other.read_exact(&mut theirs[..read])?;
        if ours[..read] != theirs[..read] {
            return Ok(false);
        }
    }
}

/// Copy the database to `original` before the first change to it. An
/// interrupted run that has staged rows keeps the copy it made; without one
/// (it was moved away) the database is copied as it stands, old tables
/// whole.
fn keep_original(
    conn: &Connection,
    opened: &mut Opened,
    path: &Path,
    original: &Path,
    started: bool,
) -> Result<()> {
    if started && original.exists() {
        return Ok(());
    }
    // Every committed row is in the main file after this.
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        bail!("the history database could not be checkpointed; nothing was changed");
    }
    let mut database = opened.open(path)?;
    if original.exists() {
        if same_bytes(database, original)? {
            return Ok(());
        }
        bail!(
            "{} exists and is not a copy of this history; move it away, then run this again",
            original.display()
        );
    }
    let partial = PathBuf::from(format!("{}.partial", original.display()));
    let mut copy = std::fs::File::create(&partial)
        .with_context(|| format!("failed to create {}", partial.display()))?;
    crate::fs::set_mode(&partial, 0o600)?;
    std::io::copy(&mut database, &mut copy)
        .with_context(|| format!("failed to copy the history to {}", partial.display()))?;
    copy.sync_all()?;
    std::fs::rename(&partial, original)
        .with_context(|| format!("failed to name {}", original.display()))?;
    Ok(())
}

const STAGING: &str = "compact_rows";
const PROGRESS: &str = "compact_progress";

/// The staging table has the shape of `readings`, which it becomes. A
/// progress row is one old table: the last row taken (`place`, the key
/// columns and id as JSON), the counts so far, and the account whose rows
/// were being walked with the time of its last read.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS compact_rows (
        id INTEGER PRIMARY KEY,
        service TEXT NOT NULL,
        account TEXT NOT NULL,
        at INTEGER NOT NULL,
        reading TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_compact_rows ON compact_rows(service, account, at);
    CREATE TABLE IF NOT EXISTS compact_progress (
        source TEXT PRIMARY KEY,
        place TEXT,
        rows INTEGER NOT NULL,
        unreadable INTEGER NOT NULL,
        kept INTEGER NOT NULL,
        service TEXT,
        account TEXT,
        confirmed INTEGER,
        done INTEGER NOT NULL,
        accounts INTEGER NOT NULL DEFAULT 0
    );";

/// What a reading says apart from when, as a digest of its stored form.
type Said = [u8; 32];

fn said(json: &str) -> Result<Said> {
    let body = body(json).context("a reading did not serialize as an object")?;
    Ok(Sha256::digest(serde_json::to_vec(&body)?).into())
}

/// The account whose rows are being walked: its newest kept row, and when
/// it was last read.
struct Open {
    row: Reading,
    said: Said,
    confirmed: i64,
}

/// The growth rule over old rows, one account after another, each oldest
/// first. Rows go to the staging table of `sink`, or are only counted.
#[derive(Default)]
struct Walk {
    open: Option<Open>,
    kept: u64,
}

impl Walk {
    fn take(&mut self, sink: Option<&Connection>, reading: Reading) -> Result<()> {
        let json = serde_json::to_string(&reading)?;
        let said = said(&json)?;
        let continues = self.open.as_ref().is_some_and(|open| {
            (open.row.service, &open.row.account) == (reading.service, &reading.account)
        });
        if !continues {
            self.close(sink)?;
        }
        match self.open.as_mut() {
            None => {
                write(&mut self.kept, sink, &reading, &json)?;
                self.open = Some(Open {
                    confirmed: reading.at,
                    row: reading,
                    said,
                });
            }
            Some(open) => {
                if reading.at < open.confirmed {
                    bail!(
                        "the old rows of {} {} are not in time order at {}; nothing was dropped",
                        reading.service.name(),
                        reading.account,
                        reading.at
                    );
                }
                let kept = keep(
                    Some(open.row.at),
                    open.said == said,
                    Some(open.confirmed),
                    reading.at,
                );
                if let Some(at) = kept.closing {
                    let closing = Reading {
                        at,
                        ..open.row.clone()
                    };
                    write(
                        &mut self.kept,
                        sink,
                        &closing,
                        &serde_json::to_string(&closing)?,
                    )?;
                }
                open.confirmed = reading.at;
                if kept.row {
                    write(&mut self.kept, sink, &reading, &json)?;
                    open.row = reading;
                    open.said = said;
                }
            }
        }
        Ok(())
    }

    /// End the open account's history with the value it held at its last
    /// read, the way a closing row ends a hold.
    fn close(&mut self, sink: Option<&Connection>) -> Result<()> {
        if let Some(open) = self.open.take()
            && open.confirmed > open.row.at
        {
            let closing = Reading {
                at: open.confirmed,
                ..open.row
            };
            write(
                &mut self.kept,
                sink,
                &closing,
                &serde_json::to_string(&closing)?,
            )?;
        }
        Ok(())
    }
}

/// Count a kept row, and stage it when there is a `sink`.
fn write(kept: &mut u64, sink: Option<&Connection>, reading: &Reading, json: &str) -> Result<()> {
    *kept += 1;
    if let Some(conn) = sink {
        conn.prepare_cached(&format!(
            "INSERT INTO {STAGING} (service, account, at, reading) VALUES (?1, ?2, ?3, ?4)"
        ))?
        .execute(params![
            reading.service.name(),
            reading.account,
            reading.at,
            json
        ])?;
    }
    Ok(())
}

/// How far the walk of one old table has come.
struct Progress {
    source: &'static str,
    place: Option<Vec<Sql>>,
    rows: u64,
    unreadable: u64,
    kept: u64,
    /// The open account (service, account) and its last read.
    open: Option<(String, String, i64)>,
    done: bool,
}

impl Progress {
    fn new(source: &OldTable) -> Self {
        Self {
            source: source.table,
            place: None,
            rows: 0,
            unreadable: 0,
            kept: 0,
            open: None,
            done: false,
        }
    }

    fn table(&self, accounts: u64) -> Table {
        Table {
            table: self.source.to_owned(),
            rows: self.rows,
            unreadable: self.unreadable,
            kept: self.kept,
            accounts,
        }
    }

    fn load(conn: &Connection, source: &OldTable) -> Result<Self> {
        let stored = conn
            .query_row(
                &format!(
                    "SELECT place, rows, unreadable, kept, service, account, confirmed, done
                     FROM {PROGRESS} WHERE source = ?1"
                ),
                [source.table],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, bool>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((place, rows, unreadable, kept, service, account, confirmed, done)) = stored
        else {
            return Ok(Self::new(source));
        };
        let place = match place {
            Some(json) => Some(
                serde_json::from_str::<Vec<serde_json::Value>>(&json)?
                    .into_iter()
                    .map(|value| match value {
                        serde_json::Value::String(text) => Ok(Sql::Text(text)),
                        serde_json::Value::Number(number) if number.is_i64() => {
                            Ok(Sql::Integer(number.as_i64().unwrap_or_default()))
                        }
                        other => bail!("an unreadable place in {PROGRESS}: {other}"),
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            None => None,
        };
        Ok(Self {
            source: source.table,
            place,
            rows,
            unreadable,
            kept,
            open: service
                .zip(account)
                .zip(confirmed)
                .map(|((s, a), c)| (s, a, c)),
            done,
        })
    }

    fn save(&self, conn: &Connection) -> Result<()> {
        let place = match &self.place {
            Some(place) => Some(serde_json::to_string(
                &place
                    .iter()
                    .map(|value| match value {
                        Sql::Text(text) => Ok(serde_json::Value::from(text.as_str())),
                        Sql::Integer(number) => Ok(serde_json::Value::from(*number)),
                        other => bail!(
                            "{}: a row is keyed by {other:?}, which cannot be walked past",
                            self.source
                        ),
                    })
                    .collect::<Result<Vec<_>>>()?,
            )?),
            None => None,
        };
        let (service, account, confirmed) = match &self.open {
            Some((service, account, confirmed)) => (Some(service), Some(account), Some(*confirmed)),
            None => (None, None, None),
        };
        conn.execute(
            &format!(
                "INSERT INTO {PROGRESS}
                    (source, place, rows, unreadable, kept, service, account, confirmed, done)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (source) DO UPDATE SET place = excluded.place, rows = excluded.rows,
                    unreadable = excluded.unreadable, kept = excluded.kept,
                    service = excluded.service, account = excluded.account,
                    confirmed = excluded.confirmed, done = excluded.done"
            ),
            params![
                self.source,
                place,
                self.rows,
                self.unreadable,
                self.kept,
                service,
                account,
                confirmed,
                self.done
            ],
        )?;
        Ok(())
    }

    /// The walk as this progress left it: the open account's newest staged
    /// row and its last read.
    fn walk(&self, conn: &Connection) -> Result<Walk> {
        let open = match &self.open {
            Some((service, account, confirmed)) => {
                let json: String = conn.query_row(
                    &format!(
                        "SELECT reading FROM {STAGING} WHERE service = ?1 AND account = ?2
                         ORDER BY at DESC, id DESC LIMIT 1"
                    ),
                    params![service, account],
                    |row| row.get(0),
                )?;
                Some(Open {
                    said: said(&json)?,
                    row: serde_json::from_str(&json)?,
                    confirmed: *confirmed,
                })
            }
            None => None,
        };
        Ok(Walk {
            open,
            kept: self.kept,
        })
    }

    /// Every old table's counts, for the report.
    fn all(conn: &Connection) -> Result<Vec<Table>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT source, rows, unreadable, kept, accounts FROM {PROGRESS} ORDER BY rowid"
        ))?;
        let tables = stmt
            .query_map([], |row| {
                Ok(Table {
                    table: row.get(0)?,
                    rows: row.get(1)?,
                    unreadable: row.get(2)?,
                    kept: row.get(3)?,
                    accounts: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tables)
    }
}

/// Take the next `batch` rows of `source` through the walk. `false` when
/// they were the last: the open account is closed and the table is done.
fn take_batch(
    conn: &Connection,
    source: &OldTable,
    progress: &mut Progress,
    walk: &mut Walk,
    sink: Option<&Connection>,
    batch: usize,
) -> Result<bool> {
    let rows = source.rows(conn, progress.place.as_deref(), batch)?;
    let more = rows.len() == batch;
    for row in rows {
        progress.rows += 1;
        match row.reading {
            Some(reading) => walk.take(sink, reading)?,
            None => progress.unreadable += 1,
        }
        progress.place = Some(row.place);
    }
    if !more {
        walk.close(sink)?;
        progress.done = true;
    }
    progress.kept = walk.kept;
    progress.open = walk.open.as_ref().map(|open| {
        (
            open.row.service.name().to_owned(),
            open.row.account.clone(),
            open.confirmed,
        )
    });
    Ok(more)
}

/// Stage one old table, a transaction per batch, from where its progress
/// row says an earlier run stopped.
fn rewrite(
    conn: &Connection,
    source: &OldTable,
    batch: usize,
    between: &mut dyn FnMut(Step, &Connection) -> Result<()>,
) -> Result<()> {
    let mut progress = Progress::load(conn, source)?;
    if progress.done {
        return Ok(());
    }
    source.ensure_index(conn)?;
    let total = source.count(conn)?;
    let mut walk = progress.walk(conn)?;
    let mut batches = 0u64;
    loop {
        let tx = conn.unchecked_transaction()?;
        let more = take_batch(conn, source, &mut progress, &mut walk, Some(conn), batch)?;
        progress.save(conn)?;
        tx.commit()?;
        batches += 1;
        if !more || batches.is_multiple_of(10) {
            notice(&format!(
                "history: {} {} of {total} rows",
                source.table, progress.rows
            ));
        }
        between(Step::Batch, conn)?;
        if !more {
            return Ok(());
        }
    }
}

/// A history as a step function: the times its value changes, with what it
/// says from then on. Rows arrive oldest first; of several at one instant
/// the last is the value.
#[derive(Default, PartialEq, Eq)]
struct Steps(Vec<(i64, Said)>);

impl Steps {
    fn push(&mut self, at: i64, said: Said) {
        match self.0.last_mut() {
            Some(last) if last.0 == at => {
                last.1 = said;
                let count = self.0.len();
                if count >= 2 && self.0[count - 2].1 == said {
                    self.0.pop();
                }
            }
            Some(last) if last.1 == said => {}
            _ => self.0.push((at, said)),
        }
    }
}

/// One account's staged rows.
#[derive(Default)]
struct Staged {
    times: Vec<i64>,
    steps: Steps,
}

/// One account's old rows, as they are read again.
#[derive(Default)]
struct Old {
    steps: Steps,
    /// Staged rows at or before the newest old row.
    passed: usize,
    last: i64,
}

/// Read every old row again and compare each account's history with what
/// was staged for it. For every (service, account):
///
/// - the two are the same step function: the same value from the same
///   instant, at every instant either has a row;
/// - every staged row stands at the time of an old row, and the last of
///   each is at the same time;
/// - no old row is further than the hold from the staged row at or before
///   it, so nothing is held longer than a live read would hold it.
///
/// Returns the accounts compared, per old table.
fn verify(conn: &Connection, sources: &[&OldTable]) -> Result<BTreeMap<&'static str, u64>> {
    let mut staged: BTreeMap<(String, String), Staged> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT service, account, at, reading FROM {STAGING} ORDER BY service, account, at, id"
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let account = staged.entry((row.get(0)?, row.get(1)?)).or_default();
            let at: i64 = row.get(2)?;
            let reading = row.get_ref(3)?.as_str()?;
            account.times.push(at);
            account.steps.push(at, said(reading)?);
        }
    }
    let mut compared = BTreeMap::new();
    for source in sources {
        notice(&format!(
            "history: comparing {} before and after",
            source.table
        ));
        let mut old: BTreeMap<(String, String), Old> = BTreeMap::new();
        let mut place: Option<Vec<Sql>> = None;
        loop {
            let rows = source.rows(conn, place.as_deref(), BATCH)?;
            let more = rows.len() == BATCH;
            for row in rows {
                place = Some(row.place);
                let Some(reading) = row.reading else {
                    continue;
                };
                let key = (reading.service.name().to_owned(), reading.account.clone());
                let differs =
                    |what: &str| anyhow::anyhow!("{} {}: {what} (at {})", key.0, key.1, reading.at);
                let after = staged.get(&key).ok_or_else(|| differs("no row was kept"))?;
                let before = old.entry(key.clone()).or_default();
                before
                    .steps
                    .push(reading.at, said(&serde_json::to_string(&reading)?)?);
                before.last = reading.at;
                while let Some(kept) = after.times.get(before.passed)
                    && *kept <= reading.at
                {
                    if *kept != reading.at {
                        return Err(differs("a kept row stands where no old row does"));
                    }
                    before.passed += 1;
                }
                match before
                    .passed
                    .checked_sub(1)
                    .map(|newest| after.times[newest])
                {
                    Some(kept) if reading.at - kept < HOLD_SECS => {}
                    _ => return Err(differs("a value is held longer than the hold")),
                }
            }
            if !more {
                break;
            }
        }
        for (key, before) in &old {
            let after = &staged[key];
            let differs = |what: &str| anyhow::anyhow!("{} {}: {what}", key.0, key.1);
            if before.passed != after.times.len() || after.times.last() != Some(&before.last) {
                return Err(differs("the kept rows do not end where the old rows end"));
            }
            if before.steps != after.steps {
                return Err(differs(
                    "the kept rows say something else than the old rows",
                ));
            }
        }
        compared.insert(source.table, old.len() as u64);
        for key in old.into_keys() {
            staged.remove(&key);
        }
    }
    if let Some((service, account)) = staged.keys().next() {
        bail!("{service} {account}: rows were kept for an account the old tables do not have");
    }
    Ok(compared)
}

/// In one transaction: the rows `readings` has follow the staged ones (they
/// are newer, and so are their ids), the staging table becomes `readings`,
/// and the old tables are dropped.
fn swap(
    conn: &Connection,
    sources: &[&OldTable],
    accounts: &BTreeMap<&'static str, u64>,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    if has_table(conn, "readings")? {
        conn.execute_batch(&format!(
            "INSERT INTO {STAGING} (service, account, at, reading)
                SELECT service, account, at, reading FROM readings ORDER BY id;
             DROP TABLE readings;"
        ))?;
    }
    conn.execute_batch(&format!(
        "ALTER TABLE {STAGING} RENAME TO readings;
         DROP INDEX idx_compact_rows;
         CREATE INDEX idx_readings_account_time ON readings(service, account, at);"
    ))?;
    for source in sources {
        conn.execute_batch(&format!("DROP TABLE {}", source.table))?;
        conn.execute(
            &format!("UPDATE {PROGRESS} SET accounts = ?1 WHERE source = ?2"),
            params![
                accounts.get(source.table).copied().unwrap_or(0),
                source.table
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{
        CodexDetail, Detail, ReadError, ResetCreditCount, Scope, Service, SessionLimit, Source,
        Span, Verdict, Window, WindowStatus,
    };
    use crate::store::Series;
    use crate::store::tests::{
        OLD_DOCUMENTS, OLD_SNAPSHOTS, latest, old_database, overage, reading, usage,
    };

    const QUICK: Tuning = Tuning {
        batch: 7,
        wait: Duration::from_millis(50),
    };

    fn go(path: &Path) -> Report {
        compact(path, &QUICK, &mut |_, _| Ok(())).unwrap()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn tables(path: &Path) -> Vec<String> {
        let store = Store::open_read_only_at(path).unwrap();
        let mut stmt = store
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Every row of `readings`, in id order.
    fn rows(path: &Path) -> Vec<(String, String, i64, String)> {
        let store = Store::open_read_only_at(path).unwrap();
        let mut stmt = store
            .conn
            .prepare("SELECT service, account, at, reading FROM readings ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn table(name: &str, rows: u64, unreadable: u64, kept: u64, accounts: u64) -> Table {
        Table {
            table: name.into(),
            rows,
            unreadable,
            kept,
            accounts,
        }
    }

    #[test]
    fn old_snapshots_become_readings_that_say_what_they_said() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(dir.path(), OLD_SNAPSHOTS);
        let bytes = std::fs::read(&path).unwrap();
        let report = go(&path);
        assert_eq!(report.outcome, Outcome::Compacted);
        assert_eq!(report.tables, [table("snapshots", 3, 0, 3, 2)]);
        assert_eq!(report.original, Some(original_of(&path)));
        assert_eq!(
            std::fs::read(original_of(&path)).unwrap(),
            bytes,
            "the original is kept as it was"
        );
        assert_eq!(report.bytes_before, bytes.len() as u64);
        assert_eq!(report.bytes_after, size(&path));
        assert_eq!(files(dir.path()), ["old.db", "old.db.pre-0.4"]);
        assert_eq!(tables(&path), ["readings"]);
        #[cfg(unix)]
        for file in [path.clone(), original_of(&path)] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }

        let store = Store::open_read_only_at(&path).unwrap();
        assert!(!store.has_old_tables());
        let history = store.since(Service::Claude, Some("acct"), 0).unwrap();
        assert_eq!(history.len(), 2);
        let first = &history[0];
        assert_eq!((first.service, first.at), (Service::Claude, 1_700_000_000));
        assert_eq!(first.session(), Some(Window::new(0.25, 10)));
        assert_eq!(first.weekly(), Some(Window::new(0.5, 20)));
        assert_eq!(
            first.limit(Span::Weekly, &Scope::All).unwrap().status,
            Some(WindowStatus {
                verdict: Verdict::AllowedWarning,
                surpassed_threshold: Some(0.5)
            }),
            "the window status the signals column held rides on its limit"
        );
        let models: Vec<_> = first
            .models()
            .map(|limit| (limit.key().unwrap().as_str(), limit.window, limit.share))
            .collect();
        assert_eq!(
            models,
            [
                ("opus48", Window::new(0.75, 30), None),
                ("fable", Window::new(0.2, 9), Some(0.5))
            ],
            "an uncanonical key is read as its model, and an unreadable bucket is dropped alone"
        );
        assert_eq!(first.models().nth(1).unwrap().source, Source::Headers);
        let claude = first.claude().unwrap();
        assert_eq!(claude.verdict, Verdict::Allowed);
        assert_eq!(claude.claim.as_deref(), Some("five_hour"));
        assert_eq!(claude.session_limit, SessionLimit::Absent);
        assert_eq!(claude.signals.claims, ["seven_day"]);
        assert_eq!(claude.signals.upgrade_paths, ["overage"]);
        assert_eq!(overage().window(first), Some(Window::new(0.1, 40)));
        assert_eq!(
            claude.overage.as_ref().unwrap().status.as_deref(),
            Some("ok")
        );
        assert!(first.has_quota() && first.above(0.05, 0.05, None));

        let second = &history[1];
        assert_eq!(second.at, 1_700_000_060);
        assert_eq!(second.session(), None);
        assert_eq!(second.verdict(), Verdict::Rejected);
        assert_eq!(
            second.claude().unwrap().profile_error.as_deref(),
            Some("usage nope")
        );
        assert_eq!(second.session_limit(), SessionLimit::Unknown);

        let recent = store.recent(Service::Claude, None, 2).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|r| r.account.as_str())
                .collect::<Vec<_>>(),
            ["dead", "acct"]
        );
        assert_eq!(
            recent[0].error,
            Some(ReadError::Http {
                status: 401,
                body: "revoked".into()
            })
        );
        assert!(recent[0].is_refused());
        let fleet = [
            (Service::Claude, "acct"),
            (Service::Claude, "dead"),
            (Service::Claude, "never read"),
            (Service::Codex, "acct"),
        ];
        assert_eq!(latest(&store, &fleet), [second.clone(), recent[0].clone()]);
        drop(store);

        // A second run has nothing to do and changes nothing.
        let bytes = std::fs::read(&path).unwrap();
        let again = go(&path);
        assert_eq!(again.outcome, Outcome::Nothing);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_table_from_before_a_column_existed_is_rewritten_without_it() {
        // 0.2: no session_limit or signals column, no index, a naive
        // timestamp, and the per-family columns, which are not read.
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(
            dir.path(),
            "CREATE TABLE snapshots (
                id INTEGER PRIMARY KEY, token_name TEXT NOT NULL, probed_at TEXT NOT NULL,
                unified_status TEXT, utilization_5h REAL, reset_5h INTEGER,
                utilization_7d REAL, reset_7d INTEGER, representative_claim TEXT,
                overage_status TEXT, utilization_overage REAL, reset_overage INTEGER,
                error TEXT, utilization_opus_7d REAL, reset_opus_7d INTEGER);
             INSERT INTO snapshots (token_name, probed_at, utilization_7d, reset_7d,
                utilization_opus_7d, reset_opus_7d)
             VALUES ('old', '2026-01-01T00:00:00.5', 0.5, 9, 0.4, 7);
             INSERT INTO snapshots (token_name, probed_at, utilization_7d)
             VALUES ('old', 'not a time', 0.6);",
        );
        // The file has no write-ahead log yet. From the first row staged
        // one stands beside it, so a read-only open takes the locked path.
        let log = PathBuf::from(format!("{}-wal", path.display()));
        let report = compact(&path, &QUICK, &mut |step, _| {
            assert_eq!(log.exists(), step != Step::OriginalKept, "{step:?}");
            Ok(())
        })
        .unwrap();
        assert!(!log.exists());
        assert_eq!(
            report.tables,
            [table("snapshots", 2, 1, 1, 1)],
            "a row without a readable time is left out"
        );
        let store = Store::open_read_only_at(&path).unwrap();
        let rows = store.recent(Service::Claude, Some("old"), 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].weekly(), Some(Window::new(0.5, 9)));
        assert_eq!(rows[0].models().count(), 0);
        assert_eq!(rows[0].verdict(), Verdict::Unknown);
        assert_eq!(rows[0].at, 1_767_225_600);
    }

    #[test]
    fn old_service_and_codex_rows_become_readings() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(dir.path(), OLD_DOCUMENTS);
        let report = go(&path);
        assert_eq!(
            report.tables,
            [
                table("codex_readings", 3, 0, 3, 2),
                table("service_readings", 5, 1, 4, 3)
            ],
            "the row of a service this build does not know is left out"
        );
        let store = Store::open_read_only_at(&path).unwrap();

        let glm = store
            .recent(Service::Zai, Some("glm"), 5)
            .unwrap()
            .remove(0);
        assert_eq!((glm.at, glm.plan.as_deref()), (100, Some("max")));
        assert_eq!(glm.session(), Some(Window::new(0.005, 1_794_448_052)));
        assert_eq!(glm.weekly(), Some(Window::new(0.01435, 1_794_879_119)));
        assert_eq!(glm.limits[0].label, "5h");
        assert_eq!(glm.limits[0].source, Source::Poll);
        assert_eq!(glm.limits[0].amount.as_ref().unwrap().unit, "credits");
        let mcp = &glm.limits[2];
        assert_eq!((mcp.span, mcp.group()), (Span::Monthly, Some("MCP tools")));
        assert_eq!(glm.detail, Detail::None);

        let kimi = store.since(Service::Kimi, None, 0).unwrap();
        assert_eq!(kimi.len(), 1);
        assert_eq!(kimi[0].limits[1].span, Span::Unknown);
        assert_eq!(kimi[0].limits[1].label, "monthly (Kimi Code share)");
        assert_eq!(kimi[0].balances[0].left_minor, 2196);

        let agy = store
            .since(Service::Antigravity, Some("a@example.com"), 0)
            .unwrap();
        assert_eq!(agy.iter().map(|r| r.at).collect::<Vec<_>>(), [120, 130]);
        assert_eq!(agy[0].scoped().len(), 2);
        assert!(!agy[0].has_quota());
        assert!(
            agy[0]
                .scoped_to(Some("gemini"))
                .unwrap()
                .above(0.05, 0.05, None)
        );
        assert_eq!(
            agy[1].error,
            Some(ReadError::Other {
                message: "sign in again: the grant was revoked".into()
            })
        );

        let work = store.since(Service::Codex, Some("work"), 0).unwrap();
        assert_eq!(work.len(), 2);
        let read = &work[0];
        assert_eq!((read.at, read.plan.as_deref()), (200, Some("plus")));
        assert_eq!(read.weekly(), Some(Window::new(0.42, 1_794_607_293)));
        assert_eq!(read.session(), Some(Window::new(0.1, 4_600)));
        assert_eq!(
            read.window(Span::Other(86_400), &Scope::All),
            Some(Window::new(0.5, 99))
        );
        let spark = read
            .limit(Span::Unknown, &Scope::Group("gpt-5.3-codex-spark".into()))
            .unwrap();
        assert_eq!(spark.label, "codex_spark");
        assert!(spark.is_rejected());
        let codex: &CodexDetail = read.codex().unwrap();
        assert_eq!(codex.email.as_deref(), Some("w@example.com"));
        assert_eq!(codex.allowed, Some(true));
        assert_eq!(codex.models[0].slug, "gpt-6-astra");
        assert_eq!(
            codex.reset_credits,
            Some(ResetCreditCount {
                available: 2,
                applicable: Some(0),
                expires_at: None,
            })
        );
        assert!(read.above(0.0, 0.0, None));
        assert_eq!(
            work[1].error,
            Some(ReadError::Edge {
                status: 403,
                attempts: 4
            })
        );
        assert_eq!(
            work[1].codex().unwrap().email.as_deref(),
            Some("hint@example.com"),
            "the stored id token's email stands in"
        );

        let fleet = [
            (Service::Codex, "gone"),
            (Service::Codex, "work"),
            (Service::Antigravity, "a@example.com"),
            (Service::Zai, "glm"),
            (Service::Kimi, "kimi"),
        ];
        let newest: Vec<_> = latest(&store, &fleet)
            .into_iter()
            .map(|r| (r.service, r.account, r.at))
            .collect();
        assert_eq!(
            newest,
            [
                (Service::Codex, "gone".to_owned(), 220),
                (Service::Codex, "work".to_owned(), 210),
                (Service::Antigravity, "a@example.com".to_owned(), 130),
                (Service::Zai, "glm".to_owned(), 100),
                (Service::Kimi, "kimi".to_owned(), 110),
            ]
        );
        let gone = store.recent(Service::Codex, Some("gone"), 1).unwrap();
        assert!(gone[0].is_refused());
        assert_eq!(
            gone[0].error.as_ref().unwrap().to_string(),
            "auth.json has no access_token"
        );
        drop(store);

        // A new read of an old account is the latest, and the history is one.
        let store = Store::open_at(&path).unwrap();
        store.insert(&usage(Service::Zai, "glm", 300, 0.3)).unwrap();
        let history = store.since(Service::Zai, Some("glm"), 0).unwrap();
        assert_eq!(history.iter().map(|r| r.at).collect::<Vec<_>>(), [100, 300]);
        assert_eq!(
            store.latest(&[(Service::Zai, "glm")]).unwrap(),
            [usage(Service::Zai, "glm", 300, 0.3)]
        );
    }

    /// What two accounts read, as (seconds after the start, weekly use): a
    /// read every 20 s with holds, steps, a stretch with no reads, two reads
    /// in one second, and an account whose last read was held.
    fn reads() -> Vec<(&'static str, i64, f64)> {
        let mut reads = Vec::new();
        for tick in 0..=200i64 {
            let at = tick * 20;
            // `busy`: 0.10 for 25 minutes, a step, a climb, then flat.
            let used = match tick {
                0..=75 => 0.10,
                76..=90 => 0.12,
                91..=120 => 0.12 + (tick - 90) as f64 * 0.001,
                _ => 0.15,
            };
            // Nothing read for an hour in the middle of the flat stretch.
            let at = if tick > 150 { at + 3_600 } else { at };
            reads.push(("busy", at, used));
            if tick == 100 {
                reads.push(("busy", at, 0.5));
            }
            // `idle` never moves, and is read off the 20 s grid.
            if tick % 3 == 0 {
                reads.push(("idle", at + 7, 0.33));
            }
        }
        reads
    }

    const START: i64 = 1_700_000_000;

    /// An old `snapshots` table holding [`reads`], inserted in time order
    /// across the accounts, the way a daemon wrote them.
    fn dense(dir: &Path) -> PathBuf {
        let mut sql = OLD_SNAPSHOTS
            .split_once("INSERT")
            .map(|(schema, _)| schema.to_owned())
            .unwrap();
        let mut reads = reads();
        reads.sort_by_key(|(_, at, _)| *at);
        for (account, at, used) in reads {
            let time = chrono::DateTime::from_timestamp(START + at, 0)
                .unwrap()
                .to_rfc3339();
            sql.push_str(&format!(
                "INSERT INTO snapshots (token_name, probed_at, unified_status, utilization_7d, \
                 reset_7d) VALUES ('{account}', '{time}', 'allowed', {used}, 9000000);"
            ));
        }
        old_database(dir, &sql)
    }

    /// (account, seconds after the start, weekly use) of each `readings` row.
    fn weekly_rows(path: &Path) -> Vec<(String, i64, f64)> {
        rows(path)
            .into_iter()
            .map(|(_, account, at, json)| {
                let reading: Reading = serde_json::from_str(&json).unwrap();
                assert_eq!((reading.at, &reading.account), (at, &account));
                (account, at - START, reading.weekly().unwrap().used)
            })
            .collect()
    }

    #[test]
    fn dense_old_rows_are_kept_as_a_live_store_would_have_kept_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dense(dir.path());
        let report = go(&path);
        let all = reads().len() as u64;
        let kept = weekly_rows(&path);
        assert_eq!(
            report.tables,
            [table("snapshots", all, 0, kept.len() as u64, 2)]
        );
        assert!(kept.len() < reads().len() / 2, "{} rows kept", kept.len());

        // The same reads through `Store::insert`, as they were made.
        let live = dir.path().join("live.db");
        let store = Store::open_at(&live).unwrap();
        let mut made = reads();
        made.sort_by_key(|(_, at, _)| *at);
        let as_read = |account: &str, at: i64, used: f64| {
            let mut reading = Reading::empty(Service::Claude, account, START + at);
            reading.set_weekly(Some(Window::new(used, 9_000_000)));
            reading
        };
        for (account, at, used) in &made {
            store.insert(&as_read(account, *at, *used)).unwrap();
        }
        // A live history ends each account with the value it holds now.
        let mut expected = Vec::new();
        for account in ["busy", "idle"] {
            expected.extend(
                store
                    .since(Service::Claude, Some(account), 0)
                    .unwrap()
                    .into_iter()
                    .map(|r| (r.account.clone(), r.at - START, r.weekly().unwrap().used)),
            );
        }
        drop(store);
        assert_eq!(kept, expected);
        // What the rule kept, spelled out for the account that never moves:
        // a row every ten minutes of reads, and its last read.
        let idle: Vec<i64> = kept
            .iter()
            .filter(|(account, _, _)| account == "idle")
            .map(|(_, at, _)| *at)
            .collect();
        assert_eq!(idle, [7, 607, 1207, 1807, 2407, 3007, 6667, 7267, 7567]);

        // The history reads the same through the store as from every row.
        let store = Store::open_read_only_at(&path).unwrap();
        let history = store.since(Service::Claude, Some("busy"), 0).unwrap();
        let stats = crate::store::Stats::compute("busy", &history);
        let every: Vec<Reading> = made
            .iter()
            .filter(|(account, _, _)| *account == "busy")
            .map(|(account, at, used)| as_read(account, *at, *used))
            .collect();
        let dense = crate::store::Stats::compute("busy", &every);
        let (kept, all) = (&stats.limits[0], &dense.limits[0]);
        assert_eq!(kept.series, Series::weekly());
        assert!((kept.mean.unwrap() - all.mean.unwrap()).abs() < 1e-9);
        assert!((kept.peak.unwrap() - all.peak.unwrap()).abs() < 1e-9);
        assert_eq!(kept.latest, all.latest);
    }

    /// Old tables of every kind, and rows `readings` already has.
    fn mixed(dir: &Path) -> PathBuf {
        let path = dense(dir);
        let store = Store::open_at(&path).unwrap();
        store.conn.execute_batch(OLD_DOCUMENTS).unwrap();
        let mut new = reading();
        new.account = "busy".into();
        new.at = START + 20_000;
        store.insert(&new).unwrap();
        new.at += 60;
        store.insert(&new).unwrap();
        store
            .insert(&usage(Service::Zai, "glm", START, 0.3))
            .unwrap();
        drop(store);
        path
    }

    fn confirmed(path: &Path) -> Vec<(String, String, i64)> {
        let store = Store::open_read_only_at(path).unwrap();
        let mut stmt = store
            .conn
            .prepare("SELECT service, account, at FROM confirmed ORDER BY 1, 2")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn rows_readings_already_has_follow_the_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = mixed(dir.path());
        let before = rows(&path);
        let seen = confirmed(&path);
        assert_eq!(before.len(), 2);
        go(&path);
        let after = rows(&path);
        assert_eq!(
            after[after.len() - 2..],
            before[..],
            "the newer rows are last"
        );
        assert_eq!(confirmed(&path), seen);
        assert_eq!(tables(&path), ["confirmed", "readings"]);
        let store = Store::open_read_only_at(&path).unwrap();
        let mut newest = reading();
        newest.account = "busy".into();
        newest.at = START + 20_060;
        assert_eq!(
            store.latest(&[(Service::Claude, "busy")]).unwrap(),
            [newest.clone()],
            "the stored view is the newest read, as of its confirmation"
        );
        let history = store.since(Service::Claude, Some("busy"), 0).unwrap();
        assert_eq!(history.first().unwrap().at, START);
        assert_eq!(history.last().unwrap(), &newest);
        assert!(history.windows(2).all(|pair| pair[0].at <= pair[1].at));
        let glm = store.since(Service::Zai, Some("glm"), 0).unwrap();
        assert_eq!(
            glm.iter().map(|r| r.at).collect::<Vec<_>>(),
            [100, START],
            "an account's old and new rows are one history"
        );
        // The index the accessors use is there under its name.
        let index: String = store
            .conn
            .query_row(
                "SELECT tbl_name FROM sqlite_master WHERE name = 'idx_readings_account_time'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, "readings");
    }

    #[test]
    fn a_run_interrupted_anywhere_is_finished_by_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let source = mixed(dir.path());
        let bytes = std::fs::read(&source).unwrap();
        let whole = dir.path().join("whole.db");
        std::fs::copy(&source, &whole).unwrap();
        let mut steps = Vec::new();
        let report = compact(&whole, &QUICK, &mut |step, _| {
            steps.push(step);
            Ok(())
        })
        .unwrap();
        let expected = rows(&whole);
        assert!(steps.len() > 40, "{} steps", steps.len());
        assert_eq!(
            [steps[0], steps[steps.len() - 2], steps[steps.len() - 1]],
            [Step::OriginalKept, Step::Swapped, Step::Vacuumed]
        );

        for stop in 0..steps.len() {
            let path = dir.path().join(format!("cut-{stop}.db"));
            std::fs::copy(&source, &path).unwrap();
            // Killed at `stop`, and the run after that two steps later
            // (when it has that many left).
            let mut finished = None;
            for (stop, must) in [(stop, true), (2, false)] {
                let mut reached = 0;
                let cut = compact(&path, &QUICK, &mut |_, _| {
                    reached += 1;
                    if reached > stop {
                        bail!("killed");
                    }
                    Ok(())
                });
                match cut {
                    Err(error) => assert_eq!(error.to_string(), "killed"),
                    Ok(report) => {
                        assert!(!must);
                        finished = Some(report);
                    }
                }
            }
            let resumed = finished.unwrap_or_else(|| go(&path));
            assert_eq!(resumed.outcome, Outcome::Compacted, "cut at {stop}");
            assert_eq!(resumed.tables, report.tables, "cut at {stop}");
            assert_eq!(rows(&path), expected, "cut at {stop}");
            assert_eq!(tables(&path), ["confirmed", "readings"], "cut at {stop}");
            assert_eq!(
                std::fs::read(original_of(&path)).unwrap(),
                bytes,
                "cut at {stop}: the original is the database as it was"
            );
            assert_eq!(resumed.bytes_before, bytes.len() as u64);
            assert_eq!(size(&path), size(&whole), "cut at {stop}");
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_file(original_of(&path)).unwrap();
        }
        assert_eq!(
            files(dir.path()),
            ["old.db", "whole.db", "whole.db.pre-0.4"],
            "no partial copy or sidecar is left"
        );
    }

    #[test]
    fn a_dry_run_counts_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = mixed(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        let rehearsed = run_at(&path, true).unwrap();
        assert_eq!(rehearsed.outcome, Outcome::Rehearsed);
        assert_eq!(rehearsed.original, None);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(files(dir.path()), ["old.db"]);

        let real = go(&path);
        let counts = |report: &Report| -> Vec<_> {
            report
                .tables
                .iter()
                .map(|table| {
                    (
                        table.table.clone(),
                        table.rows,
                        table.unreadable,
                        table.kept,
                    )
                })
                .collect()
        };
        assert_eq!(counts(&rehearsed), counts(&real));
        assert_eq!(
            real.tables.iter().map(|t| t.accounts).sum::<u64>(),
            2 + 2 + 3
        );

        // Nothing to rehearse once it is done, or where there is no history.
        assert_eq!(run_at(&path, true).unwrap().outcome, Outcome::Nothing);
        let missing = dir.path().join("none.db");
        for dry_run in [true, false] {
            assert_eq!(run_at(&missing, dry_run).unwrap().outcome, Outcome::Nothing);
        }
        assert!(!missing.exists());
    }

    #[test]
    fn the_database_is_held_exclusively_or_the_run_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = mixed(dir.path());
        let bytes = std::fs::read(&path).unwrap();

        // Another process has the history open: nothing is touched.
        let other = Store::open_at(&path).unwrap();
        let refused = compact(&path, &QUICK, &mut |_, _| Ok(())).unwrap_err();
        assert!(
            refused.to_string().contains("in use by another tokeman"),
            "{refused:#}"
        );
        assert!(!original_of(&path).exists());
        assert_eq!(tables(&path).len(), 5);
        drop(other);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        // While it runs, no other connection reads or writes the file.
        let mut refusals = 0;
        compact(&path, &QUICK, &mut |_, _| {
            let other = Connection::open(&path)?;
            other.busy_timeout(Duration::ZERO)?;
            let read = other.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| {
                row.get::<_, i64>(0)
            });
            let write = other.execute_batch("CREATE TABLE intruder (id INTEGER)");
            for error in [read.err(), write.err()] {
                assert!(error.is_some_and(|error| is_busy(&error)));
                refusals += 1;
            }
            Ok(())
        })
        .unwrap();
        assert!(refusals > 80, "{refusals}");
        assert_eq!(tables(&path), ["confirmed", "readings"]);
        // Released when it is done.
        Store::open_at(&path)
            .unwrap()
            .insert(&usage(Service::Kimi, "kimi", START, 0.1))
            .unwrap();
    }

    const OTHER_PROCESS_DB: &str = "TOKEMAN_TEST_OTHER_PROCESS_DB";

    /// Not a test of its own: run as a child process by the test below, it
    /// tries to read and to write the database named in the environment,
    /// without waiting, and exits 0 when sqlite refused both as busy.
    #[test]
    #[ignore = "the child process of another_process_is_kept_out_for_the_whole_run"]
    fn other_process() {
        let Some(path) = std::env::var_os(OTHER_PROCESS_DB) else {
            return;
        };
        let conn = Connection::open(path).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        let read = conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        });
        let write = conn.execute_batch("CREATE TABLE intruder (id INTEGER); DROP TABLE intruder;");
        let refused = |error: Option<rusqlite::Error>| error.is_some_and(|error| is_busy(&error));
        std::process::exit(if refused(read.err()) && refused(write.err()) {
            0
        } else {
            3
        });
    }

    /// Whether another process is refused the database at `path`.
    fn kept_out(path: &Path) -> bool {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::compact::tests::other_process"])
            .args(["--ignored", "--nocapture"])
            .env(OTHER_PROCESS_DB, path)
            .output()
            .unwrap();
        match child.status.code() {
            Some(0) => true,
            Some(3) => false,
            other => panic!("{other:?}: {}", String::from_utf8_lossy(&child.stderr)),
        }
    }

    /// sqlite's lock on a file belongs to the process, and the process gives
    /// it up by closing any descriptor of that file. Connections in one
    /// process do not see that; another process does.
    #[test]
    fn another_process_is_kept_out_for_the_whole_run() {
        let dir = tempfile::tempdir().unwrap();
        // One database with a write-ahead log, one with a rollback journal.
        for path in [mixed(dir.path()), {
            let dir = dir.path().join("journal");
            std::fs::create_dir(&dir).unwrap();
            dense(&dir)
        }] {
            assert!(!kept_out(&path), "the child can tell being let in");
            let tuning = Tuning {
                batch: 60,
                wait: Duration::from_millis(50),
            };
            // A lock once given up stays given up, so the steps after the
            // batches answer for every batch.
            let mut asked = Vec::new();
            compact(&path, &tuning, &mut |step, _| {
                if !asked.contains(&step) {
                    assert!(kept_out(&path), "{step:?}");
                    asked.push(step);
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(asked.len(), 5);
            assert!(!kept_out(&path), "released when it is done");
            assert!(tables(&path).contains(&"readings".to_owned()));
        }
    }

    #[test]
    fn a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay() {
        let dir = tempfile::tempdir().unwrap();
        let source = mixed(dir.path());
        for (damage, what) in [
            (
                "DELETE FROM compact_rows WHERE account = 'idle' AND at = 1700000607",
                "held longer than the hold",
            ),
            (
                "UPDATE compact_rows SET at = at + 1 WHERE account = 'busy' AND at = 1700001520",
                "where no old row does",
            ),
            (
                "UPDATE compact_rows SET reading = replace(reading, '0.33', '0.34')
                 WHERE account = 'idle' AND at = 1700001207",
                "say something else",
            ),
            (
                "DELETE FROM compact_rows WHERE account = 'idle' AND at = 1700007567",
                "do not end where the old rows end",
            ),
            (
                "DELETE FROM compact_rows WHERE account = 'gone'",
                "no row was kept",
            ),
            (
                "INSERT INTO compact_rows (service, account, at, reading)
                 SELECT service, 'invented', at, reading FROM compact_rows LIMIT 1",
                "an account the old tables do not have",
            ),
        ] {
            let path = dir.path().join("damaged.db");
            std::fs::copy(&source, &path).unwrap();
            let before = tables(&path);
            let error = compact(&path, &QUICK, &mut |step, conn| {
                if step == Step::Rewritten {
                    assert_eq!(conn.execute(damage, [])?, 1, "{damage}");
                }
                Ok(())
            })
            .unwrap_err();
            let said = format!("{error:#}");
            assert!(said.contains(what), "{damage}: {said}");
            assert!(said.contains("nothing was dropped"), "{said}");
            let mut left = tables(&path);
            left.retain(|table| table != PROGRESS);
            assert_eq!(left, before, "{damage}");
            assert_eq!(rows(&path).len(), 2);
            // The next run starts over and finishes.
            assert_eq!(go(&path).outcome, Outcome::Compacted);
            assert_eq!(tables(&path), ["confirmed", "readings"]);
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_file(original_of(&path)).unwrap();
        }
    }

    #[test]
    fn the_errors_old_snapshots_held_as_text_keep_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        let schema = OLD_SNAPSHOTS.split_once("INSERT").unwrap().0;
        let row = |account: &str, error: &str| {
            format!(
                "INSERT INTO snapshots (token_name, probed_at, unified_status, error)
                 VALUES ('{account}', '2026-01-01T00:00:00+00:00', 'unknown', '{error}');"
            )
        };
        let path = old_database(
            dir.path(),
            &[
                schema.to_owned(),
                row("none", "no usable credential"),
                row("offline", "network: timed out"),
                row("odd", "something this build has no kind for"),
            ]
            .concat(),
        );
        assert_eq!(go(&path).tables, [table("snapshots", 3, 0, 3, 3)]);
        let store = Store::open_read_only_at(&path).unwrap();
        let error = |account: &str| {
            let mut rows = store.recent(Service::Claude, Some(account), 1).unwrap();
            rows.remove(0).error.unwrap()
        };
        // A missing credential stays a refusal; a timeout stays transient.
        assert_eq!(error("none"), ReadError::NoCredential);
        assert!(error("none").is_definitive());
        assert_eq!(
            error("offline"),
            ReadError::Network {
                message: "timed out".into()
            }
        );
        assert_eq!(
            error("odd"),
            ReadError::Other {
                message: "something this build has no kind for".into()
            }
        );
    }

    #[test]
    fn old_rows_out_of_time_order_stop_the_run_with_nothing_dropped() {
        // `snapshots` is walked in the order of its text timestamps. The
        // second row sorts after the first as text and is ninety minutes
        // before it as a time.
        let dir = tempfile::tempdir().unwrap();
        let schema = OLD_SNAPSHOTS.split_once("INSERT").unwrap().0;
        let path = old_database(
            dir.path(),
            &format!(
                "{schema}
                 INSERT INTO snapshots (token_name, probed_at, utilization_7d, reset_7d)
                 VALUES ('acct', '2026-01-01T00:00:00+00:00', 0.5, 9);
                 INSERT INTO snapshots (token_name, probed_at, utilization_7d, reset_7d)
                 VALUES ('acct', '2026-01-01T00:30:00+02:00', 0.4, 9);"
            ),
        );
        let error = compact(&path, &QUICK, &mut |_, _| Ok(())).unwrap_err();
        let said = format!("{error:#}");
        assert!(said.contains("not in time order"), "{said}");
        assert!(tables(&path).contains(&"snapshots".to_owned()));
        assert!(!tables(&path).contains(&"readings".to_owned()));
        let store = Store::open_read_only_at(&path).unwrap();
        let old: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(old, 2);
    }

    #[test]
    fn a_file_in_the_originals_place_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(dir.path(), OLD_SNAPSHOTS);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(original_of(&path), b"something else").unwrap();
        let error = compact(&path, &QUICK, &mut |_, _| Ok(())).unwrap_err();
        assert!(error.to_string().contains("is not a copy of this history"));
        assert_eq!(
            std::fs::read(original_of(&path)).unwrap(),
            b"something else"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // A copy of this very database (a run killed right after making it)
        // is the original.
        std::fs::write(original_of(&path), &bytes).unwrap();
        assert_eq!(go(&path).outcome, Outcome::Compacted);
        assert_eq!(std::fs::read(original_of(&path)).unwrap(), bytes);
    }

    #[test]
    fn steps_are_the_changes_of_value() {
        let said = |byte: u8| [byte; 32];
        let steps = |rows: &[(i64, u8)]| {
            let mut steps = Steps::default();
            for (at, value) in rows {
                steps.push(*at, said(*value));
            }
            steps.0
        };
        assert_eq!(
            steps(&[(1, 7), (2, 7), (3, 8), (9, 8), (9, 7)]),
            [(1, said(7)), (3, said(8)), (9, said(7))]
        );
        // Of two rows at one instant the last is the value.
        assert_eq!(steps(&[(1, 7), (5, 8), (5, 7), (6, 7)]), [(1, said(7))]);
        assert_eq!(
            steps(&[(1, 7), (1, 8), (2, 8)]),
            [(1, said(8))],
            "from the first row on"
        );
    }
}
