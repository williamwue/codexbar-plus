//! Windows path resolution for every local source CodexBar reads.
//!
//! Upstream resolves these from `$HOME` (`ClaudeConfigPaths.swift:52-60`,
//! `CodexLocalDataScope.swift:21-46`, `Config/CodexBarConfigStore.swift:76-116`).
//! The CLI tools themselves use the same dotted directory names on Windows, so
//! `%USERPROFILE%\.codex` / `.claude` are the correct targets; only CodexBar's own
//! config and cache move to the Windows-native locations.

use std::env;
use std::path::{Path, PathBuf};

/// `%USERPROFILE%`, falling back to `%HOMEDRIVE%%HOMEPATH%`, then `$HOME`.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(p) = non_empty("USERPROFILE") {
        return Some(PathBuf::from(p));
    }
    if let (Some(drive), Some(path)) = (non_empty("HOMEDRIVE"), non_empty("HOMEPATH")) {
        return Some(PathBuf::from(format!("{drive}{path}")));
    }
    non_empty("HOME").map(PathBuf::from)
}

/// `%APPDATA%` (roaming) — user settings that should follow the profile.
pub fn appdata_dir() -> Option<PathBuf> {
    non_empty("APPDATA").map(PathBuf::from)
}

/// `%LOCALAPPDATA%` — machine-local caches, equivalent of `~/Library/Caches`.
pub fn local_appdata_dir() -> Option<PathBuf> {
    non_empty("LOCALAPPDATA").map(PathBuf::from)
}

fn non_empty(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// CodexBar's own config file.
///
/// Resolution order mirrors upstream (`CodexBarConfigStore.swift:76-116`) and appends the
/// Windows-native location so a fresh install lands in `%APPDATA%`:
/// 1. `CODEXBAR_CONFIG_PATH`
/// 2. `XDG_CONFIG_HOME/codexbar/config.json`
/// 3. `%APPDATA%\CodexBar\config.json`
/// 4. `~/.config/codexbar/config.json`
/// 5. legacy `~/.codexbar/config.json`
pub fn config_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(explicit) = non_empty("CODEXBAR_CONFIG_PATH") {
        out.push(PathBuf::from(explicit));
    }
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME") {
        out.push(Path::new(&xdg).join("codexbar").join("config.json"));
    }
    if let Some(appdata) = appdata_dir() {
        out.push(appdata.join("CodexBar").join("config.json"));
    }
    if let Some(home) = home_dir() {
        out.push(home.join(".config").join("codexbar").join("config.json"));
        out.push(home.join(".codexbar").join("config.json"));
    }
    out
}

/// The config path new installs write to (first candidate that is writable-by-construction).
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(explicit) = non_empty("CODEXBAR_CONFIG_PATH") {
        return Some(PathBuf::from(explicit));
    }
    appdata_dir()
        .map(|p| p.join("CodexBar").join("config.json"))
        .or_else(|| home_dir().map(|h| h.join(".codexbar").join("config.json")))
}

/// Cache root. Upstream: `~/Library/Caches/CodexBar` (`CostUsageStore.swift:168-183`).
pub fn cache_dir() -> Option<PathBuf> {
    local_appdata_dir()
        .or_else(home_dir)
        .map(|p| p.join("CodexBar").join("cache"))
}

/// Codex CLI home: `CODEX_HOME` else `%USERPROFILE%\.codex`
/// (upstream `CodexLocalDataScope.swift:21-46`).
///
/// `CODEX_HOME` names the home itself, not its parent, so `auth.json` sits directly
/// inside it — matching how the Codex CLI treats the variable.
pub fn codex_home() -> Option<PathBuf> {
    resolve_root(non_empty("CODEX_HOME"), home_dir(), ".codex")
}

pub fn codex_auth_file() -> Option<PathBuf> {
    codex_home().map(|h| h.join("auth.json"))
}

/// Claude config root: `CLAUDE_CONFIG_DIR` else `%USERPROFILE%\.claude`
/// (upstream `ClaudeConfigPaths.swift:9-17,62-68`).
pub fn claude_config_root() -> Option<PathBuf> {
    resolve_root(non_empty("CLAUDE_CONFIG_DIR"), home_dir(), ".claude")
}

/// Claude credentials file, honouring the secure-storage override
/// (upstream `ClaudeConfigPaths.swift:36-50`).
pub fn claude_credentials_file() -> Option<PathBuf> {
    let root = match non_empty("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => claude_config_root()?,
    };
    Some(root.join(".credentials.json"))
}

/// An explicit override wins verbatim; otherwise the tool's dotted directory under home.
fn resolve_root(explicit: Option<String>, home: Option<PathBuf>, dotted: &str) -> Option<PathBuf> {
    match explicit {
        Some(value) => Some(PathBuf::from(value)),
        None => home.map(|h| h.join(dotted)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_candidates_prefer_explicit_then_appdata() {
        // Only assert ordering invariants that hold regardless of the host environment.
        let candidates = config_candidates();
        assert!(
            !candidates.is_empty(),
            "expected at least one candidate path"
        );
        let appdata_idx = candidates
            .iter()
            .position(|p| p.components().any(|c| c.as_os_str() == "CodexBar"));
        let legacy_idx = candidates
            .iter()
            .position(|p| p.components().any(|c| c.as_os_str() == ".codexbar"));
        if let (Some(a), Some(l)) = (appdata_idx, legacy_idx) {
            assert!(a < l, "%APPDATA% must win over the legacy dot directory");
        }
    }

    #[test]
    fn dotted_directory_is_used_when_no_override_is_set() {
        let home = Some(PathBuf::from(r"C:\Users\dev"));
        assert_eq!(
            resolve_root(None, home.clone(), ".codex"),
            Some(PathBuf::from(r"C:\Users\dev\.codex"))
        );
        assert_eq!(
            resolve_root(None, home, ".claude"),
            Some(PathBuf::from(r"C:\Users\dev\.claude"))
        );
    }

    #[test]
    fn override_is_taken_verbatim_as_the_home_itself() {
        // CODEX_HOME points at the home directory, not its parent: auth.json sits inside.
        let resolved = resolve_root(
            Some(r"D:\runtime\codex-home".to_string()),
            Some(PathBuf::from(r"C:\Users\dev")),
            ".codex",
        );
        assert_eq!(resolved, Some(PathBuf::from(r"D:\runtime\codex-home")));
    }

    #[test]
    fn resolved_files_keep_their_expected_names() {
        if let Some(auth) = codex_auth_file() {
            assert!(auth.ends_with("auth.json"));
        }
        if let Some(creds) = claude_credentials_file() {
            assert!(creds.ends_with(".credentials.json"));
        }
    }
}
