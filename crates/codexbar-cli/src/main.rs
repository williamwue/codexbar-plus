//! `codexbar` — command line surface, subset-compatible with upstream's CLI
//! (`Sources/CodexBarCLI/CLIEntry.swift:130-340`).
//!
//! Implemented today: `usage`, `cards`, `providers`, `config`, `diagnose`.

use std::io::Read;
use std::process::ExitCode;

use codexbar_core::model::{FetchResult, RateWindow, UsageSnapshot};
use codexbar_core::settings::Settings;
use codexbar_core::{format, providers, HttpClient};
use time::OffsetDateTime;

const HELP: &str = "\
codexbar — AI coding provider limits, in your tray

USAGE:
    codexbar <COMMAND> [OPTIONS]

COMMANDS:
    usage                  Full usage detail per provider
    cards                  One compact line per provider (Provider / Usage / Reset)
    cost                   Local cost history from Codex/Claude session logs
    status                 Provider status pages (incident indicator + components)
    cookie                 Diagnose browser cookie import for a domain
    providers              List known providers, their strategy and credential state
    config <SUBCOMMAND>    Inspect and edit the config file
    diagnose               Show resolved local paths and credential availability

CONFIG SUBCOMMANDS:
    config providers                       List providers with enabled state
    config enable   --provider <id>        Enable a provider
    config disable  --provider <id>        Disable a provider
    config set-api-key --provider <id> [--key <k>] [--api-key <key> | --stdin]
    config set-secret  --provider <id> --key <k> [--api-key <key> | --stdin]
    config clear-secret --provider <id> [--key <k>]
    config set --provider <id> --key <k> --value <v>   Set a plain plugin setting
    config set-cookie  --provider <id> [--api-key <hdr> | --stdin]
    config cookie-source --provider <id> --value off|manual|auto
    config migrate-secrets                 Move plaintext keys into the encrypted store
    config path                            Print config and secret-store locations

OPTIONS:
    --provider <id>   Restrict to one provider (default: every active provider)
    --days <n>        Window for `cost` (default 30)
    --group-by <k>    `cost` grouping: day (default) | model | provider
    --domain <d>      Domain for `cookie` (default: every plugin's declared domains)
    --all             Include providers without credentials
    --json            Machine-readable output
    -h, --help        Show this help
";

#[derive(Debug, PartialEq, Default)]
struct Args {
    command: String,
    subcommand: Option<String>,
    provider: Option<String>,
    key: Option<String>,
    value: Option<String>,
    api_key: Option<String>,
    days: Option<u16>,
    domain: Option<String>,
    group_by: Option<String>,
    stdin: bool,
    all: bool,
    json: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        // Options that take a value consume the next argv entry.
        let value_for = |flag: &str, i: &mut usize| -> Result<String, String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "-h" | "--help" => return Err(HELP.to_string()),
            "--json" => args.json = true,
            "--all" => args.all = true,
            "--stdin" => args.stdin = true,
            "--provider" => args.provider = Some(value_for("--provider", &mut i)?),
            "--key" => args.key = Some(value_for("--key", &mut i)?),
            "--value" => args.value = Some(value_for("--value", &mut i)?),
            "--api-key" => args.api_key = Some(value_for("--api-key", &mut i)?),
            "--days" => {
                let raw = value_for("--days", &mut i)?;
                args.days = Some(
                    raw.parse()
                        .map_err(|_| format!("--days must be a number, got `{raw}`"))?,
                );
            }
            "--domain" => args.domain = Some(value_for("--domain", &mut i)?),
            "--group-by" => {
                let raw = value_for("--group-by", &mut i)?;
                if !["day", "model", "provider"].contains(&raw.as_str()) {
                    return Err(format!(
                        "--group-by must be day, model or provider, got `{raw}`"
                    ));
                }
                args.group_by = Some(raw);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`\n\n{HELP}"))
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    if positional.len() > 2 {
        return Err(format!("unexpected argument `{}`\n\n{HELP}", positional[2]));
    }
    args.command = positional
        .first()
        .cloned()
        .unwrap_or_else(|| "usage".to_string());
    args.subcommand = positional.get(1).cloned();
    Ok(args)
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CODEXBAR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(message) => {
            println!("{message}");
            return ExitCode::from(if message == HELP { 0 } else { 2 });
        }
    };

    match run(&args).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &Args) -> anyhow::Result<ExitCode> {
    match args.command.as_str() {
        "providers" => {
            print_providers(args.json);
            Ok(ExitCode::SUCCESS)
        }
        "config" => run_config(args),
        "diagnose" => {
            print_diagnose(args.json);
            Ok(ExitCode::SUCCESS)
        }
        "usage" | "cards" => run_usage(args).await,
        "cost" => run_cost(args),
        "status" => run_status(args).await,
        "cookie" => run_cookie(args),
        other => {
            println!("unknown command `{other}`\n\n{HELP}");
            Ok(ExitCode::from(2))
        }
    }
}

/// Providers to query: one explicit id, everything configured, or literally everything.
fn targets(args: &Args, settings: &Settings) -> anyhow::Result<Vec<&'static str>> {
    if let Some(id) = &args.provider {
        let descriptor = providers::descriptor(id).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider `{id}` (known: {})",
                providers::ids().join(", ")
            )
        })?;
        return Ok(vec![descriptor.id.as_str()]);
    }
    Ok(if args.all {
        providers::ids()
    } else {
        providers::active_ids(settings)
    })
}

async fn run_usage(args: &Args) -> anyhow::Result<ExitCode> {
    let settings = Settings::load();
    let targets = targets(args, &settings)?;
    let client = HttpClient::new()?;

    let mut results = Vec::new();
    let mut failures = Vec::new();
    for id in targets {
        match providers::fetch_with_settings(&client, id, &settings).await {
            Ok(result) => results.push((id, result)),
            Err(err) => failures.push((id, format!("{err}"))),
        }
    }

    if args.json {
        let payload = serde_json::json!({
            "providers": results
                .iter()
                .map(|(id, r)| serde_json::json!({ "provider": id, "result": r }))
                .collect::<Vec<_>>(),
            "failures": failures
                .iter()
                .map(|(id, e)| serde_json::json!({ "provider": id, "error": e }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if args.command == "cards" {
        print_cards(&results, &failures);
    } else {
        print_usage(&results, &failures);
    }

    Ok(if results.is_empty() && !failures.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn run_config(args: &Args) -> anyhow::Result<ExitCode> {
    let mut settings = Settings::load();
    let subcommand = args.subcommand.as_deref().unwrap_or("providers");

    match subcommand {
        "path" => {
            match codexbar_core::paths::default_config_path() {
                Some(path) => println!("config  {}", path.display()),
                None => println!("config  unresolved"),
            }
            match codexbar_core::SecretStore::default_path() {
                Some(path) => println!("secrets {}", path.display()),
                None => println!("secrets unresolved"),
            }
            Ok(ExitCode::SUCCESS)
        }
        "providers" => {
            let active = providers::active_ids(&settings);
            if args.json {
                let payload: Vec<_> = providers::descriptors()
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d.id,
                            "name": d.display_name,
                            "plugin": d.plugin,
                            "enabled": settings.is_enabled(&d.id),
                            "ready": active.contains(&d.id.as_str()),
                            "requiresCookies": d.requires_cookies,
                            "settings": d.settings.iter().map(|s| serde_json::json!({
                                "key": s.key,
                                "title": s.title,
                                "secure": s.secure,
                                "configured": if s.secure {
                                    settings.has_secret(&d.id, &s.key)
                                } else {
                                    settings.setting(&d.id, &s.key).is_some()
                                },
                            })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!(
                    "{:<12} {:<8} {:<9} {:<8} {}",
                    "ID", "KIND", "ENABLED", "READY", "NAME"
                );
                for descriptor in providers::descriptors() {
                    println!(
                        "{:<12} {:<8} {:<9} {:<8} {}",
                        descriptor.id,
                        if descriptor.plugin {
                            "plugin"
                        } else {
                            "native"
                        },
                        settings.is_enabled(&descriptor.id),
                        active.contains(&descriptor.id.as_str()),
                        descriptor.display_name
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        "enable" | "disable" => {
            let id = require_provider(args)?;
            settings.set_enabled(&id, subcommand == "enable");
            settings.save()?;
            println!("{id} {subcommand}d");
            Ok(ExitCode::SUCCESS)
        }
        "set-api-key" | "set-secret" => {
            let id = require_provider(args)?;
            let key = secret_key_for(&id, args)?;
            let value = read_secret(args)?;
            settings.set_secret(&id, &key, &value)?;
            settings.save()?;
            println!("stored {id}.{key} (DPAPI-encrypted)");
            Ok(ExitCode::SUCCESS)
        }
        "set-cookie" => {
            let id = require_provider(args)?;
            let header = read_secret(args)?;
            settings.set_cookie_header(&id, &header)?;
            settings.save()?;
            println!("stored cookie header for {id} (DPAPI-encrypted)");
            Ok(ExitCode::SUCCESS)
        }
        "cookie-source" => {
            let id = require_provider(args)?;
            let raw = args.value.clone().ok_or_else(|| {
                anyhow::anyhow!("config cookie-source needs --value off|manual|auto")
            })?;
            let source = match raw.trim().to_lowercase().as_str() {
                "off" => codexbar_core::CookieSource::Off,
                "manual" => codexbar_core::CookieSource::Manual,
                "auto" => codexbar_core::CookieSource::Auto,
                other => anyhow::bail!("cookie source must be off, manual or auto, got `{other}`"),
            };
            settings.set_cookie_source(&id, source);
            settings.save()?;
            println!("{id} cookie source set to {raw}");
            Ok(ExitCode::SUCCESS)
        }
        "clear-secret" => {
            let id = require_provider(args)?;
            let key = secret_key_for(&id, args)?;
            settings.set_secret(&id, &key, "")?;
            settings.save()?;
            println!("cleared {id}.{key}");
            Ok(ExitCode::SUCCESS)
        }
        "set" => {
            let id = require_provider(args)?;
            let key = args
                .key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("config set needs --key"))?;
            let value = args
                .value
                .clone()
                .ok_or_else(|| anyhow::anyhow!("config set needs --value"))?;
            settings.set_setting(&id, &key, &value);
            settings.save()?;
            println!("set {id}.{key}");
            Ok(ExitCode::SUCCESS)
        }
        "migrate-secrets" => {
            let manifests: Vec<_> = providers::descriptors()
                .iter()
                .filter(|d| d.plugin)
                .filter_map(|d| codexbar_core::plugin::load_bundled_manifest(&d.id).ok())
                .collect();
            let moved = settings.migrate_plaintext_secrets(&manifests);
            settings.save()?;
            println!("migrated {moved} plaintext secret(s) into the encrypted store");
            Ok(ExitCode::SUCCESS)
        }
        other => {
            println!("unknown config subcommand `{other}`\n\n{HELP}");
            Ok(ExitCode::from(2))
        }
    }
}

/// Resolves which setting key a secret belongs to: explicit `--key`, or the plugin's only
/// secure setting when it has exactly one.
fn secret_key_for(id: &str, args: &Args) -> anyhow::Result<String> {
    if let Some(key) = &args.key {
        return Ok(key.clone());
    }
    let descriptor =
        providers::descriptor(id).ok_or_else(|| anyhow::anyhow!("unknown provider `{id}`"))?;
    let secure: Vec<&str> = descriptor
        .settings
        .iter()
        .filter(|s| s.secure)
        .map(|s| s.key.as_str())
        .collect();
    match secure.as_slice() {
        [only] => Ok((*only).to_string()),
        [] => anyhow::bail!("provider `{id}` declares no secure setting"),
        many => anyhow::bail!("provider `{id}` needs --key (one of: {})", many.join(", ")),
    }
}

fn require_provider(args: &Args) -> anyhow::Result<String> {
    let id = args
        .provider
        .clone()
        .ok_or_else(|| anyhow::anyhow!("this command needs --provider <id>"))?;
    if providers::descriptor(&id).is_none() {
        anyhow::bail!(
            "unknown provider `{id}` (known: {})",
            providers::ids().join(", ")
        );
    }
    Ok(id)
}

/// Reads a secret from `--api-key` or stdin; stdin keeps it out of shell history.
fn read_secret(args: &Args) -> anyhow::Result<String> {
    if let Some(key) = &args.api_key {
        return Ok(key.trim().to_string());
    }
    if !args.stdin {
        anyhow::bail!("set-api-key needs --api-key <key> or --stdin");
    }
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    let trimmed = buffer.trim().to_string();
    if trimmed.is_empty() {
        anyhow::bail!("no key on stdin");
    }
    Ok(trimmed)
}

/// Diagnoses cookie import. Never prints cookie values — only names and counts.
fn run_cookie(args: &Args) -> anyhow::Result<ExitCode> {
    use codexbar_core::cookies;

    let settings = Settings::load();
    let domains: Vec<String> = match &args.domain {
        Some(domain) => vec![domain.trim().to_lowercase()],
        None => {
            let mut all: Vec<String> = providers::descriptors()
                .iter()
                .filter(|d| d.requires_cookies)
                .filter_map(|d| codexbar_core::plugin::load_bundled_manifest(&d.id).ok())
                .flat_map(|manifest| manifest.cookie_domains)
                .collect();
            all.sort();
            all.dedup();
            all
        }
    };

    let firefox = cookies::firefox::profiles();
    let chromium = cookies::chromium::installed();
    let (found, reports) = if domains.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        cookies::import(&domains)
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "domains": domains,
                "firefox_profiles": firefox.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "chromium_browsers": chromium.iter().map(|b| b.id).collect::<Vec<_>>(),
                "imports": reports,
                "cookies_found": found.len(),
                "cookie_names": found.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            }))?
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "domains: {}",
        if domains.is_empty() {
            "(none)".into()
        } else {
            domains.join(", ")
        }
    );
    println!(
        "firefox profiles: {} · chromium browsers: {}",
        firefox.len(),
        chromium
            .iter()
            .map(|b| b.display_name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    for report in &reports {
        match &report.error {
            Some(error) => println!(
                "  {:<8} {} — {error}",
                report.browser,
                short_path(&report.database)
            ),
            None => println!(
                "  {:<8} {} — {} cookie(s)",
                report.browser,
                short_path(&report.database),
                report.cookies
            ),
        }
    }
    if found.is_empty() {
        println!("no cookies imported");
    } else {
        // Names only: values are credentials.
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).take(12).collect();
        println!("imported {} cookie(s): {}", found.len(), names.join(", "));
    }

    println!();
    println!(
        "{:<12} {:<8} {:<8} {}",
        "PROVIDER", "SOURCE", "MANUAL", "DECLARED DOMAINS"
    );
    for descriptor in providers::descriptors()
        .iter()
        .filter(|d| d.requires_cookies)
    {
        let manifest = codexbar_core::plugin::load_bundled_manifest(&descriptor.id).ok();
        println!(
            "{:<12} {:<8} {:<8} {}",
            descriptor.id,
            format!("{:?}", settings.cookie_source(&descriptor.id)).to_lowercase(),
            settings.has_cookie_header(&descriptor.id),
            manifest
                .map(|m| m.cookie_domains.join(", "))
                .unwrap_or_default()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['\\', '/']).collect();
    if parts.len() <= 3 {
        return path.to_string();
    }
    format!("…{}", parts[parts.len() - 3..].join("\\"))
}

/// Scans the local session logs, then prints a rolling window.
fn run_cost(args: &Args) -> anyhow::Result<ExitCode> {
    let today = OffsetDateTime::now_utc().date();
    let days = args.days.unwrap_or(30);
    let mut store = codexbar_core::cost::CostStore::open_default()?;
    let report = codexbar_core::cost::scan_all(&mut store, today)?;
    let summary = store.summary(days, today)?;

    let rows = group_rows(&summary.days, args.group_by.as_deref().unwrap_or("day"));

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "database": store.path().display().to_string(),
                "scan": report,
                "window_days": days,
                "cost_usd": summary.cost_usd,
                "partial": summary.partial,
                "total_tokens": summary.total_tokens,
                "requests": summary.requests,
                "rows": rows,
            }))?
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "scanned {} of {} session files ({} new events, {} unchanged)",
        report.files_scanned, report.files_seen, report.events, report.skipped
    );
    println!(
        "last {days}d: {} · {} tokens · {} requests{}",
        format::currency(summary.cost_usd, Some("USD")),
        thousands(summary.total_tokens),
        thousands(summary.requests),
        if summary.partial {
            " (some models unpriced)"
        } else {
            ""
        }
    );
    if rows.is_empty() {
        println!("no local session data in this window");
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    println!(
        "{:<22} {:>14} {:>12} {:>10}",
        "GROUP", "TOKENS", "COST", "REQUESTS"
    );
    for row in &rows {
        println!(
            "{:<22} {:>14} {:>12} {:>10}",
            row.key.chars().take(22).collect::<String>(),
            thousands(row.tokens),
            format::currency(row.cost_usd, Some("USD")),
            thousands(row.requests)
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, serde::Serialize)]
struct CostRow {
    key: String,
    tokens: u64,
    cost_usd: f64,
    requests: u64,
}

/// Collapses day/provider/model rows onto one axis.
fn group_rows(days: &[codexbar_core::cost::DayTotals], group_by: &str) -> Vec<CostRow> {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, CostRow> = BTreeMap::new();
    for row in days {
        let key = match group_by {
            "model" => row.model.clone(),
            "provider" => row.provider.clone(),
            _ => row.day.clone(),
        };
        let entry = grouped.entry(key.clone()).or_insert(CostRow {
            key,
            tokens: 0,
            cost_usd: 0.0,
            requests: 0,
        });
        entry.tokens += row.total_tokens();
        entry.cost_usd += row.cost_usd.unwrap_or(0.0);
        entry.requests += row.requests;
    }
    let mut rows: Vec<CostRow> = grouped.into_values().collect();
    // Days read best newest-first; the other groupings read best by spend.
    if group_by == "day" {
        rows.sort_by(|a, b| b.key.cmp(&a.key));
    } else {
        rows.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
    }
    rows
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Polls each provider's status page.
async fn run_status(args: &Args) -> anyhow::Result<ExitCode> {
    let client = HttpClient::new()?;
    let targets: Vec<&codexbar_core::ProviderDescriptor> = providers::descriptors()
        .iter()
        .filter(|d| d.status_page_url.is_some())
        .filter(|d| {
            args.provider
                .as_deref()
                .map(|id| id == d.id)
                .unwrap_or(true)
        })
        .collect();

    if targets.is_empty() {
        println!("no provider with a status page matched");
        return Ok(ExitCode::from(2));
    }

    let mut payload = Vec::new();
    for descriptor in targets {
        let url = descriptor.status_page_url.as_deref().unwrap_or_default();
        match codexbar_core::status::fetch(&client, url).await {
            Ok(status) => {
                if args.json {
                    payload
                        .push(serde_json::json!({ "provider": descriptor.id, "status": status }));
                } else {
                    println!(
                        "{:<12} {:<12} {}",
                        descriptor.id,
                        format!("{:?}", status.indicator).to_lowercase(),
                        status.summary()
                    );
                    for component in status.issues() {
                        println!("  - {} {}", component.name, component.indicator.label());
                    }
                }
            }
            Err(err) => {
                if args.json {
                    payload.push(
                        serde_json::json!({ "provider": descriptor.id, "error": err.to_string() }),
                    );
                } else {
                    println!("{:<12} {:<12} {err}", descriptor.id, "error");
                }
            }
        }
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_providers(json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(providers::descriptors()).unwrap_or_default()
        );
        return;
    }
    for d in providers::descriptors() {
        let strategies: Vec<&str> = d.strategies.iter().map(|k| k.label()).collect();
        println!(
            "{:<12} {:<8} {:<10} {}",
            d.id,
            if d.plugin { "plugin" } else { "native" },
            strategies.join(","),
            d.display_name
        );
    }
}

fn print_diagnose(json: bool) {
    let entries = vec![
        ("config", codexbar_core::paths::default_config_path()),
        ("secrets", codexbar_core::SecretStore::default_path()),
        ("cache", codexbar_core::paths::cache_dir()),
        ("cost.db", codexbar_core::cost::database_path()),
        ("codex.auth", codexbar_core::paths::codex_auth_file()),
        (
            "claude.credentials",
            codexbar_core::paths::claude_credentials_file(),
        ),
    ];

    if json {
        let payload: Vec<_> = entries
            .iter()
            .map(|(name, path)| {
                serde_json::json!({
                    "name": name,
                    "path": path.as_ref().map(|p| p.display().to_string()),
                    "exists": path.as_ref().map(|p| p.exists()).unwrap_or(false),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return;
    }

    for (name, path) in entries {
        match path {
            Some(p) => println!(
                "{:<20} {:<7} {}",
                name,
                if p.exists() { "found" } else { "missing" },
                p.display()
            ),
            None => println!("{name:<20} unresolved"),
        }
    }
}

fn window_line(label: &str, window: &RateWindow, now: OffsetDateTime) -> String {
    let reset = match window.time_until_reset(now) {
        Some(delta) => format!("resets in {}", format::duration_short(delta)),
        None => window
            .reset_description
            .clone()
            .unwrap_or_else(|| "reset time unknown".to_string()),
    };
    format!(
        "  {label:<10} {:<12} {reset}",
        format::percent_left(window.used_percent)
    )
}

fn print_usage(results: &[(&str, FetchResult)], failures: &[(&str, String)]) {
    let now = OffsetDateTime::now_utc();
    for (id, result) in results {
        let snapshot: &UsageSnapshot = &result.usage;
        println!("== {id} ({}) ==", result.strategy_kind.label());
        if let Some(w) = &snapshot.primary {
            println!("{}", window_line("Session", w, now));
        }
        if let Some(w) = &snapshot.secondary {
            println!("{}", window_line("Weekly", w, now));
        }
        if let Some(w) = &snapshot.tertiary {
            println!("{}", window_line("Model", w, now));
        }
        for extra in &snapshot.extra_rate_windows {
            println!("{}", window_line(&extra.title, &extra.window, now));
        }
        if let Some(credits) = &snapshot.credits {
            println!(
                "  {:<10} {}",
                "Credits",
                format::currency(credits.remaining, credits.currency.as_deref())
            );
        }
        if let Some(cost) = &snapshot.cost {
            let limit = cost
                .limit
                .map(|l| format!(" / {}", format::currency(l, Some(&cost.currency))))
                .unwrap_or_default();
            println!(
                "  {:<10} {}{limit}{}",
                cost.period.as_deref().unwrap_or("Cost"),
                format::currency(cost.used, Some(&cost.currency)),
                cost.balance
                    .map(|b| format!(" (balance {})", format::currency(b, Some(&cost.currency))))
                    .unwrap_or_default()
            );
        }
        if let Some(history) = &snapshot.cost_usage {
            println!(
                "  {:<10} {} over {}d",
                "History",
                format::currency(history.total_cost(), Some(&history.currency)),
                history.history_days
            );
        }
        for section in &snapshot.details {
            if let Some(title) = &section.title {
                println!("  -- {title}");
            }
            for detail in &section.rows {
                let hint = detail
                    .hint
                    .as_deref()
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default();
                println!("  {:<10} {}{hint}", detail.label, detail.value);
            }
        }
        if let Some(account) = &snapshot.identity.account {
            println!("  {:<10} {account}", "Account");
        }
        if let Some(plan) = &snapshot.identity.plan {
            println!("  {:<10} {plan}", "Plan");
        }
        if let Some(org) = &snapshot.identity.organization {
            println!("  {:<10} {org}", "Org");
        }
        println!();
    }
    for (id, err) in failures {
        println!("== {id} ==\n  error: {err}\n");
    }
}

fn print_cards(results: &[(&str, FetchResult)], failures: &[(&str, String)]) {
    let now = OffsetDateTime::now_utc();
    println!("{:<12} {:<12} {}", "PROVIDER", "USAGE", "RESET");
    for (id, result) in results {
        let (usage, reset) = match &result.usage.primary {
            Some(w) => (
                format::percent_left(w.used_percent),
                w.time_until_reset(now)
                    .map(format::duration_short)
                    .unwrap_or_else(|| "-".to_string()),
            ),
            None => ("no data".to_string(), "-".to_string()),
        };
        println!("{id:<12} {usage:<12} {reset}");
    }
    for (id, err) in failures {
        println!(
            "{id:<12} {:<12} {}",
            "error",
            err.chars().take(60).collect::<String>()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_usage_across_active_providers() {
        let args = parse_args(&[]).unwrap();
        assert_eq!(args.command, "usage");
        assert_eq!(args.provider, None);
        assert!(!args.json && !args.all);
    }

    #[test]
    fn parses_command_with_options_in_any_order() {
        let argv = ["--json", "cards", "--provider", "codex"].map(String::from);
        let args = parse_args(&argv).unwrap();
        assert_eq!(args.command, "cards");
        assert_eq!(args.provider.as_deref(), Some("codex"));
        assert!(args.json);
    }

    #[test]
    fn parses_config_subcommands_and_values() {
        let argv = [
            "config",
            "set",
            "--provider",
            "zai",
            "--key",
            "Z_AI_REGION",
            "--value",
            "cn",
        ]
        .map(String::from);
        let args = parse_args(&argv).unwrap();
        assert_eq!(args.command, "config");
        assert_eq!(args.subcommand.as_deref(), Some("set"));
        assert_eq!(args.key.as_deref(), Some("Z_AI_REGION"));
        assert_eq!(args.value.as_deref(), Some("cn"));
    }

    #[test]
    fn rejects_unknown_options_and_missing_values() {
        assert!(parse_args(&["--nope".to_string()]).is_err());
        assert!(parse_args(&["--provider".to_string()]).is_err());
        assert!(parse_args(&["usage".into(), "a".into(), "b".into()]).is_err());
    }

    #[test]
    fn secret_input_requires_an_explicit_source() {
        let args = Args::default();
        assert!(read_secret(&args).is_err());

        let with_key = Args {
            api_key: Some("  sk-1  ".into()),
            ..Args::default()
        };
        assert_eq!(read_secret(&with_key).unwrap(), "sk-1");
    }

    #[test]
    fn help_is_returned_as_the_error_payload() {
        let err = parse_args(&["--help".to_string()]).unwrap_err();
        assert!(err.contains("USAGE:"));
    }
}
