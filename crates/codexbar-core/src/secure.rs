//! Windows equivalent of upstream's `chmod 0600` on secret-bearing files
//! (`Config/CodexBarConfigStore.swift:126-130`).
//!
//! POSIX modes are meaningless on NTFS, so the equivalent is an explicit DACL:
//! drop inherited ACEs and grant full control to the current user only. `icacls` is the
//! documented, dependency-free way to do that and reports failures on stderr.

use std::path::Path;
use std::process::Command;

/// Restricts `path` to the current user. Returns true when the ACL was rewritten.
///
/// Best-effort by design: a failure must not lose the user's freshly refreshed token,
/// so callers log and continue (the file is still under the user profile).
pub fn restrict_to_current_user(path: &Path) -> bool {
    let Some(principal) = current_principal() else {
        tracing::warn!(
            "cannot resolve current user; leaving inherited ACL on {}",
            path.display()
        );
        return false;
    };

    let output = Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{principal}:(F)"))
        .output();

    match output {
        Ok(out) if out.status.success() => {
            tracing::debug!(path = %path.display(), %principal, "restricted file ACL");
            true
        }
        Ok(out) => {
            tracing::warn!(
                path = %path.display(),
                status = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "icacls refused to restrict file"
            );
            false
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "icacls unavailable");
            false
        }
    }
}

/// `DOMAIN\user` when the machine is joined, otherwise the bare account name.
fn current_principal() -> Option<String> {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.trim().is_empty() => Some(format!("{domain}\\{user}")),
        _ => Some(user),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_is_domain_qualified_when_available() {
        let principal = current_principal().expect("USERNAME must exist on Windows");
        assert!(!principal.is_empty());
        if std::env::var("USERDOMAIN").is_ok() {
            assert!(
                principal.contains('\\'),
                "expected DOMAIN\\user, got {principal}"
            );
        }
    }

    #[test]
    fn restricting_a_real_file_succeeds_and_keeps_it_readable() {
        let dir = std::env::temp_dir().join("codexbar-acl-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.json");
        std::fs::write(&path, b"{}").unwrap();

        assert!(
            restrict_to_current_user(&path),
            "icacls should succeed for an owned file"
        );
        // The owner must still be able to read it after the ACL rewrite.
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");

        let _ = std::fs::remove_file(&path);
    }
}
