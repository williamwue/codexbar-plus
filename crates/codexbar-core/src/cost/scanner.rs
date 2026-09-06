//! Local session-log scanners.
//!
//! Codex: `Vendored/CostUsage/CostUsageScanner.swift:2059-2082` (roots) and `:4880-4928`
//! (parser), accumulator semantics `:700-815`.
//! Claude: `Vendored/CostUsage/CostUsageScanner+Claude.swift:109-225`.
//!
//! Both formats report **cumulative** or **repeated** numbers, so the scanners are not a
//! naive sum:
//! - Codex emits `token_count` events whose `info.total_token_usage` is the running total
//!   for the session. Deltas come from a watermark; a total that goes *down* means the
//!   context was compacted or the session restarted, so the new total counts in full.
//!   Verified against real logs: summing `last_token_usage` over one 6 MB session
//!   over-counts by ~40 % because events repeat, while the watermark rule reconciles.
//! - Claude streams partial assistant messages; the final chunk for a
//!   `message.id` + `requestId` pair wins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::{Date, OffsetDateTime};

use super::pricing::{self, RequestTokens};

/// One priced request, ready to aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    /// `codex` or `claude`.
    pub provider: &'static str,
    /// UTC day, `YYYY-MM-DD`.
    pub day: Date,
    pub model: String,
    pub tokens: RequestTokens,
    /// `None` when the model has no known price.
    pub cost_usd: Option<f64>,
    pub session_id: Option<String>,
}

impl UsageEvent {
    pub fn total_tokens(&self) -> u64 {
        self.tokens.uncached_input
            + self.tokens.cache_read
            + self.tokens.cache_write
            + self.tokens.output
    }
}

/// Result of scanning one file, including the state needed to resume.
#[derive(Debug, Clone, Default)]
pub struct FileScan {
    pub events: Vec<UsageEvent>,
    /// Bytes consumed; a later scan starts here when the file only grew.
    pub parsed_bytes: u64,
    /// Codex cumulative watermark to carry into the next scan.
    pub watermark: Option<CodexTotals>,
    /// Claude message keys already counted, so a resumed scan cannot double count.
    pub seen_keys: Vec<String>,
}

/// Codex `info.total_token_usage` / `last_token_usage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexTotals {
    pub input: u64,
    pub cached_input: u64,
    pub cache_write_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
    pub total: u64,
}

impl CodexTotals {
    fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let get = |key: &str| object.get(key).and_then(Value::as_u64).unwrap_or(0);
        // Upstream also accepts `cache_read_input_tokens` and takes the max.
        let cached = get("cached_input_tokens").max(get("cache_read_input_tokens"));
        Some(Self {
            input: get("input_tokens"),
            cached_input: cached,
            cache_write_input: get("cache_write_input_tokens")
                .max(get("cache_creation_input_tokens")),
            output: get("output_tokens"),
            reasoning_output: get("reasoning_output_tokens"),
            total: get("total_tokens"),
        })
    }

    fn magnitude(&self) -> u64 {
        if self.total > 0 {
            self.total
        } else {
            self.input + self.output
        }
    }

    /// Monotonic difference; `None` when nothing changed.
    fn delta_from(&self, previous: &Self) -> Option<Self> {
        let sub = |now: u64, before: u64| now.saturating_sub(before);
        let delta = Self {
            input: sub(self.input, previous.input),
            cached_input: sub(self.cached_input, previous.cached_input),
            cache_write_input: sub(self.cache_write_input, previous.cache_write_input),
            output: sub(self.output, previous.output),
            reasoning_output: sub(self.reasoning_output, previous.reasoning_output),
            total: sub(self.total, previous.total),
        };
        (delta.input + delta.output + delta.cached_input + delta.cache_write_input > 0)
            .then_some(delta)
    }

    /// Splits cumulative counters into billing buckets.
    ///
    /// Codex's `input_tokens` includes the cached ones, and `reasoning_output_tokens` is a
    /// subset of `output_tokens` (upstream caps it), so neither is added twice.
    fn to_request_tokens(self) -> RequestTokens {
        RequestTokens {
            uncached_input: self
                .input
                .saturating_sub(self.cached_input)
                .saturating_sub(self.cache_write_input),
            cache_read: self.cached_input,
            cache_write: self.cache_write_input,
            output: self.output,
        }
    }
}

/// Codex session roots: `CODEX_HOME/sessions` plus its sibling `archived_sessions`.
pub fn codex_roots() -> Vec<PathBuf> {
    let Some(home) = crate::paths::codex_home() else {
        return Vec::new();
    };
    vec![home.join("sessions"), home.join("archived_sessions")]
}

/// Claude session roots: `CLAUDE_CONFIG_DIR/projects` (upstream globs `**/*.jsonl`).
pub fn claude_roots() -> Vec<PathBuf> {
    match crate::paths::claude_config_root() {
        Some(root) => vec![root.join("projects")],
        None => Vec::new(),
    }
}

/// Recursively collects `.jsonl` files under `roots`.
pub fn discover(roots: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(&path, out),
                Ok(kind) if kind.is_file() => {
                    if path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                    {
                        out.push(path);
                    }
                }
                _ => {}
            }
        }
    }

    let mut out = Vec::new();
    for root in roots {
        walk(root, &mut out);
    }
    out.sort();
    out
}

fn parse_day(raw: &str) -> Option<Date> {
    OffsetDateTime::parse(raw.trim(), &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|dt| dt.to_offset(time::UtcOffset::UTC).date())
}

/// Scans a Codex session file from `resume_bytes`, continuing from `watermark`.
pub fn scan_codex_file(
    path: &Path,
    resume_bytes: u64,
    watermark: Option<CodexTotals>,
) -> std::io::Result<FileScan> {
    let text = read_from(path, resume_bytes)?;
    let mut scan = FileScan {
        parsed_bytes: resume_bytes + text.len() as u64,
        watermark,
        ..FileScan::default()
    };

    let mut model: Option<String> = None;
    let mut session_id: Option<String> = None;

    for line in text.lines() {
        let Ok(root) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = root.get("payload").unwrap_or(&Value::Null);

        // Model is announced by turn_context / thread_settings and applies to later events.
        if let Some(found) = payload
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| payload.get("model_name").and_then(Value::as_str))
            .or_else(|| {
                payload
                    .get("thread_settings")
                    .and_then(|s| s.get("model"))
                    .and_then(Value::as_str)
            })
        {
            model = Some(found.to_string());
        }
        if session_id.is_none() {
            session_id = payload
                .get("session_id")
                .and_then(Value::as_str)
                .or_else(|| payload.get("id").and_then(Value::as_str))
                .map(str::to_owned);
        }

        if root.get("type").and_then(Value::as_str) != Some("event_msg")
            || payload.get("type").and_then(Value::as_str) != Some("token_count")
        {
            continue;
        }
        let Some(info) = payload.get("info").filter(|v| !v.is_null()) else {
            continue;
        };

        // `info.model` wins when present (upstream precedence).
        let event_model = info
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| info.get("model_name").and_then(Value::as_str))
            .map(str::to_owned)
            .or_else(|| model.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let Some(day) = root
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_day)
        else {
            continue;
        };

        let delta = match info
            .get("total_token_usage")
            .and_then(CodexTotals::from_json)
        {
            Some(totals) => {
                let delta = match scan.watermark {
                    // A total that shrank means compaction or a fresh context: count it whole.
                    Some(previous) if totals.magnitude() >= previous.magnitude() => {
                        totals.delta_from(&previous)
                    }
                    Some(_) | None => Some(totals),
                };
                scan.watermark = Some(totals);
                delta
            }
            // Older Codex builds emit only the per-request numbers.
            None => info
                .get("last_token_usage")
                .and_then(CodexTotals::from_json),
        };

        let Some(delta) = delta.filter(|d| d.input + d.output > 0) else {
            continue;
        };
        let tokens = delta.to_request_tokens();
        scan.events.push(UsageEvent {
            provider: "codex",
            day,
            cost_usd: pricing::cost_for(&event_model, tokens),
            model: event_model,
            tokens,
            session_id: session_id.clone(),
        });
    }

    Ok(scan)
}

/// Scans a Claude session file from `resume_bytes`, skipping already-counted messages.
pub fn scan_claude_file(
    path: &Path,
    resume_bytes: u64,
    seen: &[String],
) -> std::io::Result<FileScan> {
    let text = read_from(path, resume_bytes)?;
    let mut scan = FileScan {
        parsed_bytes: resume_bytes + text.len() as u64,
        seen_keys: seen.to_vec(),
        ..FileScan::default()
    };

    // Streaming chunks repeat a message with growing usage; the last one wins.
    let mut latest: HashMap<String, UsageEvent> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for line in text.lines() {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(root) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if root.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message) = root.get("message") else {
            continue;
        };
        let Some(usage) = message.get("usage") else {
            continue;
        };

        let Some(day) = root
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_day)
        else {
            continue;
        };

        let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let ephemeral_1h = usage
            .get("cache_creation")
            .and_then(|c| c.get("ephemeral_1h_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let tokens = RequestTokens {
            uncached_input: get("input_tokens"),
            cache_read: get("cache_read_input_tokens"),
            cache_write: get("cache_creation_input_tokens").max(ephemeral_1h),
            output: get("output_tokens"),
        };
        if tokens.uncached_input + tokens.cache_read + tokens.cache_write + tokens.output == 0 {
            continue;
        }

        let model = message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let session_id = root
            .get("sessionId")
            .and_then(Value::as_str)
            .or_else(|| root.get("session_id").and_then(Value::as_str))
            .or_else(|| {
                root.get("metadata")
                    .and_then(|m| m.get("sessionId"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);

        let key = format!(
            "{}:{}",
            message.get("id").and_then(Value::as_str).unwrap_or(""),
            root.get("requestId").and_then(Value::as_str).unwrap_or("")
        );
        // Messages with no ids at all cannot be de-duplicated; keep them distinct.
        let key = if key == ":" {
            format!("anon:{}:{}", order.len(), path.display())
        } else {
            key
        };
        if scan.seen_keys.contains(&key) {
            continue;
        }

        if !latest.contains_key(&key) {
            order.push(key.clone());
        }
        latest.insert(
            key,
            UsageEvent {
                provider: "claude",
                day,
                cost_usd: pricing::cost_for(&model, tokens),
                model,
                tokens,
                session_id,
            },
        );
    }

    for key in order {
        if let Some(event) = latest.remove(&key) {
            scan.events.push(event);
            scan.seen_keys.push(key);
        }
    }

    Ok(scan)
}

/// Reads a file from `offset`, lossily decoding so one bad byte cannot stop a scan.
///
/// A file shorter than `offset` was rotated or truncated, so it is read from the start.
fn read_from(path: &Path, offset: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if offset > 0 && offset <= len {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn write(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("codexbar-scanner-test")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CODEX_LINES: &[&str] = &[
        r#"{"timestamp":"2026-09-01T10:00:00.000Z","type":"session_meta","payload":{"session_id":"s-1","id":"s-1"}}"#,
        r#"{"timestamp":"2026-09-01T10:00:01.000Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#,
        r#"{"timestamp":"2026-09-01T10:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":100,"total_tokens":1100},"last_token_usage":{"input_tokens":1000,"output_tokens":100}}}}"#,
        r#"{"timestamp":"2026-09-01T10:00:03.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":900,"output_tokens":250,"total_tokens":3250},"last_token_usage":{"input_tokens":2000,"output_tokens":150}}}}"#,
    ];

    #[test]
    fn codex_totals_are_treated_as_cumulative() {
        let dir = temp_dir("codex-cumulative");
        let path = write(&dir, "rollout.jsonl", CODEX_LINES);

        let scan = scan_codex_file(&path, 0, None).unwrap();
        assert_eq!(scan.events.len(), 2);

        // First event counts in full.
        assert_eq!(scan.events[0].tokens.uncached_input, 1000);
        assert_eq!(scan.events[0].tokens.output, 100);
        // Second is the difference, with the cached part split out.
        assert_eq!(scan.events[1].tokens.uncached_input, 2000 - 900);
        assert_eq!(scan.events[1].tokens.cache_read, 900);
        assert_eq!(scan.events[1].tokens.output, 150);
        assert_eq!(scan.events[1].day, date!(2026 - 09 - 01));
        assert_eq!(scan.events[1].model, "gpt-5.6-sol");
        assert!(scan.events[1].cost_usd.unwrap() > 0.0);
        assert_eq!(scan.watermark.unwrap().total, 3250);
    }

    #[test]
    fn a_shrinking_total_counts_in_full_after_compaction() {
        let dir = temp_dir("codex-compaction");
        let mut lines = CODEX_LINES.to_vec();
        // Context compacted: totals restart low.
        lines.push(
            r#"{"timestamp":"2026-09-01T11:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":40,"total_tokens":540}}}}"#,
        );
        let path = write(&dir, "rollout.jsonl", &lines);

        let scan = scan_codex_file(&path, 0, None).unwrap();
        assert_eq!(scan.events.len(), 3);
        assert_eq!(
            scan.events[2].tokens.uncached_input, 500,
            "post-reset total counts whole"
        );
        assert_eq!(scan.events[2].tokens.output, 40);
    }

    #[test]
    fn repeated_identical_totals_produce_no_event() {
        let dir = temp_dir("codex-repeat");
        let mut lines = CODEX_LINES.to_vec();
        lines.push(lines[3]); // exact re-emission
        let path = write(&dir, "rollout.jsonl", &lines);

        let scan = scan_codex_file(&path, 0, None).unwrap();
        assert_eq!(scan.events.len(), 2, "a duplicate total adds nothing");
    }

    #[test]
    fn resuming_a_grown_file_only_counts_new_events() {
        let dir = temp_dir("codex-resume");
        let path = write(&dir, "rollout.jsonl", &CODEX_LINES[..3]);
        let first = scan_codex_file(&path, 0, None).unwrap();
        assert_eq!(first.events.len(), 1);

        // Append the next event and resume from the recorded offset and watermark.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push('\n');
        text.push_str(CODEX_LINES[3]);
        std::fs::write(&path, &text).unwrap();

        let second = scan_codex_file(&path, first.parsed_bytes, first.watermark).unwrap();
        assert_eq!(second.events.len(), 1, "only the appended event");
        assert_eq!(second.events[0].tokens.uncached_input, 2000 - 900);
    }

    #[test]
    fn unknown_models_still_report_tokens_without_a_price() {
        let dir = temp_dir("codex-unknown-model");
        let path = write(
            &dir,
            "rollout.jsonl",
            &[
                r#"{"timestamp":"2026-09-01T10:00:01.000Z","type":"turn_context","payload":{"model":"acme-experimental-9"}}"#,
                r#"{"timestamp":"2026-09-01T10:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}}"#,
            ],
        );
        let scan = scan_codex_file(&path, 0, None).unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].cost_usd, None, "no invented price");
        assert_eq!(scan.events[0].total_tokens(), 12);
    }

    const CLAUDE_LINES: &[&str] = &[
        r#"{"type":"user","timestamp":"2026-09-01T10:00:00.000Z","message":{"role":"user"}}"#,
        r#"{"type":"assistant","timestamp":"2026-09-01T10:00:01.000Z","sessionId":"c-1","requestId":"req-1","message":{"id":"msg-1","model":"claude-opus-4-6","usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":300,"output_tokens":10}}}"#,
        r#"{"type":"assistant","timestamp":"2026-09-01T10:00:02.000Z","sessionId":"c-1","requestId":"req-1","message":{"id":"msg-1","model":"claude-opus-4-6","usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":300,"output_tokens":55}}}"#,
        r#"{"type":"assistant","timestamp":"2026-09-02T10:00:03.000Z","sessionId":"c-1","requestId":"req-2","message":{"id":"msg-2","model":"claude-sonnet-4-5-20250929","usage":{"input_tokens":50,"output_tokens":5}}}"#,
    ];

    #[test]
    fn claude_streaming_chunks_collapse_to_the_final_usage() {
        let dir = temp_dir("claude-stream");
        let path = write(&dir, "session.jsonl", CLAUDE_LINES);

        let scan = scan_claude_file(&path, 0, &[]).unwrap();
        assert_eq!(scan.events.len(), 2, "two requests, not three chunks");

        let first = &scan.events[0];
        assert_eq!(first.tokens.output, 55, "last chunk wins");
        assert_eq!(first.tokens.uncached_input, 100);
        assert_eq!(first.tokens.cache_read, 300);
        assert_eq!(first.tokens.cache_write, 20);
        assert_eq!(first.session_id.as_deref(), Some("c-1"));
        let expected = 100.0 * 5e-6 + 300.0 * 5e-7 + 20.0 * 6.25e-6 + 55.0 * 2.5e-5;
        assert!((first.cost_usd.unwrap() - expected).abs() < 1e-12);

        assert_eq!(scan.events[1].day, date!(2026 - 09 - 02));
        assert!(
            scan.events[1].cost_usd.is_some(),
            "dated model name resolves"
        );
    }

    #[test]
    fn claude_resume_skips_messages_already_counted() {
        let dir = temp_dir("claude-resume");
        let path = write(&dir, "session.jsonl", CLAUDE_LINES);
        let first = scan_claude_file(&path, 0, &[]).unwrap();
        assert_eq!(first.events.len(), 2);

        // Re-scan the same bytes with the recorded keys: nothing new.
        let second = scan_claude_file(&path, 0, &first.seen_keys).unwrap();
        assert!(second.events.is_empty());
    }

    #[test]
    fn discovery_walks_nested_directories_and_ignores_other_files() {
        let dir = temp_dir("discovery");
        write(&dir.join("2026/09/01"), "a.jsonl", &["{}"]);
        write(&dir.join("2026/09/02"), "b.jsonl", &["{}"]);
        write(&dir, "notes.txt", &["ignore me"]);

        let found = discover(&[dir.clone()]);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.extension().unwrap() == "jsonl"));
    }

    #[test]
    fn truncated_files_are_reread_from_the_start() {
        let dir = temp_dir("truncated");
        let path = write(&dir, "rollout.jsonl", CODEX_LINES);
        let full = scan_codex_file(&path, 0, None).unwrap();

        // Simulate rotation: file is now shorter than the recorded offset.
        write(&dir, "rollout.jsonl", &CODEX_LINES[..3]);
        let scan = scan_codex_file(&path, full.parsed_bytes, None).unwrap();
        assert_eq!(scan.events.len(), 1, "rotated file is read from the start");
    }
}
