//! Cost history store.
//!
//! Windows counterpart of `Vendored/CostUsage/CostUsageStore.swift:662-825` and
//! `CostUsageStore+Retention.swift:20-180`. Same engine (SQLite, WAL, busy timeout), same
//! budgets (25,000 retained rows / 256 MiB, `CostUsageStore+CodexCache.swift:46-47`), and
//! the same incremental-resume idea: per-file `parsed_bytes` plus scanner state.
//!
//! Deliberately a *subset* of upstream's 13 tables. Upstream additionally persists
//! `token_snapshots`, `usage_rows`, `buffered_lines`, `fork_lineage`, `discovery_state`,
//! `lookback_state` and `accumulators` to support fork-aware re-attribution and partial
//! line buffering. This port keeps `files` (resume state), `file_day_aggregates`
//! (recomputable per file) and `day_aggregates` (what the UI reads), which is what the
//! displayed today / 7-day / 30-day figures actually need. Anything omitted is omitted on
//! purpose, not forgotten.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use time::Date;

use super::scanner::{self, CodexTotals, UsageEvent};

/// `CostUsageStore+CodexCache.swift:46-47`.
pub const MAX_RETAINED_ROWS: i64 = 25_000;
pub const MAX_DB_BYTES: i64 = 256 * 1024 * 1024;
/// Aggregates older than this are pruned; upstream keeps a rolling day window.
pub const RETAIN_DAYS: i64 = 400;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CostError {
    #[error("could not resolve %LOCALAPPDATA% for the cost database")]
    NoLocation,
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A day/model row as the UI and CLI consume it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DayTotals {
    pub day: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    /// `None` when every request in the row used an unpriced model.
    pub cost_usd: Option<f64>,
}

impl DayTotals {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens
    }
}

/// Aggregated answer for a window, e.g. "last 30 days".
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CostSummary {
    pub days: Vec<DayTotals>,
    pub cost_usd: f64,
    /// True when some rows had no price, so `cost_usd` is a lower bound.
    pub partial: bool,
    pub total_tokens: u64,
    pub requests: u64,
}

pub struct CostStore {
    connection: Connection,
    path: PathBuf,
}

impl CostStore {
    /// `%LOCALAPPDATA%\CodexBar\cache\cost-usage\cost-usage.sqlite`
    /// (upstream: `~/Library/Caches/CodexBar/cost-usage/cost-usage.sqlite`).
    pub fn default_path() -> Option<PathBuf> {
        crate::paths::cache_dir().map(|dir| dir.join("cost-usage").join("cost-usage.sqlite"))
    }

    pub fn open_default() -> Result<Self, CostError> {
        Self::open(&Self::default_path().ok_or(CostError::NoLocation)?)
    }

    pub fn open(path: &Path) -> Result<Self, CostError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CostError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let connection = Connection::open(path)?;
        // Same durability posture as upstream: WAL for a single writer with readers, a
        // busy timeout so a concurrent CLI run waits instead of failing.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self {
            connection,
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, CostError> {
        let connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let store = Self {
            connection,
            path: PathBuf::from(":memory:"),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<(), CostError> {
        self.connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                id            INTEGER PRIMARY KEY,
                provider      TEXT    NOT NULL,
                path          TEXT    NOT NULL UNIQUE,
                size          INTEGER NOT NULL,
                mtime_ms      INTEGER NOT NULL,
                parsed_bytes  INTEGER NOT NULL,
                watermark     TEXT,
                seen_keys     TEXT,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS files_provider ON files(provider);

            CREATE TABLE IF NOT EXISTS file_day_aggregates (
                file_id      INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                day          TEXT    NOT NULL,
                model        TEXT    NOT NULL,
                input        INTEGER NOT NULL,
                cache_read   INTEGER NOT NULL,
                cache_write  INTEGER NOT NULL,
                output       INTEGER NOT NULL,
                requests     INTEGER NOT NULL,
                cost_nanos   INTEGER,
                PRIMARY KEY (file_id, day, model)
            );
            CREATE INDEX IF NOT EXISTS file_day_aggregates_day ON file_day_aggregates(day);

            CREATE TABLE IF NOT EXISTS day_aggregates (
                day          TEXT    NOT NULL,
                provider     TEXT    NOT NULL,
                model        TEXT    NOT NULL,
                input        INTEGER NOT NULL,
                cache_read   INTEGER NOT NULL,
                cache_write  INTEGER NOT NULL,
                output       INTEGER NOT NULL,
                requests     INTEGER NOT NULL,
                cost_nanos   INTEGER,
                PRIMARY KEY (day, provider, model)
            );
            CREATE INDEX IF NOT EXISTS day_aggregates_day ON day_aggregates(day);
            "#,
        )?;
        self.connection.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Records one file's scan: replaces that file's aggregates, then rebuilds the affected
    /// day totals from every file. Rebuilding by day keeps `day_aggregates` correct when a
    /// file is re-read after truncation.
    pub fn apply_scan(
        &mut self,
        provider: &str,
        path: &Path,
        size: u64,
        mtime_ms: i64,
        scan: &scanner::FileScan,
        replace_file_rows: bool,
    ) -> Result<(), CostError> {
        let watermark = scan
            .watermark
            .map(|w| serde_json::to_string(&WatermarkRow::from(w)).unwrap_or_default());
        let seen_keys = if scan.seen_keys.is_empty() {
            None
        } else {
            // Bound the resume set: Claude message ids are only needed for recent files.
            let tail: Vec<&String> = scan.seen_keys.iter().rev().take(5_000).collect();
            Some(serde_json::to_string(&tail).unwrap_or_default())
        };
        let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;

        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO files(provider, path, size, mtime_ms, parsed_bytes, watermark, seen_keys, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET
                 provider = excluded.provider,
                 size = excluded.size,
                 mtime_ms = excluded.mtime_ms,
                 parsed_bytes = excluded.parsed_bytes,
                 watermark = excluded.watermark,
                 seen_keys = excluded.seen_keys,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                provider,
                path_key(path),
                size as i64,
                mtime_ms,
                scan.parsed_bytes as i64,
                watermark,
                seen_keys,
                now_ms
            ],
        )?;
        let file_id: i64 = tx.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path_key(path)],
            |row| row.get(0),
        )?;

        if replace_file_rows {
            tx.execute(
                "DELETE FROM file_day_aggregates WHERE file_id = ?1",
                params![file_id],
            )?;
        }

        // Fold the events into (day, model) buckets before touching the database.
        let mut buckets: HashMap<(String, String), Bucket> = HashMap::new();
        for event in &scan.events {
            let key = (event.day.to_string(), event.model.clone());
            let bucket = buckets.entry(key).or_default();
            bucket.add(event);
        }

        for ((day, model), bucket) in &buckets {
            tx.execute(
                "INSERT INTO file_day_aggregates
                     (file_id, day, model, input, cache_read, cache_write, output, requests, cost_nanos)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(file_id, day, model) DO UPDATE SET
                     input = input + excluded.input,
                     cache_read = cache_read + excluded.cache_read,
                     cache_write = cache_write + excluded.cache_write,
                     output = output + excluded.output,
                     requests = requests + excluded.requests,
                     cost_nanos = CASE
                         WHEN cost_nanos IS NULL THEN excluded.cost_nanos
                         WHEN excluded.cost_nanos IS NULL THEN cost_nanos
                         ELSE cost_nanos + excluded.cost_nanos
                     END",
                params![
                    file_id,
                    day,
                    model,
                    bucket.input as i64,
                    bucket.cache_read as i64,
                    bucket.cache_write as i64,
                    bucket.output as i64,
                    bucket.requests as i64,
                    bucket.cost_nanos
                ],
            )?;
        }

        let touched_days: Vec<String> = if replace_file_rows {
            tx.prepare("SELECT DISTINCT day FROM file_day_aggregates WHERE file_id = ?1")?
                .query_map(params![file_id], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        } else {
            buckets.keys().map(|(day, _)| day.clone()).collect()
        };

        for day in touched_days {
            rebuild_day(&tx, &day)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Resume state for a file: `(parsed_bytes, watermark, seen_keys)`.
    ///
    /// Returns zeroes when the file is unknown, or when it shrank since the last scan
    /// (rotation), which forces a full re-read.
    pub fn resume_state(
        &self,
        path: &Path,
        current_size: u64,
    ) -> Result<(u64, Option<CodexTotals>, Vec<String>), CostError> {
        let row: Option<(i64, Option<String>, Option<String>, i64)> = self
            .connection
            .query_row(
                "SELECT parsed_bytes, watermark, seen_keys, size FROM files WHERE path = ?1",
                params![path_key(path)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        let Some((parsed_bytes, watermark, seen_keys, _size)) = row else {
            return Ok((0, None, Vec::new()));
        };
        if (current_size as i64) < parsed_bytes {
            return Ok((0, None, Vec::new()));
        }
        let watermark = watermark
            .and_then(|raw| serde_json::from_str::<WatermarkRow>(&raw).ok())
            .map(CodexTotals::from);
        let seen_keys = seen_keys
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default();
        Ok((parsed_bytes.max(0) as u64, watermark, seen_keys))
    }

    /// Day/model rows in `[from, to]`, newest day first.
    pub fn totals_between(&self, from: Date, to: Date) -> Result<Vec<DayTotals>, CostError> {
        let mut statement = self.connection.prepare(
            "SELECT day, provider, model, input, cache_read, cache_write, output, requests, cost_nanos
             FROM day_aggregates
             WHERE day >= ?1 AND day <= ?2
             ORDER BY day DESC, provider ASC, model ASC",
        )?;
        let rows = statement
            .query_map(params![from.to_string(), to.to_string()], |row| {
                let cost_nanos: Option<i64> = row.get(8)?;
                Ok(DayTotals {
                    day: row.get(0)?,
                    provider: row.get(1)?,
                    model: row.get(2)?,
                    input_tokens: row.get::<_, i64>(3)? as u64,
                    cache_read_tokens: row.get::<_, i64>(4)? as u64,
                    cache_write_tokens: row.get::<_, i64>(5)? as u64,
                    output_tokens: row.get::<_, i64>(6)? as u64,
                    requests: row.get::<_, i64>(7)? as u64,
                    cost_usd: cost_nanos.map(|nanos| nanos as f64 / 1e9),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rolling window ending today (inclusive), e.g. `summary(1)` = today.
    pub fn summary(&self, days: u16, today: Date) -> Result<CostSummary, CostError> {
        let from = today - time::Duration::days((days.max(1) - 1) as i64);
        let rows = self.totals_between(from, today)?;

        let mut summary = CostSummary {
            partial: rows.iter().any(|r| r.cost_usd.is_none()),
            ..CostSummary::default()
        };
        for row in &rows {
            summary.cost_usd += row.cost_usd.unwrap_or(0.0);
            summary.total_tokens += row.total_tokens();
            summary.requests += row.requests;
        }
        summary.days = rows;
        Ok(summary)
    }

    /// Prunes old aggregates and orphaned files, then checks the size budget.
    ///
    /// Returns the number of deleted `day_aggregates` rows. Mirrors upstream's day-window
    /// retention: newest days always survive, and the row/byte budgets bound the file.
    pub fn enforce_retention(&mut self, today: Date) -> Result<usize, CostError> {
        let cutoff = (today - time::Duration::days(RETAIN_DAYS)).to_string();
        let tx = self.connection.transaction()?;
        let mut deleted =
            tx.execute("DELETE FROM day_aggregates WHERE day < ?1", params![cutoff])?;
        tx.execute(
            "DELETE FROM file_day_aggregates WHERE day < ?1",
            params![cutoff],
        )?;

        // Row budget: drop the oldest days until the table fits.
        let rows: i64 =
            tx.query_row("SELECT COUNT(*) FROM day_aggregates", [], |row| row.get(0))?;
        if rows > MAX_RETAINED_ROWS {
            let over = rows - MAX_RETAINED_ROWS;
            deleted += tx.execute(
                "DELETE FROM day_aggregates WHERE rowid IN (
                     SELECT rowid FROM day_aggregates ORDER BY day ASC LIMIT ?1
                 )",
                params![over],
            )?;
        }

        // Files whose aggregates are all gone and that no longer exist on disk.
        let stale: Vec<(i64, String)> = tx
            .prepare(
                "SELECT id, path FROM files
                 WHERE id NOT IN (SELECT DISTINCT file_id FROM file_day_aggregates)",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (id, path) in stale {
            if !Path::new(&path).exists() {
                tx.execute("DELETE FROM files WHERE id = ?1", params![id])?;
            }
        }
        tx.commit()?;

        if self.size_bytes() > MAX_DB_BYTES {
            tracing::warn!(
                bytes = self.size_bytes(),
                budget = MAX_DB_BYTES,
                "cost database exceeds its budget; compacting"
            );
            self.connection.execute_batch("VACUUM")?;
        }
        Ok(deleted)
    }

    pub fn size_bytes(&self) -> i64 {
        std::fs::metadata(&self.path)
            .map(|m| m.len() as i64)
            .unwrap_or(0)
    }
}

/// Canonical key for a session file.
///
/// The same file can arrive with different spellings — `CODEX_HOME` may use forward or
/// back slashes, and callers may pass relative paths — which would otherwise create a
/// second `files` row and double every aggregate. Canonicalising first makes the
/// `path` UNIQUE constraint mean what it says.
fn path_key(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    // Windows canonicalisation prepends the \\?\ verbatim prefix; drop it for readability.
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('/', "\\").to_lowercase()
}

/// Recomputes one day's provider/model totals from every file's aggregates.
fn rebuild_day(tx: &rusqlite::Transaction<'_>, day: &str) -> Result<(), CostError> {
    tx.execute("DELETE FROM day_aggregates WHERE day = ?1", params![day])?;
    tx.execute(
        "INSERT INTO day_aggregates
             (day, provider, model, input, cache_read, cache_write, output, requests, cost_nanos)
         SELECT a.day, f.provider, a.model,
                SUM(a.input), SUM(a.cache_read), SUM(a.cache_write), SUM(a.output),
                SUM(a.requests), SUM(a.cost_nanos)
         FROM file_day_aggregates a
         JOIN files f ON f.id = a.file_id
         WHERE a.day = ?1
         GROUP BY a.day, f.provider, a.model",
        params![day],
    )?;
    Ok(())
}

#[derive(Debug, Default)]
struct Bucket {
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    requests: u64,
    cost_nanos: Option<i64>,
}

impl Bucket {
    fn add(&mut self, event: &UsageEvent) {
        self.input += event.tokens.uncached_input;
        self.cache_read += event.tokens.cache_read;
        self.cache_write += event.tokens.cache_write;
        self.output += event.tokens.output;
        self.requests += 1;
        if let Some(cost) = event.cost_usd {
            let nanos = (cost * 1e9).round() as i64;
            self.cost_nanos = Some(self.cost_nanos.unwrap_or(0) + nanos);
        }
    }
}

/// Serialised Codex watermark.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct WatermarkRow {
    input: u64,
    cached_input: u64,
    cache_write_input: u64,
    output: u64,
    reasoning_output: u64,
    total: u64,
}

impl From<CodexTotals> for WatermarkRow {
    fn from(value: CodexTotals) -> Self {
        Self {
            input: value.input,
            cached_input: value.cached_input,
            cache_write_input: value.cache_write_input,
            output: value.output,
            reasoning_output: value.reasoning_output,
            total: value.total,
        }
    }
}

impl From<WatermarkRow> for CodexTotals {
    fn from(value: WatermarkRow) -> Self {
        Self {
            input: value.input,
            cached_input: value.cached_input,
            cache_write_input: value.cache_write_input,
            output: value.output,
            reasoning_output: value.reasoning_output,
            total: value.total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::pricing::RequestTokens;
    use time::macros::date;

    fn event(day: Date, model: &str, input: u64, output: u64, cost: Option<f64>) -> UsageEvent {
        UsageEvent {
            provider: "codex",
            day,
            model: model.to_string(),
            tokens: RequestTokens {
                uncached_input: input,
                output,
                ..RequestTokens::default()
            },
            cost_usd: cost,
            session_id: None,
        }
    }

    fn scan(events: Vec<UsageEvent>, parsed_bytes: u64) -> scanner::FileScan {
        scanner::FileScan {
            events,
            parsed_bytes,
            ..scanner::FileScan::default()
        }
    }

    #[test]
    fn schema_uses_wal_and_records_its_version() {
        let dir = std::env::temp_dir().join("codexbar-cost-store-wal");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cost.sqlite");
        let store = CostStore::open(&path).unwrap();

        let mode: String = store
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let version: String = store
            .connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, "1");

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregates_roll_up_by_day_provider_and_model() {
        let mut store = CostStore::open_in_memory().unwrap();
        let day = date!(2026 - 09 - 01);
        store
            .apply_scan(
                "codex",
                Path::new("a.jsonl"),
                10,
                1,
                &scan(
                    vec![
                        event(day, "gpt-5.6-sol", 1_000, 100, Some(0.005)),
                        event(day, "gpt-5.6-sol", 2_000, 200, Some(0.010)),
                        event(day, "gpt-5.4", 500, 50, Some(0.001)),
                    ],
                    10,
                ),
                true,
            )
            .unwrap();

        let rows = store.totals_between(day, day).unwrap();
        assert_eq!(rows.len(), 2, "two models");
        let sol = rows.iter().find(|r| r.model == "gpt-5.6-sol").unwrap();
        assert_eq!(sol.input_tokens, 3_000);
        assert_eq!(sol.output_tokens, 300);
        assert_eq!(sol.requests, 2);
        assert!((sol.cost_usd.unwrap() - 0.015).abs() < 1e-9);
    }

    #[test]
    fn two_files_on_the_same_day_sum_into_one_row() {
        let mut store = CostStore::open_in_memory().unwrap();
        let day = date!(2026 - 09 - 01);
        for name in ["a.jsonl", "b.jsonl"] {
            store
                .apply_scan(
                    "codex",
                    Path::new(name),
                    10,
                    1,
                    &scan(vec![event(day, "gpt-5.6-sol", 1_000, 100, Some(0.005))], 10),
                    true,
                )
                .unwrap();
        }
        let rows = store.totals_between(day, day).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 2_000);
        assert_eq!(rows[0].requests, 2);
    }

    #[test]
    fn rescanning_a_file_replaces_its_contribution_instead_of_doubling() {
        let mut store = CostStore::open_in_memory().unwrap();
        let day = date!(2026 - 09 - 01);
        let file = Path::new("a.jsonl");
        let events = vec![event(day, "gpt-5.6-sol", 1_000, 100, Some(0.005))];

        store
            .apply_scan("codex", file, 10, 1, &scan(events.clone(), 10), true)
            .unwrap();
        store
            .apply_scan("codex", file, 10, 1, &scan(events, 10), true)
            .unwrap();

        let rows = store.totals_between(day, day).unwrap();
        assert_eq!(rows[0].input_tokens, 1_000, "a full rescan is idempotent");
        assert_eq!(rows[0].requests, 1);
    }

    #[test]
    fn incremental_append_adds_to_the_existing_row() {
        let mut store = CostStore::open_in_memory().unwrap();
        let day = date!(2026 - 09 - 01);
        let file = Path::new("a.jsonl");

        store
            .apply_scan(
                "codex",
                file,
                10,
                1,
                &scan(vec![event(day, "gpt-5.6-sol", 1_000, 100, Some(0.005))], 10),
                true,
            )
            .unwrap();
        // Resumed scan: only the new events, appended rather than replacing.
        store
            .apply_scan(
                "codex",
                file,
                20,
                2,
                &scan(vec![event(day, "gpt-5.6-sol", 500, 50, Some(0.002))], 20),
                false,
            )
            .unwrap();

        let rows = store.totals_between(day, day).unwrap();
        assert_eq!(rows[0].input_tokens, 1_500);
        assert_eq!(rows[0].requests, 2);
        assert!((rows[0].cost_usd.unwrap() - 0.007).abs() < 1e-9);
    }

    #[test]
    fn resume_state_round_trips_and_resets_on_truncation() {
        let mut store = CostStore::open_in_memory().unwrap();
        let file = Path::new("a.jsonl");
        let mut file_scan = scan(vec![], 4_096);
        file_scan.watermark = Some(CodexTotals {
            input: 10,
            cached_input: 4,
            cache_write_input: 0,
            output: 2,
            reasoning_output: 1,
            total: 12,
        });
        file_scan.seen_keys = vec!["msg-1:req-1".into()];
        store
            .apply_scan("claude", file, 4_096, 7, &file_scan, true)
            .unwrap();

        let (bytes, watermark, keys) = store.resume_state(file, 8_192).unwrap();
        assert_eq!(bytes, 4_096);
        assert_eq!(watermark.unwrap().total, 12);
        assert_eq!(keys, vec!["msg-1:req-1".to_string()]);

        // Rotated file (now smaller than what we parsed) forces a full re-read.
        let (bytes, watermark, keys) = store.resume_state(file, 100).unwrap();
        assert_eq!(bytes, 0);
        assert!(watermark.is_none());
        assert!(keys.is_empty());

        // Unknown file starts from scratch.
        let (bytes, _, _) = store.resume_state(Path::new("nope.jsonl"), 10).unwrap();
        assert_eq!(bytes, 0);
    }

    #[test]
    fn the_same_file_spelled_differently_is_one_row() {
        let dir = std::env::temp_dir().join("codexbar-cost-path-key");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        let real = dir.join("sessions").join("rollout.jsonl");
        std::fs::write(&real, b"{}").unwrap();

        // Same file, three spellings: backslashes, forward slashes, different case.
        let back = PathBuf::from(real.to_string_lossy().replace('/', "\\"));
        let forward = PathBuf::from(real.to_string_lossy().replace('\\', "/"));
        let upper = PathBuf::from(real.to_string_lossy().to_uppercase());

        let mut store = CostStore::open_in_memory().unwrap();
        let day = date!(2026 - 09 - 01);
        for path in [back, forward, upper] {
            store
                .apply_scan(
                    "codex",
                    &path,
                    2,
                    1,
                    &scan(vec![event(day, "gpt-5.6-sol", 1_000, 100, Some(0.005))], 2),
                    true,
                )
                .unwrap();
        }

        let files: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(files, 1, "one file on disk must be one row");
        let rows = store.totals_between(day, day).unwrap();
        assert_eq!(
            rows[0].requests, 1,
            "totals must not multiply by path spelling"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_windows_are_inclusive_and_flag_missing_prices() {
        let mut store = CostStore::open_in_memory().unwrap();
        let today = date!(2026 - 09 - 10);
        store
            .apply_scan(
                "codex",
                Path::new("a.jsonl"),
                10,
                1,
                &scan(
                    vec![
                        event(today, "gpt-5.6-sol", 1_000, 100, Some(0.01)),
                        event(
                            today - time::Duration::days(6),
                            "gpt-5.6-sol",
                            1_000,
                            100,
                            Some(0.02),
                        ),
                        event(
                            today - time::Duration::days(40),
                            "gpt-5.6-sol",
                            1_000,
                            100,
                            Some(0.04),
                        ),
                        event(today, "acme-9", 1_000, 100, None),
                    ],
                    10,
                ),
                true,
            )
            .unwrap();

        let day_summary = store.summary(1, today).unwrap();
        assert!((day_summary.cost_usd - 0.01).abs() < 1e-9);
        assert!(
            day_summary.partial,
            "an unpriced model makes the total a lower bound"
        );

        let week = store.summary(7, today).unwrap();
        assert!((week.cost_usd - 0.03).abs() < 1e-9, "{}", week.cost_usd);

        let month = store.summary(30, today).unwrap();
        assert!(
            (month.cost_usd - 0.03).abs() < 1e-9,
            "40 days ago is outside the window"
        );

        let quarter = store.summary(90, today).unwrap();
        assert!((quarter.cost_usd - 0.07).abs() < 1e-9);
    }

    #[test]
    fn retention_prunes_days_beyond_the_window() {
        let mut store = CostStore::open_in_memory().unwrap();
        let today = date!(2026 - 09 - 10);
        let ancient = today - time::Duration::days(RETAIN_DAYS + 5);
        store
            .apply_scan(
                "codex",
                Path::new("a.jsonl"),
                10,
                1,
                &scan(
                    vec![
                        event(today, "gpt-5.6-sol", 10, 1, Some(0.001)),
                        event(ancient, "gpt-5.6-sol", 10, 1, Some(0.001)),
                    ],
                    10,
                ),
                true,
            )
            .unwrap();
        assert_eq!(store.totals_between(ancient, today).unwrap().len(), 2);

        let deleted = store.enforce_retention(today).unwrap();
        assert_eq!(deleted, 1);
        let remaining = store.totals_between(ancient, today).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].day, today.to_string());
    }

    #[test]
    fn budgets_match_upstream() {
        assert_eq!(MAX_RETAINED_ROWS, 25_000);
        assert_eq!(MAX_DB_BYTES, 256 * 1024 * 1024);
    }
}
