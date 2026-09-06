use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use codexbar_core::plugin::{self, HostConfig};
use codexbar_core::HttpClient;
use time::OffsetDateTime;

struct RouteServer {
    origin: String,
}

fn serve(routes: Vec<(&'static str, &'static str)>) -> RouteServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for _ in 0..routes.len() {
            let (mut stream, _) = listener.accept().unwrap();
            let target = request_target(&mut stream);
            let (_, body) = routes
                .iter()
                .find(|(prefix, _)| target.starts_with(prefix))
                .unwrap_or_else(|| panic!("unexpected fixture request: {target}"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    RouteServer { origin }
}

fn request_target(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    first.split_whitespace().nth(1).unwrap().to_string()
}

async fn run_fixture(
    id: &'static str,
    routes: Vec<(&'static str, &'static str)>,
    origins: &[&str],
    secret_key: &str,
    settings: impl FnOnce(&str) -> HashMap<String, String>,
) -> codexbar_core::UsageSnapshot {
    let server = serve(routes);
    let mut source = plugin::bundled_source(id).unwrap().to_string();
    let endpoints_start = source.find("endpoints: [").unwrap();
    let endpoints_end = endpoints_start + source[endpoints_start..].find("],").unwrap() + 2;
    source.replace_range(
        endpoints_start..endpoints_end,
        "endpoints: [{ setting: \"TEST_BASE\", policy: \"https-or-loopback-http\" }],",
    );
    source = source.replacen(
        "settings: [",
        "settings: [{ key: \"TEST_BASE\", title: \"Test base\", type: \"plain\" }, ",
        1,
    );
    for upstream in origins {
        source = source.replace(upstream, &server.origin);
    }
    let mut config = HostConfig::new(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap());
    config.settings = settings(&server.origin);
    config
        .settings
        .insert("TEST_BASE".into(), server.origin.clone());
    config
        .secrets
        .insert(secret_key.into(), "fixture-key".into());
    let client = HttpClient::new().unwrap();
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let outcome = plugin::engine::run(&source, config, client, handle).unwrap();
        plugin::snapshot::map(
            &outcome.snapshot_json,
            OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        )
        .unwrap()
    })
    .await
    .unwrap()
}

fn empty_settings(_: &str) -> HashMap<String, String> {
    HashMap::new()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]

async fn ten_windows_conversions_execute_their_provider_contracts() {
    let aiand = run_fixture(
        "aiand",
        vec![("/logs?", r#"{"data":[{"cost":"1.25","currency":"usd"},{"cost":"2.75","currency":"usd"}],"has_more":false}"#)],
        &["https://api.aiand.com"],
        "AIAND_API_KEY",
        empty_settings,
    )
    .await;
    assert_eq!(aiand.cost.unwrap().used, 4.0);

    let chutes = run_fixture(
        "chutes",
        vec![("/users/me/subscription_usage", r#"{"subscription":{"active":true,"plan_name":"Pro","current_period_end":"2027-01-15T08:00:00Z"},"monthly":{"used":250,"limit":1000,"unit":"credits"},"rolling_window":{"requests":40,"limit":100,"window_minutes":240,"unit":"requests"}}"#)],
        &["https://api.chutes.ai"],
        "CHUTES_API_KEY",
        |origin| HashMap::from([("CHUTES_API_URL".into(), origin.into())]),
    )
    .await;
    assert_eq!(chutes.primary.unwrap().used_percent, 40.0);
    assert_eq!(chutes.secondary.unwrap().used_percent, 25.0);

    let deepinfra = run_fixture(
        "deepinfra",
        vec![
            ("/payment/checklist", r#"{"stripe_balance":-99.75,"recent":3.94,"limit":20,"suspended":false,"suspend_reason":null}"#),
            ("/payment/usage", r#"{"months":[{"period":"2027-01","total_cost":1234}],"initial_month":"2027-01"}"#),
        ],
        &["https://api.deepinfra.com"],
        "DEEPINFRA_API_KEY",
        empty_settings,
    )
    .await;
    assert_eq!(deepinfra.cost.unwrap().used, 3.94);

    let elevenlabs = run_fixture(
        "elevenlabs",
        vec![("/v1/user/subscription", r#"{"tier":"creator","character_count":25000,"character_limit":100000,"voice_slots_used":2,"voice_limit":10,"professional_voice_slots_used":1,"professional_voice_limit":2,"status":"active","next_character_count_reset_unix":1801000000}"#)],
        &["https://api.elevenlabs.io"],
        "ELEVENLABS_API_KEY",
        |origin| HashMap::from([("ELEVENLABS_API_URL".into(), origin.into())]),
    )
    .await;
    assert_eq!(elevenlabs.primary.unwrap().used_percent, 25.0);
    assert_eq!(elevenlabs.extra_rate_windows.len(), 2);

    let fireworks = run_fixture(
        "fireworks",
        vec![("/v1/accounts/acme/billing/summary", r#"{"lineItems":[{"totalCost":{"currencyCode":"USD","units":"12","nanos":500000000}}]}"#)],
        &["https://api.fireworks.ai"],
        "FIREWORKS_API_KEY",
        |_| HashMap::from([("FIREWORKS_ACCOUNT_SLUG".into(), "acme".into())]),
    )
    .await;
    assert_eq!(fireworks.cost.unwrap().used, 12.5);

    let litellm = run_fixture(
        "litellm",
        vec![
            ("/key/info", r#"{"info":{"user_id":"user-1","team_id":"team-1","key_name":"fixture","spend":2,"expires":"2027-01-15T00:00:00Z"}}"#),
            ("/user/info?user_id=user-1", r#"{"user_id":"user-1","user_info":{"user_id":"user-1","user_email":"me@example.com","max_budget":100,"spend":25},"teams":[{"team_id":"team-1","team_alias":"Platform","max_budget":200,"spend":50}]}"#),
        ],
        &[],
        "LITELLM_API_KEY",
        |origin| HashMap::from([("LITELLM_BASE_URL".into(), origin.into())]),
    )
    .await;
    assert_eq!(litellm.primary.unwrap().used_percent, 25.0);
    assert_eq!(litellm.secondary.unwrap().used_percent, 25.0);

    let llmproxy = run_fixture(
        "llmproxy",
        vec![("/v1/quota-stats", r#"{"providers":{"openai":{"credential_count":3,"active_count":2,"exhausted_count":1,"total_requests":120,"tokens":{"input_cached":1000,"input_uncached":2000,"output":3000},"approx_cost":12.5,"quota_groups":[{"remaining_percent":60,"reset_time":"2027-01-15T00:00:00Z"}]}},"summary":{"total_requests":120,"total_tokens":6000,"approx_cost":12.5}}"#)],
        &[],
        "LLM_PROXY_API_KEY",
        |origin| HashMap::from([("LLM_PROXY_BASE_URL".into(), origin.into())]),
    )
    .await;
    assert_eq!(llmproxy.primary.unwrap().used_percent, 40.0);
    assert_eq!(llmproxy.cost.unwrap().used, 12.5);

    let moonshot = run_fixture(
        "moonshot",
        vec![("/v1/users/me/balance", r#"{"code":0,"data":{"available_balance":49.58,"voucher_balance":50,"cash_balance":12.34},"scode":"0x0","status":true}"#)],
        &["https://api.moonshot.ai"],
        "MOONSHOT_API_KEY",
        empty_settings,
    )
    .await;
    assert!(moonshot.identity.plan.unwrap().contains("$49.58"));

    let neuralwatt = run_fixture(
        "neuralwatt",
        vec![("/v1/quota", r#"{"balance":{"credits_remaining_usd":32.6774,"total_credits_usd":52.34,"credits_used_usd":19.6626,"accounting_method":"energy"},"subscription":{"plan":"pro","current_period_start":"2027-01-01T00:00:00Z","current_period_end":"2027-02-01T00:00:00Z","auto_renew":true,"kwh_included":100,"kwh_used":25,"kwh_remaining":75},"key":{"allowance":{"limit_usd":10,"spent_usd":2,"period":"monthly","blocked":false}}}"#)],
        &["https://api.neuralwatt.com"],
        "NEURALWATT_API_KEY",
        |origin| HashMap::from([("NEURALWATT_API_URL".into(), origin.into())]),
    )
    .await;
    assert_eq!(neuralwatt.primary.unwrap().used_percent, 25.0);
    assert_eq!(neuralwatt.cost.unwrap().used, 32.6774);

    let zenmux = run_fixture(
        "zenmux",
        vec![
            ("/api/v1/management/subscription/detail", r#"{"success":true,"data":{"plan":{"tier":"pro","expires_at":"2027-02-01T00:00:00Z"},"account_status":"healthy","quota_5_hour":{"usage_percentage":0.25,"resets_at":"2027-01-15T10:00:00Z","max_flows":100,"used_flows":25,"remaining_flows":75},"quota_7_day":{"usage_percentage":0.5,"resets_at":"2027-01-20T00:00:00Z","max_flows":1000,"used_flows":500,"remaining_flows":500}}}"#),
            ("/api/v1/management/payg/balance", r#"{"success":true,"data":{"currency":"usd","total_credits":8.5}}"#),
        ],
        &["https://zenmux.ai"],
        "ZENMUX_MANAGEMENT_API_KEY",
        empty_settings,
    )
    .await;
    assert_eq!(zenmux.primary.unwrap().used_percent, 25.0);
    assert_eq!(zenmux.secondary.unwrap().used_percent, 50.0);
    assert_eq!(zenmux.cost.unwrap().used, 8.5);
}
