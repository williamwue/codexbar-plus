//! Velopack update integration. The feed URL is compiled into release builds so a child
//! process cannot redirect update checks by changing the runtime environment.

use serde::Serialize;
use velopack::sources::HttpSource;
use velopack::{UpdateCheck, UpdateManager, UpdateOptions};

const UPDATE_URL: Option<&str> = option_env!("CODEXBAR_UPDATE_URL");

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub configured: bool,
    pub current_version: String,
    pub state: &'static str,
    pub available_version: Option<String>,
}

fn manager() -> Result<UpdateManager, String> {
    let url = UPDATE_URL.ok_or_else(|| {
        "this build has no update feed; package with CODEXBAR_UPDATE_URL set".to_string()
    })?;
    let options = UpdateOptions {
        // A retracted bad release can move the stable feed back to the previous version.
        // Velopack uses a full package and clears newer cached packages for downgrades.
        AllowVersionDowngrade: true,
        ..UpdateOptions::default()
    };
    UpdateManager::new(HttpSource::new(url), Some(options), None).map_err(|err| err.to_string())
}

pub fn check() -> Result<UpdateStatus, String> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    if UPDATE_URL.is_none() {
        return Ok(UpdateStatus {
            configured: false,
            current_version,
            state: "notConfigured",
            available_version: None,
        });
    }

    let manager = manager()?;
    let status = match manager.check_for_updates().map_err(|err| err.to_string())? {
        UpdateCheck::RemoteIsEmpty => UpdateStatus {
            configured: true,
            current_version,
            state: "emptyFeed",
            available_version: None,
        },
        UpdateCheck::NoUpdateAvailable => UpdateStatus {
            configured: true,
            current_version,
            state: "upToDate",
            available_version: None,
        },
        UpdateCheck::UpdateAvailable(update) => UpdateStatus {
            configured: true,
            current_version,
            state: if update.IsDowngrade {
                "rollbackAvailable"
            } else {
                "updateAvailable"
            },
            available_version: Some(update.TargetFullRelease.Version.clone()),
        },
    };
    Ok(status)
}

/// Downloads the current feed target and starts the updater in wait-for-exit mode.
/// Returns `true` only after the updater process has been scheduled successfully.
pub fn download_and_schedule() -> Result<bool, String> {
    let manager = manager()?;
    let update = match manager.check_for_updates().map_err(|err| err.to_string())? {
        UpdateCheck::UpdateAvailable(update) => update,
        UpdateCheck::RemoteIsEmpty | UpdateCheck::NoUpdateAvailable => return Ok(false),
    };
    manager
        .download_updates(&update, None)
        .map_err(|err| err.to_string())?;
    manager
        .wait_exit_then_apply_updates(&*update, false, true, std::iter::empty::<&str>())
        .map_err(|err| err.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_development_build_reports_a_stable_state() {
        if UPDATE_URL.is_none() {
            assert_eq!(
                check().unwrap(),
                UpdateStatus {
                    configured: false,
                    current_version: env!("CARGO_PKG_VERSION").to_string(),
                    state: "notConfigured",
                    available_version: None,
                }
            );
        }
    }
}
