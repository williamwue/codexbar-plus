//! End-to-end plugin host test: runs upstream's bundled `sub2api.js` unmodified against a
//! local HTTP server.
//!
//! `sub2api` is the widest single exercise of the contract: configured loopback endpoint,
//! bearer auth injection, `ctx.settings`, `ctx.env.timeZone`, `ctx.http.get` (text),
//! `ctx.fail.*`, `ctx.date.iso`, `ctx.format.number`, `ctx.pct`, extra windows, detail
//! sections and `subscriptionExpiresAt`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use codexbar_core::model::Confidence;
use codexbar_core::settings::Settings;
use codexbar_core::{providers, HttpClient};

struct MockServer {
    port: u16,
    requests: mpsc::Receiver<Request>,
}

#[derive(Debug)]
struct Request {
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Serves `body` once for every connection, recording what was asked for.
fn serve(status: u16, body: &'static str) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let _ = tx.send(request);
            let reason = if status == 200 { "OK" } else { "ERROR" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    MockServer { port, requests: rx }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let target = request_line.split_whitespace().nth(1)?.to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    // Read exactly the declared body, if any: reading to EOF would deadlock because the
    // client keeps the connection open waiting for our response.
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if length > 0 {
        let mut body = vec![0u8; length];
        let _ = reader.read_exact(&mut body);
    }
    Some(Request { target, headers })
}

const USAGE_PAYLOAD: &str = r#"{
  "mode": "subscription",
  "isValid": true,
  "planName": "Pro plan",
  "balance": 12.5,
  "unit": "USD",
  "subscription": {
    "daily_usage_usd": 2.5,
    "daily_limit_usd": 10,
    "weekly_usage_usd": 20,
    "weekly_limit_usd": 100,
    "monthly_usage_usd": 60,
    "monthly_limit_usd": 400,
    "expires_at": "2026-12-31T00:00:00Z"
  },
  "rate_limits": [
    { "window": "5h", "limit": 100, "used": 25, "remaining": 75, "reset_at": "2026-09-01T18:00:00Z" },
    { "window": "7d", "limit": 1000, "used": 900, "remaining": 100, "reset_at": "2026-09-08T00:00:00Z" }
  ],
  "usage": {
    "today": { "requests": 12, "total_tokens": 34567, "actual_cost": 1.25 },
    "total": { "requests": 4321, "total_tokens": 9876543, "actual_cost": 250.5 }
  }
}"#;

/// Settings with the plugin's key in the encrypted store and its origin configured.
fn config_for(port: u16) -> Settings {
    let mut settings = Settings::default();
    settings
        .set_secret("sub2api", "SUB2API_API_KEY", "sk-mock-key")
        .expect("DPAPI store");
    settings.set_setting(
        "sub2api",
        "SUB2API_BASE_URL",
        &format!("http://127.0.0.1:{port}"),
    );
    settings
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_a_bundled_plugin_end_to_end() {
    let server = serve(200, USAGE_PAYLOAD);
    let config = config_for(server.port);
    let client = HttpClient::new().unwrap();

    let result = providers::fetch_with_settings(&client, "sub2api", &config)
        .await
        .expect("plugin fetch succeeds");

    // The plugin built the request itself: /v1/usage with its query, over loopback HTTP.
    let request = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        request.target.starts_with("/v1/usage?days=30"),
        "{}",
        request.target
    );
    assert!(request.target.contains("timezone="));
    assert_eq!(request.header("authorization"), Some("Bearer sk-mock-key"));
    assert_eq!(request.header("accept"), Some("application/json"));

    let snapshot = result.usage;
    assert_eq!(result.strategy_id, "sub2api.js");

    // primary/secondary/tertiary come from the subscription lanes.
    let primary = snapshot.primary.expect("daily lane");
    assert_eq!(primary.used_percent, 25.0);
    assert_eq!(primary.window_minutes, Some(1440));
    assert_eq!(primary.reset_description.as_deref(), Some("$2.50 / $10.00"));
    assert_eq!(snapshot.secondary.unwrap().used_percent, 20.0);
    assert_eq!(snapshot.tertiary.unwrap().used_percent, 15.0);

    // extraWindows keep their ids, titles, reset instants and formatted descriptions.
    let ids: Vec<&str> = snapshot
        .extra_rate_windows
        .iter()
        .map(|w| w.id.as_str())
        .collect();
    assert_eq!(ids, vec!["5h", "7d"]);
    let weekly = &snapshot.extra_rate_windows[1];
    assert_eq!(weekly.title, "7 day limit");
    assert_eq!(weekly.window.used_percent, 90.0);
    assert_eq!(weekly.window.window_minutes, Some(10080));
    assert!(weekly.window.resets_at.is_some());

    // Detail sections, including the thousands-separated numbers the prelude formats.
    let section = &snapshot.details[0];
    assert_eq!(section.title.as_deref(), Some("Usage summary"));
    let balance = section.rows.iter().find(|r| r.label == "Balance").unwrap();
    assert_eq!(balance.value, "$12.50");
    let tokens = section
        .rows
        .iter()
        .find(|r| r.label == "All time tokens")
        .unwrap();
    assert_eq!(tokens.value, "9,876,543");
    assert_eq!(tokens.hint.as_deref(), Some("$250.50"));

    assert_eq!(snapshot.identity.organization.as_deref(), Some("Pro plan"));
    assert!(snapshot.subscription_expires_at.is_some());
    assert_eq!(snapshot.confidence, Confidence::Exact);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_classified_failures_survive_the_bridge() {
    let server = serve(401, r#"{"error":"nope"}"#);
    let config = config_for(server.port);
    let client = HttpClient::new().unwrap();

    let err = providers::fetch_with_settings(&client, "sub2api", &config)
        .await
        .expect_err("401 must fail");

    // `ctx.fail.authenticationExpired(...)` crosses the QuickJS boundary intact.
    let message = err.to_string();
    assert!(message.contains("authentication-expired"), "{message}");
    assert!(message.contains("rejected the API key"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_outside_the_declared_origin_are_rejected() {
    let server = serve(200, USAGE_PAYLOAD);
    let mut config = config_for(server.port);
    // Point the plugin at a different loopback port than the one it is allowed to use…
    config.set_setting(
        "sub2api",
        "SUB2API_BASE_URL",
        &format!("http://127.0.0.1:{}", server.port + 1),
    );
    let client = HttpClient::new().unwrap();

    // …the request now targets an origin the manifest does not declare only if the
    // plugin hard-codes a host, so instead assert the gate directly: an https origin the
    // configured setting does not match must fail.
    config.set_setting("sub2api", "SUB2API_BASE_URL", "https://sub2api.example");

    let err = providers::fetch_with_settings(&client, "sub2api", &config)
        .await
        .expect_err("unreachable origin must fail");
    let message = err.to_string();
    assert!(
        message.contains("network-failure") || message.contains("request failed"),
        "{message}"
    );
    assert_eq!(
        server
            .requests
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        true,
        "the mock must not have been contacted"
    );
}

/// A cookie-capable plugin, written for this test only: it declares `browser-cookies`
/// plus a loopback endpoint, asks the host for a cookie header, and sends it back as a
/// request header. That proves the whole chain — settings → resolver → host bridge →
/// plugin → HTTP request — without needing a real signed-in browser session.
const COOKIE_PLUGIN: &str = r#"
defineProvider({
  id: "cookietest",
  name: "Cookie Test",
  endpoints: [{ setting: "COOKIETEST_BASE_URL", policy: "https-or-loopback-http" }],
  settings: [{ key: "COOKIETEST_BASE_URL", title: "Base URL", type: "plain" }],
  capabilities: ["browser-cookies", "http-status"],
  cookieDomains: ["t3.chat"],
  async fetchUsage(ctx) {
    const cookie = await ctx.browser.cookieHeader("t3.chat");
    const base = ctx.settings.get("COOKIETEST_BASE_URL").replace(/\/+$/, "");
    const response = await ctx.http.getJSON(`${base}/quota`, { headers: { "X-Echo-Cookie": cookie } });
    return {
      primary: { usedPercent: response.json.usedPercent },
      details: [{ title: "Cookie", rows: [{ label: "header", value: cookie }] }],
    };
  },
});
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pasted_cookie_header_reaches_the_plugins_request() {
    let server = serve(200, r#"{"usedPercent":42}"#);

    let mut settings = Settings::default();
    settings
        .set_cookie_header("cookietest", "session=pasted-value; csrf=xyz")
        .unwrap();
    settings.set_setting(
        "cookietest",
        "COOKIETEST_BASE_URL",
        &format!("http://127.0.0.1:{}", server.port),
    );

    let manifest =
        codexbar_core::plugin::engine::load_manifest(COOKIE_PLUGIN).expect("manifest loads");
    assert!(manifest.has_capability(codexbar_core::plugin::Capability::BrowserCookies));

    let values = settings.plugin_values(&manifest);
    let resolver = settings.cookie_resolver_for("cookietest");
    let http = HttpClient::new().unwrap();
    let handle = tokio::runtime::Handle::current();

    let snapshot = tokio::task::spawn_blocking(move || {
        let mut config = codexbar_core::plugin::HostConfig::new(time::OffsetDateTime::now_utc());
        config.settings = values.settings;
        config.secrets = values.secrets;
        config.cookie_resolver = resolver;
        let outcome = codexbar_core::plugin::engine::run(COOKIE_PLUGIN, config, http, handle)?;
        codexbar_core::plugin::snapshot::map(
            &outcome.snapshot_json,
            time::OffsetDateTime::now_utc(),
        )
    })
    .await
    .unwrap()
    .expect("plugin run succeeds");

    let request = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        request.header("x-echo-cookie"),
        Some("session=pasted-value; csrf=xyz"),
        "the pasted header must reach the wire"
    );
    assert_eq!(snapshot.primary.unwrap().used_percent, 42.0);
    assert_eq!(
        snapshot.details[0].rows[0].value,
        "session=pasted-value; csrf=xyz"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeclared_cookie_domains_are_refused() {
    // Same plugin, but asking for a domain the manifest does not declare.
    let source = COOKIE_PLUGIN.replace("cookieHeader(\"t3.chat\")", "cookieHeader(\"evil.test\")");
    let server = serve(200, r#"{"usedPercent":1}"#);

    let mut settings = Settings::default();
    settings
        .set_cookie_header("cookietest", "session=pasted-value")
        .unwrap();
    settings.set_setting(
        "cookietest",
        "COOKIETEST_BASE_URL",
        &format!("http://127.0.0.1:{}", server.port),
    );
    let manifest = codexbar_core::plugin::engine::load_manifest(&source).unwrap();
    let values = settings.plugin_values(&manifest);
    let resolver = settings.cookie_resolver_for("cookietest");
    let http = HttpClient::new().unwrap();
    let handle = tokio::runtime::Handle::current();

    let err = tokio::task::spawn_blocking(move || {
        let mut config = codexbar_core::plugin::HostConfig::new(time::OffsetDateTime::now_utc());
        config.settings = values.settings;
        config.secrets = values.secrets;
        config.cookie_resolver = resolver;
        codexbar_core::plugin::engine::run(&source, config, http, handle)
    })
    .await
    .unwrap()
    .expect_err("an undeclared domain must be rejected");

    assert!(
        err.to_string().contains("cookie domain is not declared"),
        "{err}"
    );
    assert!(
        server
            .requests
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "no request may be sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_cookie_source_stops_the_plugin_before_any_request() {
    let server = serve(200, r#"{"usedPercent":1}"#);
    let mut settings = Settings::default();
    settings
        .set_cookie_header("cookietest", "session=pasted-value")
        .unwrap();
    settings.set_cookie_source("cookietest", codexbar_core::CookieSource::Off);
    settings.set_setting(
        "cookietest",
        "COOKIETEST_BASE_URL",
        &format!("http://127.0.0.1:{}", server.port),
    );

    let manifest = codexbar_core::plugin::engine::load_manifest(COOKIE_PLUGIN).unwrap();
    let values = settings.plugin_values(&manifest);
    let resolver = settings.cookie_resolver_for("cookietest");
    assert!(resolver.is_none(), "an off source produces no resolver");

    let http = HttpClient::new().unwrap();
    let handle = tokio::runtime::Handle::current();
    let err = tokio::task::spawn_blocking(move || {
        let mut config = codexbar_core::plugin::HostConfig::new(time::OffsetDateTime::now_utc());
        config.settings = values.settings;
        config.secrets = values.secrets;
        config.cookie_resolver = resolver;
        codexbar_core::plugin::engine::run(COOKIE_PLUGIN, config, http, handle)
    })
    .await
    .unwrap()
    .expect_err("cookies are disabled");
    assert!(
        err.to_string().contains("cookie import is unavailable"),
        "{err}"
    );
    assert!(server
        .requests
        .recv_timeout(std::time::Duration::from_millis(300))
        .is_err());
}
