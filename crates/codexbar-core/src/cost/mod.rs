//! Local cost history: scan session logs, price them, aggregate into SQLite.
//!
//! Windows port of `Sources/CodexBarCore/Vendored/CostUsage/**`. Everything here is local:
//! no network, no provider API. That is what makes it work for Codex and Claude, whose
//! plans do not expose per-request spend.

pub mod pricing;
pub mod scanner;
pub mod store;

use std::path::PathBuf;

use time::Date;

pub use pricing::{cost_for, rates_for, RequestTokens};
pub use scanner::{CodexTotals, UsageEvent};
pub use store::{CostError, CostStore, CostSummary, DayTotals};

/// What one scan run did, for logs and the CLI.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ScanReport {
    pub files_seen: usize,
    pub files_scanned: usize,
    pub events: usize,
    pub skipped: usize,
    pub pruned_rows: usize,
}

/// Scans every Codex and Claude session log into `store`, resuming where it left off.
///
/// Files whose size and mtime are unchanged since the last run are skipped outright; the
/// rest resume from their recorded offset. This is what keeps a refresh cheap when one
/// 6 MB session grows by a few lines.
pub fn scan_all(store: &mut CostStore, today: Date) -> Result<ScanReport, CostError> {
    let mut report = ScanReport::default();

    for (provider, roots) in [
        ("codex", scanner::codex_roots()),
        ("claude", scanner::claude_roots()),
    ] {
        for path in scanner::discover(&roots) {
            report.files_seen += 1;
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            let size = metadata.len();
            let mtime_ms = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let (resume_bytes, watermark, seen_keys) = store.resume_state(&path, size)?;
            if resume_bytes == size && resume_bytes > 0 {
                report.skipped += 1;
                continue;
            }

            let scanned = match provider {
                "codex" => scanner::scan_codex_file(&path, resume_bytes, watermark),
                _ => scanner::scan_claude_file(&path, resume_bytes, &seen_keys),
            };
            let scan = match scanned {
                Ok(scan) => scan,
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "skipping unreadable session log");
                    continue;
                }
            };

            report.files_scanned += 1;
            report.events += scan.events.len();
            // A full re-read replaces the file's rows; a resumed read appends.
            store.apply_scan(provider, &path, size, mtime_ms, &scan, resume_bytes == 0)?;
        }
    }

    report.pruned_rows = store.enforce_retention(today)?;
    Ok(report)
}

/// Convenience for the CLI and the tray: scan, then summarise a rolling window.
pub fn scan_and_summarize(days: u16, today: Date) -> Result<(ScanReport, CostSummary), CostError> {
    let mut store = CostStore::open_default()?;
    let report = scan_all(&mut store, today)?;
    let summary = store.summary(days, today)?;
    Ok((report, summary))
}

/// Where the database lives, for `diagnose`.
pub fn database_path() -> Option<PathBuf> {
    CostStore::default_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_report_serializes_for_the_cli() {
        let json = serde_json::to_string(&ScanReport {
            files_seen: 4,
            files_scanned: 2,
            events: 91,
            skipped: 2,
            pruned_rows: 0,
        })
        .unwrap();
        assert!(json.contains("\"files_scanned\":2"));
        assert!(json.contains("\"events\":91"));
    }

    #[test]
    fn roots_follow_the_provider_home_overrides() {
        // CODEX_HOME is honoured by paths::codex_home, so the roots must sit under it.
        if let Some(home) = crate::paths::codex_home() {
            let roots = scanner::codex_roots();
            assert_eq!(roots.len(), 2);
            assert!(roots[0].starts_with(&home));
            assert!(roots[1].ends_with("archived_sessions"));
        }
    }
}
