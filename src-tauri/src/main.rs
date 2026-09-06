// Tray-only app: no console window, no taskbar button.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod icon;
mod notifications;
mod state;
mod updater;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use codexbar_core::adaptive::{self, ThermalPressure};
use codexbar_core::{format, providers, HttpClient};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutostartExt};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use time::OffsetDateTime;

use crate::icon::IconState;
use crate::notifications::QuotaAlertTracker;
use crate::state::{AppState, ProviderView};

const TRAY_ID: &str = "codexbar";
const GLOBAL_SHORTCUT: &str = "Ctrl+Shift+Space";
const POPOVER_LABEL: &str = "main";
const SETTINGS_LABEL: &str = "settings";
/// Windows tray icons are 16 px at 100 % scaling; render at 2× and let the shell
/// downscale so the meter stays crisp on 150 %/200 % displays.
const TRAY_ICON_SIZE: u32 = 32;
/// How often to check that the webviews are still alive.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);
/// How long a window may take to answer a liveness ping before it counts as dead.
const PING_GRACE: Duration = Duration::from_millis(700);

struct Runtime {
    state: Arc<AppState>,
    http: HttpClient,
    quota_alerts: Mutex<QuotaAlertTracker>,
    /// Set by the Quit paths so window teardown is not mistaken for a crash.
    quitting: Arc<std::sync::atomic::AtomicBool>,
}

#[tauri::command]
fn snapshots(runtime: State<'_, Runtime>) -> Vec<ProviderView> {
    runtime.state.providers()
}

#[tauri::command]
fn selected_provider(runtime: State<'_, Runtime>) -> Option<ProviderView> {
    runtime.state.selected()
}

#[tauri::command]
fn select_provider(runtime: State<'_, Runtime>, app: AppHandle, id: String) -> bool {
    let changed = runtime.state.select(&id);
    if changed {
        update_tray(&app, &runtime.state);
    }
    changed
}

#[tauri::command]
async fn refresh(app: AppHandle) {
    refresh_all(&app).await;
}

#[tauri::command]
fn autostart_enabled(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|err| err.to_string())
}

#[tauri::command]
fn set_autostart_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable()
    } else {
        manager.disable()
    }
    .map_err(|err| err.to_string())
}

#[tauri::command]
async fn check_for_updates() -> Result<updater::UpdateStatus, String> {
    tauri::async_runtime::spawn_blocking(updater::check)
        .await
        .map_err(|err| format!("update task failed: {err}"))?
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<bool, String> {
    let scheduled = tauri::async_runtime::spawn_blocking(updater::download_and_schedule)
        .await
        .map_err(|err| format!("update task failed: {err}"))??;
    if scheduled {
        begin_quit(&app);
    }
    Ok(scheduled)
}

/// The UI answering a liveness ping.
#[tauri::command]
fn ui_ready(runtime: State<'_, Runtime>, label: String) {
    runtime.state.note_pong(&label);
}

#[tauri::command]
fn hide_popover(app: AppHandle) {
    if let Some(window) = app.get_webview_window(POPOVER_LABEL) {
        let _ = window.hide();
    }
}

#[tauri::command]
fn quit(app: AppHandle) {
    tracing::info!("quit requested from the UI");
    begin_quit(&app);
}

/// Marks the app as quitting so window destruction is not treated as a crash.
fn begin_quit(app: &AppHandle) {
    app.state::<Runtime>()
        .quitting
        .store(true, std::sync::atomic::Ordering::SeqCst);
    app.exit(0);
}

/// One provider row for the settings window.
#[derive(Debug, serde::Serialize)]
struct ProviderSetting {
    id: String,
    display_name: String,
    accent: String,
    plugin: bool,
    requires_cookies: bool,
    enabled: bool,
    ready: bool,
    strategies: Vec<String>,
    settings: Vec<ProviderSettingField>,
    /// Cookie domains the plugin declares; empty for providers that need no cookies.
    cookie_domains: Vec<String>,
    /// `off` | `manual` | `auto`.
    cookie_source: String,
    has_cookie_header: bool,
}

/// A single editable field. Secrets are never sent back to the UI, only their presence.
#[derive(Debug, serde::Serialize)]
struct ProviderSettingField {
    key: String,
    title: String,
    secure: bool,
    configured: bool,
    value: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct ProviderSettingsPayload {
    providers: Vec<ProviderSetting>,
    config_path: Option<String>,
    secrets_path: Option<String>,
}

#[tauri::command]
fn provider_settings() -> ProviderSettingsPayload {
    let settings = codexbar_core::Settings::load();
    let active = providers::active_ids(&settings);

    let rows = providers::descriptors()
        .iter()
        .map(|descriptor| ProviderSetting {
            id: descriptor.id.clone(),
            display_name: descriptor.display_name.clone(),
            accent: descriptor.accent.clone(),
            plugin: descriptor.plugin,
            requires_cookies: descriptor.requires_cookies,
            enabled: settings.is_enabled(&descriptor.id),
            ready: active.contains(&descriptor.id.as_str()),
            strategies: descriptor
                .strategies
                .iter()
                .map(|k| k.label().to_string())
                .collect(),
            cookie_domains: if descriptor.requires_cookies {
                codexbar_core::plugin::load_bundled_manifest(&descriptor.id)
                    .map(|manifest| manifest.cookie_domains)
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            cookie_source: format!("{:?}", settings.cookie_source(&descriptor.id)).to_lowercase(),
            has_cookie_header: settings.has_cookie_header(&descriptor.id),
            settings: descriptor
                .settings
                .iter()
                .map(|field| ProviderSettingField {
                    key: field.key.clone(),
                    title: field.title.clone(),
                    secure: field.secure,
                    configured: if field.secure {
                        settings.has_secret(&descriptor.id, &field.key)
                    } else {
                        settings.setting(&descriptor.id, &field.key).is_some()
                    },
                    value: if field.secure {
                        None
                    } else {
                        settings.setting(&descriptor.id, &field.key)
                    },
                })
                .collect(),
        })
        .collect();

    ProviderSettingsPayload {
        providers: rows,
        config_path: codexbar_core::paths::default_config_path().map(|p| p.display().to_string()),
        secrets_path: codexbar_core::SecretStore::default_path().map(|p| p.display().to_string()),
    }
}

#[tauri::command]
fn set_provider_enabled(id: String, enabled: bool) -> Result<(), String> {
    let mut settings = codexbar_core::Settings::load();
    settings.set_enabled(&id, enabled);
    settings.save().map_err(|e| e.to_string())
}

/// Stores a secret through DPAPI; an empty value clears it.
#[tauri::command]
fn set_provider_secret(id: String, key: String, value: String) -> Result<(), String> {
    let mut settings = codexbar_core::Settings::load();
    settings
        .set_secret(&id, &key, &value)
        .map_err(|e| e.to_string())?;
    settings.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_provider_setting(id: String, key: String, value: String) -> Result<(), String> {
    let mut settings = codexbar_core::Settings::load();
    settings.set_setting(&id, &key, &value);
    settings.save().map_err(|e| e.to_string())
}

/// Stores a pasted cookie header through DPAPI; an empty value clears it.
#[tauri::command]
fn set_provider_cookie(id: String, header: String) -> Result<(), String> {
    let mut settings = codexbar_core::Settings::load();
    settings
        .set_cookie_header(&id, &header)
        .map_err(|e| e.to_string())?;
    settings.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_provider_cookie_source(id: String, source: String) -> Result<(), String> {
    let source = match source.as_str() {
        "off" => codexbar_core::CookieSource::Off,
        "manual" => codexbar_core::CookieSource::Manual,
        "auto" => codexbar_core::CookieSource::Auto,
        other => return Err(format!("unknown cookie source `{other}`")),
    };
    let mut settings = codexbar_core::Settings::load();
    settings.set_cookie_source(&id, source);
    settings.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn open_settings(app: AppHandle, provider: Option<String>) {
    show_settings(&app);
    // Deep-link straight to a provider so the popover's cards can jump into their setup.
    if let (Some(provider), Some(window)) = (provider, app.get_webview_window(SETTINGS_LABEL)) {
        let sanitized: String = provider
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .collect();
        if !sanitized.is_empty() {
            let script = format!(
                "location.hash = '#{sanitized}'; window.dispatchEvent(new HashChangeEvent('hashchange'));"
            );
            if let Err(err) = window.eval(&script) {
                tracing::debug!(error = %err, "could not deep-link the settings window");
            }
        }
    }
}

#[tauri::command]
fn hide_settings(app: AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = window.hide();
    }
}

/// Fetches every provider, updates state, refreshes the tray, notifies the UI.
async fn refresh_all(app: &AppHandle) {
    let (state, http) = {
        let runtime = app.state::<Runtime>();
        (runtime.state.clone(), runtime.http.clone())
    };

    if !state.begin_refresh() {
        tracing::debug!("refresh already in flight; skipping");
        return;
    }

    // Config decides which providers run: disabled ones and plugins without a
    // credential are skipped instead of filling the popover with error rows.
    let settings = codexbar_core::Settings::load();
    // Local cost history is provider-independent and incremental; scan once per refresh.
    let costs = scan_costs().await;

    let mut views = Vec::new();
    for id in providers::active_ids(&settings) {
        let Some(descriptor) = providers::descriptor(id) else {
            continue;
        };
        let view =
            ProviderView::pending(&descriptor.id, &descriptor.display_name, &descriptor.accent);
        let at = OffsetDateTime::now_utc();
        let view = match providers::fetch_with_settings(&http, id, &settings).await {
            Ok(result) => view.with_result(result, at),
            Err(err) => {
                tracing::info!(provider = id, error = %err, "provider fetch failed");
                view.with_error(err.to_string(), at)
            }
        };

        // Status pages are a best-effort enrichment, like upstream: a failure never
        // replaces a good reading and never marks the provider down.
        let status = match &descriptor.status_page_url {
            Some(url) => match codexbar_core::status::fetch(&http, url).await {
                Ok(status) => Some(status),
                Err(err) => {
                    tracing::debug!(provider = id, error = %err, "status poll failed");
                    None
                }
            },
            None => None,
        };

        let (today, month) = costs.get(id).copied().unwrap_or((None, None));
        views.push(view.with_status(status).with_cost(today, month));
    }

    state.replace(views);
    state.select_highest_usage();
    state.end_refresh();
    let snapshots = state.providers();
    deliver_quota_alerts(app, &snapshots);
    update_tray(app, &state);
    let _ = app.emit("providers-updated", snapshots);
}

/// True when the taskbar uses the light theme, meaning the tray needs dark glyphs.
///
/// Windows exposes this as `SystemUsesLightTheme` under the Personalize key; the taskbar
/// follows it independently of the app theme (`AppsUseLightTheme`). Missing value means
/// the default dark taskbar.
#[cfg(windows)]
fn taskbar_uses_light_theme() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_value::<u32, _>("SystemUsesLightTheme"))
        .map(|value| value == 1)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn taskbar_uses_light_theme() -> bool {
    false
}

/// Scans local session logs and returns per-provider today / 30-day cost.
///
/// SQLite and file walking are blocking work, so this runs off the async runtime.
async fn scan_costs() -> HashMap<String, (Option<f64>, Option<f64>)> {
    let scanned = tokio::task::spawn_blocking(|| -> Result<_, codexbar_core::cost::CostError> {
        let today = OffsetDateTime::now_utc().date();
        let mut store = codexbar_core::cost::CostStore::open_default()?;
        let report = codexbar_core::cost::scan_all(&mut store, today)?;
        let day = store.summary(1, today)?;
        let month = store.summary(30, today)?;
        Ok((report, day, month))
    })
    .await;

    let mut out: HashMap<String, (Option<f64>, Option<f64>)> = HashMap::new();
    match scanned {
        Ok(Ok((report, day, month))) => {
            if report.files_scanned > 0 {
                tracing::info!(
                    files = report.files_scanned,
                    events = report.events,
                    skipped = report.skipped,
                    "scanned local session logs"
                );
            }
            for provider in ["codex", "claude"] {
                let today: f64 = day
                    .days
                    .iter()
                    .filter(|row| row.provider == provider)
                    .map(|row| row.cost_usd.unwrap_or(0.0))
                    .sum();
                let month_total: f64 = month
                    .days
                    .iter()
                    .filter(|row| row.provider == provider)
                    .map(|row| row.cost_usd.unwrap_or(0.0))
                    .sum();
                if month_total > 0.0 {
                    out.insert(provider.to_string(), (Some(today), Some(month_total)));
                }
            }
        }
        Ok(Err(err)) => tracing::warn!(error = %err, "cost scan failed"),
        Err(err) => tracing::warn!(error = %err, "cost scan task failed"),
    }
    out
}

/// Sends each threshold crossing through the native notification plugin.
///
/// Alert edge detection lives in `QuotaAlertTracker`; refreshes above the threshold do not
/// create repeat Toasts, and delivery errors never fail an otherwise successful refresh.
fn deliver_quota_alerts(app: &AppHandle, providers: &[ProviderView]) {
    let alerts = {
        let runtime = app.state::<Runtime>();
        let evaluated = runtime
            .quota_alerts
            .lock()
            .expect("quota alert lock")
            .evaluate(providers);
        evaluated
    };
    for alert in alerts {
        if let Err(err) = app
            .notification()
            .builder()
            .title(alert.title)
            .body(alert.body)
            .show()
        {
            tracing::warn!(
                provider = alert.provider_id,
                error = %err,
                "could not show quota notification"
            );
        }
    }
}

/// Redraws the tray icon and tooltip from the selected provider.
fn update_tray(app: &AppHandle, state: &AppState) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let selected = state.selected();

    let icon_state = match &selected {
        Some(view) => IconState {
            primary_remaining: view.primary_remaining(),
            secondary_remaining: view.secondary_remaining(),
            stale: view.snapshot.is_none(),
            incident: view.has_incident(),
            light_theme: taskbar_uses_light_theme(),
        },
        None => IconState {
            light_theme: taskbar_uses_light_theme(),
            ..IconState::default()
        },
    };
    let rendered = icon::render(icon_state, TRAY_ICON_SIZE);
    let image = Image::new_owned(rendered.rgba, rendered.width, rendered.height);
    match tray.set_icon(Some(image)) {
        Ok(()) => tracing::info!(
            provider = selected.as_ref().map(|v| v.id.as_str()).unwrap_or("none"),
            primary_remaining = ?icon_state.primary_remaining,
            secondary_remaining = ?icon_state.secondary_remaining,
            stale = icon_state.stale,
            "tray icon updated"
        ),
        Err(err) => tracing::error!(error = %err, "tray icon update failed"),
    }
    let _ = tray.set_tooltip(Some(&tooltip(selected.as_ref())));
}

fn tooltip(view: Option<&ProviderView>) -> String {
    let Some(view) = view else {
        return "CodexBar".to_string();
    };
    if let Some(err) = &view.error {
        return format!(
            "{} — {}",
            view.display_name,
            err.chars().take(80).collect::<String>()
        );
    }
    let Some(snapshot) = &view.snapshot else {
        return format!("{} — loading", view.display_name);
    };
    let mut line = view.display_name.clone();
    if let Some(status) = &view.status {
        if status.indicator.has_issue() {
            line.push_str(&format!(" — {}", status.summary()));
        }
    }
    if let Some(primary) = &snapshot.primary {
        line.push_str(&format!(
            " — {}",
            format::percent_left(primary.used_percent)
        ));
        if let Some(delta) = primary.time_until_reset(OffsetDateTime::now_utc()) {
            line.push_str(&format!(", resets in {}", format::duration_short(delta)));
        }
    }
    line
}

/// Pings a window's UI and reloads it when there is no answer.
///
/// Recovers from a dead WebView2 host: the window shell survives a runtime update but its
/// content is gone, leaving a blank panel that no amount of showing will fix.
fn verify_or_reload(app: &AppHandle, label: &'static str) {
    let Some(window) = app.get_webview_window(label) else {
        // Gone entirely (a previous rebuild failed): build it again.
        recreate_window(app, label);
        return;
    };
    let state = app.state::<Runtime>().state.clone();
    let app = app.clone();
    let asked = std::time::Instant::now();
    let _ = app.emit_to(label, "ping", ());

    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(PING_GRACE).await;
        if state.answered_since(label, asked) {
            return;
        }
        // `destroy` is asynchronous: the label stays taken until the Destroyed event
        // arrives, so the rebuild happens there rather than inline.
        tracing::warn!(
            label,
            "window did not answer; tearing down its dead webview"
        );
        if let Err(err) = window.destroy() {
            tracing::error!(label, error = %err, "could not destroy the dead window");
        }
        let _ = app;
    });
}

/// Presents the settings window.
///
/// The window is declared in `tauri.conf.json` and starts hidden: building a webview
/// on demand from inside a command deadlocks, because window creation needs the main
/// thread that is running the command.
fn show_settings(app: &AppHandle) {
    let Some(window) = app.get_webview_window(SETTINGS_LABEL) else {
        tracing::error!("settings window is missing from the app configuration");
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
    verify_or_reload(app, SETTINGS_LABEL);
}

/// Shows the popover anchored to the tray icon, clamped to the work area.
fn show_popover(app: &AppHandle, tray_rect: Option<(f64, f64)>) {
    let Some(window) = app.get_webview_window(POPOVER_LABEL) else {
        return;
    };

    if let (Some((x, y)), Ok(size)) = (tray_rect, window.outer_size()) {
        let mut left = x - size.width as f64 / 2.0;
        let mut top = y - size.height as f64 - 12.0;

        if let Ok(Some(monitor)) = window.current_monitor() {
            let area = monitor.size();
            let origin = monitor.position();
            let max_x = origin.x as f64 + area.width as f64 - size.width as f64 - 8.0;
            let min_x = origin.x as f64 + 8.0;
            left = left.clamp(min_x.min(max_x), max_x.max(min_x));
            if top < origin.y as f64 + 8.0 {
                // Tray at the top of the screen: drop the popover below it instead.
                top = y + 12.0;
            }
        }

        let _ = window.set_position(PhysicalPosition::new(left as i32, top as i32));
    }

    let _ = window.show();
    let _ = window.set_focus();

    let state = app.state::<Runtime>().state.clone();
    state.note_menu_open(OffsetDateTime::now_utc());
    let _ = app.emit("providers-updated", state.providers());
    verify_or_reload(app, POPOVER_LABEL);
}

fn toggle_popover(app: &AppHandle, tray_rect: Option<(f64, f64)>) {
    let Some(window) = app.get_webview_window(POPOVER_LABEL) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        show_popover(app, tray_rect);
    }
}

/// Watchdog: pings every window once a minute and reloads any that stopped answering.
///
/// A WebView2 runtime update kills the webview and leaves the window shell behind, so the
/// panels go blank while the tray icon keeps updating. Healing in the background means the
/// user never opens a white rectangle — measured cost is one no-op IPC round trip per
/// window per minute.
fn spawn_webview_watchdog(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(WATCHDOG_INTERVAL).await;
            for label in [POPOVER_LABEL, SETTINGS_LABEL] {
                verify_or_reload(&app, label);
            }
        }
    });
}

/// Background poller. Cadence comes from the ported adaptive policy, so an app nobody
/// looks at settles to 30-minute polls instead of hammering provider APIs.
fn spawn_refresh_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // First fetch immediately so the tray shows real numbers on launch.
        refresh_all(&app).await;
        loop {
            let state = app.state::<Runtime>().state.clone();
            let decision = adaptive::next_delay(adaptive::Input {
                now: OffsetDateTime::now_utc(),
                last_menu_open_at: state.last_menu_open_at(),
                last_coding_activity_at: None,
                low_power_mode: false,
                thermal_pressure: ThermalPressure::Nominal,
            });
            tracing::debug!(
                delay_minutes = decision.delay.whole_minutes(),
                reason = decision.reason.as_str(),
                "scheduling next refresh"
            );
            let seconds = decision.delay.whole_seconds().max(1) as u64;
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            refresh_all(&app).await;
        }
    });
}

/// Builds the popover window: frameless, always on top, no taskbar button, hidden.
fn build_popover(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    tauri::WebviewWindowBuilder::new(
        app,
        POPOVER_LABEL,
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("CodexBar")
    .inner_size(400.0, 560.0)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .shadow(true)
    .center()
    .visible(false)
    .build()
}

/// Builds the settings window: a normal window that starts hidden and never steals focus.
fn build_settings(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    tauri::WebviewWindowBuilder::new(
        app,
        SETTINGS_LABEL,
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("CodexBar Settings")
    .inner_size(880.0, 620.0)
    .min_inner_size(720.0, 480.0)
    .resizable(true)
    .skip_taskbar(false)
    .center()
    .visible(false)
    .focused(false)
    .build()
}

/// Destroys and rebuilds a window whose webview died.
///
/// `WebviewWindow::reload` cannot help here: with the WebView2 host process gone it fails
/// with `ERROR_INVALID_STATE` (measured). Only a fresh webview recovers, so the window is
/// torn down and rebuilt from the same builder used at startup.
fn recreate_window(app: &AppHandle, label: &str) {
    if app.get_webview_window(label).is_some() {
        // Still registered: a later Destroyed event will bring us back here.
        tracing::debug!(label, "skipping rebuild; label is still taken");
        return;
    }
    let rebuilt = match label {
        POPOVER_LABEL => build_popover(app),
        SETTINGS_LABEL => build_settings(app),
        _ => return,
    };
    match rebuilt {
        Ok(_) => tracing::info!(label, "rebuilt window after webview loss"),
        Err(err) => tracing::error!(label, error = %err, "could not rebuild the window"),
    }
}

/// Whether an exit request must be refused.
///
/// Tauri asks to exit when the last window disappears, which for a tray app is never the
/// user's intent — a WebView2 runtime update destroys the hidden windows and would
/// otherwise take the whole app with it. `AppHandle::exit` always carries a code, so an
/// explicit Quit is the only thing allowed through.
fn should_prevent_exit(code: Option<i32>) -> bool {
    code.is_none()
}

fn main() {
    // Must run before logging, Tauri, or any other application initialization. During an
    // install/update hook Velopack may terminate this process after handling the hook.
    velopack::VelopackApp::build().run();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CODEXBAR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let http = match HttpClient::new() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("cannot create HTTP client: {err}");
            std::process::exit(1);
        }
    };

    let settings = codexbar_core::Settings::load();
    let initial: Vec<ProviderView> = providers::active_ids(&settings)
        .into_iter()
        .filter_map(providers::descriptor)
        .map(|d| ProviderView::pending(&d.id, &d.display_name, &d.accent))
        .collect();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle_popover(app, None);
                    }
                })
                .build(),
        )
        .manage(Runtime {
            quitting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            state: Arc::new(AppState::new(initial)),
            http,
            quota_alerts: Mutex::new(QuotaAlertTracker::default()),
        })
        .invoke_handler(tauri::generate_handler![
            snapshots,
            selected_provider,
            select_provider,
            refresh,
            hide_popover,
            ui_ready,
            quit,
            provider_settings,
            set_provider_enabled,
            set_provider_secret,
            set_provider_setting,
            set_provider_cookie,
            set_provider_cookie_source,
            open_settings,
            hide_settings,
            autostart_enabled,
            set_autostart_enabled,
            check_for_updates,
            install_update,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Windows are created here rather than declared in tauri.conf.json so that
            // startup and post-crash recreation share one definition.
            if let Err(err) = build_popover(&handle) {
                tracing::error!(error = %err, "could not create the popover window");
            }
            if let Err(err) = build_settings(&handle) {
                tracing::error!(error = %err, "could not create the settings window");
            }

            if let Err(err) = app.global_shortcut().register(GLOBAL_SHORTCUT) {
                tracing::warn!(
                    shortcut = GLOBAL_SHORTCUT,
                    error = %err,
                    "could not register global shortcut"
                );
            }
            let refresh_item =
                MenuItem::with_id(app, "refresh", "Refresh now", true, None::<&str>)?;
            let open_item = MenuItem::with_id(
                app,
                "open",
                "Open CodexBar    Ctrl+Shift+Space",
                true,
                None::<&str>,
            )?;
            let settings_item =
                MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &refresh_item,
                    &settings_item,
                    &separator,
                    &quit_item,
                ],
            )?;

            let rendered = icon::render(IconState::default(), TRAY_ICON_SIZE);
            let image = Image::new_owned(rendered.rgba, rendered.width, rendered.height);

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(image)
                .tooltip("CodexBar — loading")
                .menu(&menu)
                // Left click opens the popover; the menu stays on right click.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show_popover(app, None),
                    "refresh" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move { refresh_all(&app).await });
                    }
                    "settings" => show_settings(app),
                    "quit" => {
                        tracing::info!("quit requested from the tray menu");
                        begin_quit(app);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        let position = rect.position.to_physical::<f64>(1.0);
                        let size = rect.size.to_physical::<f64>(1.0);
                        let anchor = (position.x + size.width / 2.0, position.y);
                        toggle_popover(tray.app_handle(), Some(anchor));
                    }
                })
                .build(app)?;

            spawn_refresh_loop(handle.clone());
            spawn_webview_watchdog(handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing or losing focus hides the popover; the app lives in the tray.
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                WindowEvent::Focused(false) if window.label() == POPOVER_LABEL => {
                    let _ = window.hide();
                }
                // Either the webview died (a WebView2 runtime update does exactly that)
                // or the watchdog tore it down. Rebuild it so the tray never ends up with
                // panels that cannot open.
                WindowEvent::Destroyed => {
                    let app = window.app_handle();
                    if app
                        .state::<Runtime>()
                        .quitting
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    let label = window.label().to_string();
                    tracing::warn!(label = %label, "window destroyed; rebuilding");
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        // Give the runtime a moment to release the label.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        recreate_window(&app, &label);
                    });
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to start CodexBar")
        .run(|_app, event| {
            // A tray utility must outlive its windows. Tauri asks to exit whenever the
            // last window goes away — including when the WebView2 runtime is replaced
            // underneath us, which is how a 3-hour-old instance vanished in testing.
            // Only an explicit `AppHandle::exit` (which carries a code) may end the app.
            if let tauri::RunEvent::ExitRequested { code, api, .. } = &event {
                if should_prevent_exit(*code) {
                    tracing::warn!("ignoring exit request with no code; staying in the tray");
                    api.prevent_exit();
                } else {
                    tracing::info!(code = ?code, "exiting on request");
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_quit_may_end_the_app() {
        assert!(
            should_prevent_exit(None),
            "a window-driven exit request must be refused"
        );
        assert!(
            !should_prevent_exit(Some(0)),
            "the Quit menu item must work"
        );
        assert!(!should_prevent_exit(Some(1)));
    }

    /// Exercises the exact functions the settings window invokes over IPC.
    ///
    /// One test, run in sequence: `Settings::load()` reads `CODEXBAR_CONFIG_PATH`, which is
    /// process-wide, so splitting this across parallel tests would race.
    #[test]
    fn settings_commands_read_and_write_real_files() {
        let dir = std::env::temp_dir().join("codexbar-app-settings-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        // SAFETY: single-threaded test; nothing else in this process reads the variable yet.
        unsafe { std::env::set_var("CODEXBAR_CONFIG_PATH", &config_path) };

        // Read: the payload the UI renders on load.
        let payload = provider_settings();
        assert!(
            payload.providers.len() >= 3,
            "native providers plus plugins"
        );
        assert_eq!(
            payload.config_path.as_deref(),
            Some(config_path.to_str().unwrap())
        );
        let venice = payload
            .providers
            .iter()
            .find(|p| p.id == "venice")
            .expect("venice is bundled");
        assert!(venice.plugin);
        assert!(venice.enabled, "providers default to enabled");
        assert!(!venice.ready, "no credential yet");
        let field = venice
            .settings
            .first()
            .expect("venice declares one setting");
        assert!(field.secure);
        assert!(!field.configured);
        assert_eq!(field.value, None, "secret values are never sent to the UI");

        // Write a secret: lands encrypted, provider becomes ready.
        set_provider_secret(
            "venice".into(),
            "VENICE_API_KEY".into(),
            "sk-from-ui".into(),
        )
        .unwrap();
        let payload = provider_settings();
        let venice = payload.providers.iter().find(|p| p.id == "venice").unwrap();
        assert!(venice.ready, "a stored key makes the provider ready");
        assert!(venice.settings[0].configured);
        let config_text = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !config_text.contains("sk-from-ui"),
            "config.json must stay plaintext-free"
        );

        // Write a plain setting: stored in the config, echoed back to the UI.
        set_provider_setting(
            "sub2api".into(),
            "SUB2API_BASE_URL".into(),
            "https://gw.example".into(),
        )
        .unwrap();
        let payload = provider_settings();
        let sub2api = payload
            .providers
            .iter()
            .find(|p| p.id == "sub2api")
            .unwrap();
        let base = sub2api
            .settings
            .iter()
            .find(|s| s.key == "SUB2API_BASE_URL")
            .unwrap();
        assert_eq!(base.value.as_deref(), Some("https://gw.example"));
        assert!(base.configured);

        // Toggle: disabling drops the provider out of the active set.
        set_provider_enabled("venice".into(), false).unwrap();
        let payload = provider_settings();
        let venice = payload.providers.iter().find(|p| p.id == "venice").unwrap();
        assert!(!venice.enabled);
        assert!(!venice.ready, "a disabled provider is never polled");

        // Clearing the secret removes it again.
        set_provider_secret("venice".into(), "VENICE_API_KEY".into(), String::new()).unwrap();
        let payload = provider_settings();
        let venice = payload.providers.iter().find(|p| p.id == "venice").unwrap();
        assert!(!venice.settings[0].configured);

        unsafe { std::env::remove_var("CODEXBAR_CONFIG_PATH") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
