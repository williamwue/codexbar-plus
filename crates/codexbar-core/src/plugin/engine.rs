//! QuickJS plugin engine.
//!
//! Runs upstream's bundled provider plugins unmodified. The JS side is upstream's own
//! `provider-plugin-prelude.js`; this module supplies the native `host` bridge it expects
//! (`host.http`, `settingGet`, `cookieHeader`, `cacheGet/Set`, `log`, `nextDailyReset`,
//! `pct`, `amountFromPercent`) plus the `defineProvider` global, mirroring
//! `Plugins/QuickJSProviderPluginEngine.swift:297-455,497-767`.
//!
//! Execution is synchronous by design: `host.http` performs the request and resolves the
//! JS promise before returning, then the runtime's job queue is pumped until `fetchUsage`
//! settles. Callers run it on a blocking thread and hand in a Tokio handle for the actual
//! I/O.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use reqwest::Method;
use rquickjs::function::Func;
use rquickjs::{CatchResultExt, Context, Ctx, Function, Object, Runtime, Value};
use time::OffsetDateTime;
use url::Url;

use super::manifest::{Capability, Endpoint, EndpointPolicy, Manifest, SettingKind};
use super::{PluginError, PluginFailure, PluginOutcome};
use crate::http::HttpClient;

/// Upstream: 64 MiB per plugin runtime (`QuickJSRuntimeLimits`).
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// Upstream: bundled plugins may read 5 MiB responses.
const RESPONSE_LIMIT: usize = 5 * 1024 * 1024;
/// Upstream: `ProviderPluginRuntime.defaultTimeout`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Upstream: per-request default and bounds.
const DEFAULT_REQUEST_TIMEOUT: f64 = 15.0;
const MIN_REQUEST_TIMEOUT: f64 = 1.0;
const MAX_REQUEST_TIMEOUT: f64 = 30.0;

const PRELUDE: &str = include_str!("../../resources/plugins/provider-plugin-prelude.js");

/// Everything the host needs to answer a plugin's questions.
pub struct HostConfig {
    /// Plain settings, keyed by the plugin's declared setting keys.
    pub settings: HashMap<String, String>,
    /// Secure settings (API keys, tokens).
    pub secrets: HashMap<String, String>,
    /// Clock the plugin sees; makes fetches reproducible in tests.
    pub now: OffsetDateTime,
    /// IANA zone exposed as `ctx.env.timeZone`.
    pub time_zone: String,
    /// Resolves `ctx.browser.cookieHeader(domain)`.
    ///
    /// Injected so the engine stays free of config/secret-store knowledge, and so tests
    /// can supply a header without touching a browser.
    pub cookie_resolver: Option<CookieResolver>,
}

/// Returns a `Cookie:` header for one declared domain, or an error to reject with.
pub type CookieResolver = Box<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

impl HostConfig {
    pub fn new(now: OffsetDateTime) -> Self {
        Self {
            settings: HashMap::new(),
            secrets: HashMap::new(),
            now,
            time_zone: "UTC".to_string(),
            cookie_resolver: None,
        }
    }
}

/// Shared mutable state for the host bridges.
struct HostState {
    manifest: Manifest,
    config: HostConfig,
    http: HttpClient,
    handle: tokio::runtime::Handle,
    cache: HashMap<String, (String, Instant)>,
    logs: Vec<String>,
    deadline: Instant,
}

impl HostState {
    fn secret(&self, key: &str) -> Option<&String> {
        self.secrets_map().get(key).filter(|v| !v.is_empty())
    }

    fn secrets_map(&self) -> &HashMap<String, String> {
        &self.config.secrets
    }
}

/// Loads a plugin's manifest without running `fetchUsage`.
pub fn load_manifest(source: &str) -> Result<Manifest, PluginError> {
    let runtime = new_runtime()?;
    let context = Context::full(&runtime).map_err(|e| PluginError::Load(e.to_string()))?;
    context.with(|ctx| {
        let json = evaluate_definition(&ctx, source)?;
        let mut manifest: Manifest =
            serde_json::from_str(&json).map_err(|e| PluginError::InvalidManifest(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    })
}

/// Loads the plugin, runs `fetchUsage`, and returns the raw snapshot JSON plus logs.
///
/// MUST be called from a blocking thread: it drives HTTP synchronously through `handle`.
pub fn run(
    source: &str,
    config: HostConfig,
    http: HttpClient,
    handle: tokio::runtime::Handle,
) -> Result<PluginOutcome, PluginError> {
    let runtime = new_runtime()?;
    let context = Context::full(&runtime).map_err(|e| PluginError::Load(e.to_string()))?;

    let (manifest, state) = context.with(|ctx| {
        let json = evaluate_definition(&ctx, source)?;
        let mut manifest: Manifest =
            serde_json::from_str(&json).map_err(|e| PluginError::InvalidManifest(e.to_string()))?;
        manifest.validate()?;
        let state = Rc::new(RefCell::new(HostState {
            manifest: manifest.clone(),
            config,
            http,
            handle,
            cache: HashMap::new(),
            logs: Vec::new(),
            deadline: Instant::now() + FETCH_TIMEOUT,
        }));
        Ok::<_, PluginError>((manifest, state))
    })?;

    // A plugin with declared auth cannot run without its credential; fail before any I/O.
    if let Some(auth) = &manifest.auth {
        let borrowed = state.borrow();
        if borrowed.secret(&auth.secret).is_none() {
            return Err(PluginError::SecretAccess(format!(
                "required auth secret '{}' is unavailable",
                auth.secret
            )));
        }
    }

    let snapshot_json = drive_fetch(&context, &runtime, state.clone())?;
    let logs = std::mem::take(&mut state.borrow_mut().logs);

    Ok(PluginOutcome {
        manifest,
        snapshot_json,
        logs,
    })
}

fn new_runtime() -> Result<Runtime, PluginError> {
    let runtime = Runtime::new().map_err(|e| PluginError::Load(e.to_string()))?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    Ok(runtime)
}

/// Evaluates the plugin source with a capturing `defineProvider` and returns the
/// definition as JSON (functions drop out, exactly like upstream's JSON conversion).
fn evaluate_definition<'js>(ctx: &Ctx<'js>, source: &str) -> Result<String, PluginError> {
    let globals = ctx.globals();
    globals
        .set(
            "defineProvider",
            Func::from(
                |ctx: Ctx<'js>, definition: Object<'js>| -> rquickjs::Result<()> {
                    ctx.globals().set("__codexbar_definition", definition)
                },
            ),
        )
        .map_err(|e| PluginError::Load(e.to_string()))?;

    ctx.eval::<(), _>(source)
        .catch(ctx)
        .map_err(|e| PluginError::Load(format!("plugin script failed: {e}")))?;

    let definition: Value = globals
        .get("__codexbar_definition")
        .map_err(|e| PluginError::Load(e.to_string()))?;
    if definition.is_undefined() || definition.is_null() {
        return Err(PluginError::Load(
            "plugin never called defineProvider".into(),
        ));
    }
    let object = definition
        .as_object()
        .ok_or_else(|| PluginError::InvalidManifest("definition must be an object".into()))?;
    let fetch_usage: Value = object
        .get("fetchUsage")
        .map_err(|e| PluginError::InvalidManifest(e.to_string()))?;
    if !fetch_usage.is_function() {
        return Err(PluginError::InvalidManifest(
            "definition.fetchUsage must be a function".into(),
        ));
    }

    let json = ctx
        .json_stringify(definition)
        .map_err(|e| PluginError::InvalidManifest(e.to_string()))?
        .ok_or_else(|| {
            PluginError::InvalidManifest("definition is not JSON-serializable".into())
        })?;
    json.to_string()
        .map_err(|e| PluginError::InvalidManifest(e.to_string()))
}

/// Builds `ctx`, applies the prelude, calls `fetchUsage`, then pumps the job queue until
/// the returned promise settles.
///
/// Job pumping MUST happen outside `Context::with`: that guard holds the runtime lock,
/// and `execute_pending_job` takes it again.
fn drive_fetch(
    context: &Context,
    runtime: &Runtime,
    state: Rc<RefCell<HostState>>,
) -> Result<String, PluginError> {
    let deadline = state.borrow().deadline;

    context.with(|ctx| -> Result<(), PluginError> {
        let plugin_ctx = Object::new(ctx.clone()).map_err(js_error)?;
        let now_millis = (state.borrow().config.now.unix_timestamp_nanos() / 1_000_000) as f64;
        plugin_ctx
            .set("__codexbarNowMillis", now_millis)
            .map_err(js_error)?;

        let env = Object::new(ctx.clone()).map_err(js_error)?;
        env.set("timeZone", state.borrow().config.time_zone.clone())
            .map_err(js_error)?;
        plugin_ctx.set("env", env).map_err(js_error)?;

        let host = build_host(&ctx, state.clone())?;

        let apply_prelude: Function = ctx
            .eval(PRELUDE)
            .catch(&ctx)
            .map_err(|e| PluginError::Load(format!("prelude failed: {e}")))?;
        apply_prelude
            .call::<_, Value>((plugin_ctx.clone(), host))
            .catch(&ctx)
            .map_err(|e| PluginError::Load(format!("prelude failed: {e}")))?;

        let definition: Object = ctx
            .globals()
            .get("__codexbar_definition")
            .map_err(js_error)?;
        let fetch_usage: Function = definition.get("fetchUsage").map_err(js_error)?;

        let result: Value = fetch_usage
            .call((rquickjs::function::This(definition.clone()), plugin_ctx))
            .catch(&ctx)
            .map_err(|e| classify_js_error(&e.to_string()))?;
        // Park the result in a global so it outlives this borrow of the runtime.
        ctx.globals()
            .set("__codexbar_result", result)
            .map_err(js_error)?;
        Ok(())
    })?;

    loop {
        match context.with(|ctx| poll_result(&ctx))? {
            Settled::Json(json) => return Ok(json),
            Settled::Pending => {}
        }

        if Instant::now() > deadline {
            return Err(PluginError::TimedOut);
        }
        // Host callbacks resolve synchronously, so an empty job queue with a pending
        // promise means the plugin awaited something the host cannot deliver.
        if !runtime.is_job_pending() {
            return Err(PluginError::Script(
                "fetchUsage never settled (awaited something the host cannot resolve)".into(),
            ));
        }
        runtime
            .execute_pending_job()
            .map_err(|e| PluginError::Script(format!("job failed: {e:?}")))?;
    }
}

enum Settled {
    Pending,
    Json(String),
}

/// Inspects the parked result, returning its JSON once it settles.
fn poll_result(ctx: &Ctx<'_>) -> Result<Settled, PluginError> {
    let value: Value = ctx.globals().get("__codexbar_result").map_err(js_error)?;

    let resolved = match value.as_promise() {
        None => value,
        Some(promise) => match promise.state() {
            rquickjs::promise::PromiseState::Pending => return Ok(Settled::Pending),
            rquickjs::promise::PromiseState::Resolved => match promise.result::<Value>() {
                Some(Ok(value)) => value,
                Some(Err(err)) => return Err(PluginError::Script(err.to_string())),
                None => Value::new_undefined(ctx.clone()),
            },
            rquickjs::promise::PromiseState::Rejected => {
                // `result()` re-throws the rejection into the context; take it back out.
                let _ = promise.result::<Value>();
                let thrown = ctx.catch();
                return Err(classify_js_error(&describe(ctx, &thrown)));
            }
        },
    };

    let json = ctx
        .json_stringify(resolved)
        .map_err(js_error)?
        .ok_or_else(|| {
            PluginError::InvalidSnapshot("fetchUsage resolved to a non-JSON value".into())
        })?;
    json.to_string().map(Settled::Json).map_err(js_error)
}

fn describe<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> String {
    if let Some(object) = value.as_object() {
        if let Ok(message) = object.get::<_, String>("message") {
            return message;
        }
    }
    ctx.json_stringify(value.clone())
        .ok()
        .flatten()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| "unknown plugin error".to_string())
}

/// Maps a thrown JS error onto our taxonomy, honouring the prelude's classified-failure
/// marker (`__CODEXBAR_FAILURE_V2__:<kind>:<retryAfter>:<message>`).
fn classify_js_error(message: &str) -> PluginError {
    const MARKER: &str = "__CODEXBAR_FAILURE_V2__:";
    let Some(index) = message.find(MARKER) else {
        return PluginError::Script(message.trim().to_string());
    };
    let payload = &message[index + MARKER.len()..];
    let mut parts = payload.splitn(3, ':');
    let kind = parts.next().unwrap_or_default();
    let retry_after = parts.next().unwrap_or_default();
    let detail = parts.next().unwrap_or_default();

    let retry_after_seconds = retry_after
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        // Upstream clamps the plugin's request to 10 seconds.
        .map(|v| v.min(10.0));

    PluginError::Classified(PluginFailure {
        kind: kind.to_string(),
        message: detail.trim().to_string(),
        retry_after_seconds,
    })
}

fn js_error(error: rquickjs::Error) -> PluginError {
    PluginError::Script(error.to_string())
}

/// Builds the native `host` object the prelude closes over.
fn build_host<'js>(
    ctx: &Ctx<'js>,
    state: Rc<RefCell<HostState>>,
) -> Result<Object<'js>, PluginError> {
    let host = Object::new(ctx.clone()).map_err(js_error)?;

    {
        let state = state.clone();
        host.set(
            "settingGet",
            Func::from(move |key: String, secure: bool| -> Option<String> {
                let state = state.borrow();
                let expected = if secure {
                    SettingKind::Secure
                } else {
                    SettingKind::Plain
                };
                // Undeclared keys must not resolve, matching upstream's declaration gate.
                match state.manifest.setting(&key) {
                    Some(declared) if declared.kind == expected => {}
                    _ => return None,
                }
                let values = if secure {
                    &state.config.secrets
                } else {
                    &state.config.settings
                };
                values.get(&key).filter(|v| !v.is_empty()).cloned()
            }),
        )
        .map_err(js_error)?;
    }

    {
        let state = state.clone();
        host.set(
            "log",
            Func::from(move |message: String| {
                let mut state = state.borrow_mut();
                let redacted = redact(&message, &state.config.secrets);
                tracing::debug!(plugin = %state.manifest.id, "{redacted}");
                if state.logs.len() < 200 {
                    state.logs.push(redacted);
                }
            }),
        )
        .map_err(js_error)?;
    }

    {
        let state = state.clone();
        host.set(
            "cacheGet",
            Func::from(
                move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<Value<'js>> {
                    let hit = {
                        let state = state.borrow();
                        state
                            .cache
                            .get(&key)
                            .filter(|(_, expires)| *expires > Instant::now())
                            .map(|(json, _)| json.clone())
                    };
                    match hit {
                        Some(json) => ctx.json_parse(json),
                        None => Ok(Value::new_undefined(ctx)),
                    }
                },
            ),
        )
        .map_err(js_error)?;
    }

    {
        let state = state.clone();
        host.set(
            "cacheSet",
            Func::from(
                move |ctx: Ctx<'js>,
                      key: String,
                      value: Value<'js>,
                      ttl_seconds: f64|
                      -> rquickjs::Result<()> {
                    if !(ttl_seconds.is_finite() && ttl_seconds > 0.0) {
                        return Ok(());
                    }
                    if let Some(json) = ctx.json_stringify(value)? {
                        let json = json.to_string()?;
                        state.borrow_mut().cache.insert(
                            key,
                            (
                                json,
                                Instant::now() + Duration::from_secs_f64(ttl_seconds.min(3600.0)),
                            ),
                        );
                    }
                    Ok(())
                },
            ),
        )
        .map_err(js_error)?;
    }

    // `pct` and `amountFromPercent` are pure math the prelude delegates to the host.
    host.set(
        "pct",
        Func::from(|used: f64, limit: f64| -> f64 {
            if !(used.is_finite() && limit.is_finite()) || limit <= 0.0 {
                return 0.0;
            }
            ((used / limit) * 100.0).clamp(0.0, 100.0)
        }),
    )
    .map_err(js_error)?;

    host.set(
        "amountFromPercent",
        Func::from(|percent: f64, limit: f64| -> f64 {
            if !(percent.is_finite() && limit.is_finite()) {
                return 0.0;
            }
            (percent.clamp(0.0, 100.0) / 100.0) * limit
        }),
    )
    .map_err(js_error)?;

    {
        let state = state.clone();
        host.set(
            "nextDailyReset",
            Func::from(move |zone: String, hour: f64| -> f64 {
                let now = state.borrow().config.now;
                super::timezone::next_daily_reset(&zone, hour as u8, now)
                    .map(|at| (at.unix_timestamp_nanos() / 1_000_000) as f64)
                    .unwrap_or(f64::NAN)
            }),
        )
        .map_err(js_error)?;
    }

    {
        let state = state.clone();
        host.set(
            "cookieHeader",
            Func::from(
                move |ctx: Ctx<'js>,
                      domain: String,
                      resolve: Function<'js>,
                      reject: Function<'js>| {
                    let domain = domain.trim().to_lowercase();
                    let outcome = {
                        let state = state.borrow();
                        // The manifest must declare both the capability and the domain.
                        if !state.manifest.has_capability(Capability::BrowserCookies) {
                            Err("browser-cookies capability is not declared".to_string())
                        } else if !state.manifest.cookie_domains.iter().any(|d| *d == domain) {
                            Err("cookie domain is not declared".to_string())
                        } else {
                            match &state.config.cookie_resolver {
                                Some(resolver) => resolver(&domain),
                                None => Err("cookie import is unavailable".to_string()),
                            }
                        }
                    };

                    match outcome {
                        Ok(header) => {
                            let value =
                                rquickjs::String::from_str(ctx.clone(), &header)?.into_value();
                            resolve.call::<_, Value>((value,))?;
                        }
                        Err(message) => {
                            let value =
                                rquickjs::String::from_str(ctx.clone(), &message)?.into_value();
                            reject.call::<_, Value>((value,))?;
                        }
                    }
                    Ok::<_, rquickjs::Error>(())
                },
            ),
        )
        .map_err(js_error)?;
    }

    {
        let state = state.clone();
        host.set(
            "http",
            Func::from(
                move |ctx: Ctx<'js>,
                      url: String,
                      options: Object<'js>,
                      method: String,
                      wants_json: bool,
                      resolve: Function<'js>,
                      reject: Function<'js>|
                      -> rquickjs::Result<()> {
                    match perform_request(&ctx, &state, &url, &options, &method) {
                        Ok(response) => {
                            let payload = response_payload(&ctx, response, wants_json);
                            match payload {
                                Ok(value) => {
                                    resolve.call::<_, Value>((value,))?;
                                }
                                Err(message) => {
                                    let value = rquickjs::String::from_str(ctx.clone(), &message)?
                                        .into_value();
                                    reject.call::<_, Value>((value,))?;
                                }
                            }
                        }
                        Err(message) => {
                            let value =
                                rquickjs::String::from_str(ctx.clone(), &message)?.into_value();
                            reject.call::<_, Value>((value,))?;
                        }
                    }
                    Ok(())
                },
            ),
        )
        .map_err(js_error)?;
    }

    Ok(host)
}

struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Validates the request against the manifest's network policy and performs it.
fn perform_request(
    _ctx: &Ctx<'_>,
    state: &Rc<RefCell<HostState>>,
    raw_url: &str,
    options: &Object<'_>,
    method: &str,
) -> Result<RawResponse, String> {
    let (http, handle, request) = {
        let state = state.borrow();
        let url = Url::parse(raw_url).map_err(|_| "request URL is invalid".to_string())?;
        if !allowed_origin(&state.manifest, &state.config.settings, &url) {
            return Err(format!("origin '{}' is not declared", origin_label(&url)));
        }
        let method = match method {
            "GET" => Method::GET,
            "POST" => Method::POST,
            _ => return Err("HTTP method is not allowed".to_string()),
        };

        let timeout = read_timeout(options)?;
        let mut builder = state
            .http
            .raw()
            .request(method.clone(), url.clone())
            .timeout(Duration::from_secs_f64(timeout));

        if method == Method::POST {
            let body: String = options
                .get("bodyJSON")
                .map_err(|_| "POST JSON body is missing".to_string())?;
            builder = builder
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }

        let auth_header = state
            .manifest
            .auth
            .as_ref()
            .map(|a| a.header_name().to_lowercase());
        if let Ok(headers) = options.get::<_, Object>("headers") {
            for entry in headers.props::<String, String>() {
                let (name, value) =
                    entry.map_err(|_| "request header must be a string".to_string())?;
                if auth_header
                    .as_ref()
                    .is_some_and(|auth| auth == &name.to_lowercase())
                {
                    return Err("plugins may not override the auth header".to_string());
                }
                builder = builder.header(name, value);
            }
        }

        builder = builder.header(reqwest::header::ACCEPT, "application/json");

        if let Some(auth) = &state.manifest.auth {
            let credential = state
                .secret(&auth.secret)
                .ok_or_else(|| "required auth secret is unavailable".to_string())?;
            builder = builder.header(auth.header_name(), auth.header_value(credential));
        }

        (state.http.clone(), state.handle.clone(), builder)
    };
    let _ = http;

    // Blocking thread + runtime handle: the plugin API is synchronous by construction.
    let response = handle
        .block_on(async move { request.send().await })
        .map_err(|e| format!("request failed: {e}"))?;

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_lowercase(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let body = handle
        .block_on(async move { response.bytes().await })
        .map_err(|e| format!("request failed: {e}"))?;

    if body.len() > RESPONSE_LIMIT {
        return Err(format!("response exceeded the {RESPONSE_LIMIT}-byte limit"));
    }

    Ok(RawResponse {
        status,
        headers,
        body: body.to_vec(),
    })
}

fn read_timeout(options: &Object<'_>) -> Result<f64, String> {
    match options.get::<_, Option<f64>>("timeoutSeconds") {
        Ok(None) => Ok(DEFAULT_REQUEST_TIMEOUT),
        Ok(Some(seconds))
            if seconds.is_finite()
                && (MIN_REQUEST_TIMEOUT..=MAX_REQUEST_TIMEOUT).contains(&seconds) =>
        {
            Ok(seconds)
        }
        _ => Err("timeoutSeconds must be a number from 1 through 30".to_string()),
    }
}

/// Builds the `{ status, headers, json | bodyText }` object plugins receive.
fn response_payload<'js>(
    ctx: &Ctx<'js>,
    response: RawResponse,
    wants_json: bool,
) -> Result<Value<'js>, String> {
    let payload = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
    payload
        .set("status", response.status as f64)
        .map_err(|e| e.to_string())?;

    let headers = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
    for (name, value) in response.headers {
        headers.set(name, value).map_err(|e| e.to_string())?;
    }
    payload.set("headers", headers).map_err(|e| e.to_string())?;

    let text = String::from_utf8(response.body)
        .map_err(|_| "response body was not valid UTF-8".to_string())?;
    if wants_json {
        let parsed = ctx
            .json_parse(text)
            .map_err(|_| "response was not valid JSON".to_string())?;
        payload.set("json", parsed).map_err(|e| e.to_string())?;
    } else {
        payload.set("bodyText", text).map_err(|e| e.to_string())?;
    }

    Ok(payload.into_value())
}

/// Upstream `allowedOrigin(for:settings:)`: a request must match a fixed origin or a
/// configured origin under its declared policy.
fn allowed_origin(manifest: &Manifest, settings: &HashMap<String, String>, url: &Url) -> bool {
    for endpoint in &manifest.endpoints {
        match endpoint {
            Endpoint::Fixed(declared) => {
                let Ok(declared) = super::manifest::normalized_origin(declared) else {
                    continue;
                };
                if super::manifest::request_origin(url, EndpointPolicy::Https)
                    .map(|origin| origin == declared)
                    .unwrap_or(false)
                {
                    return true;
                }
            }
            Endpoint::Setting { setting, policy } => {
                let Some(raw) = settings.get(setting).filter(|v| !v.is_empty()) else {
                    continue;
                };
                let Ok(configured) = Url::parse(raw) else {
                    continue;
                };
                if configured.fragment().is_some() {
                    continue;
                }
                let (Ok(configured_origin), Ok(request)) = (
                    super::manifest::request_origin(&configured, *policy),
                    super::manifest::request_origin(url, *policy),
                ) else {
                    continue;
                };
                if configured_origin == request {
                    return true;
                }
            }
        }
    }
    false
}

fn origin_label(url: &Url) -> String {
    match url.host_str() {
        Some(host) => format!("{}://{host}", url.scheme()),
        None => "invalid".to_string(),
    }
}

/// Keeps credentials out of logs and error text.
fn redact(message: &str, secrets: &HashMap<String, String>) -> String {
    let mut out = message.to_string();
    for value in secrets.values() {
        if value.len() >= 6 {
            out = out.replace(value.as_str(), "<redacted>");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const VENICE: &str = include_str!("../../resources/plugins/venice.js");

    #[test]
    fn loads_a_bundled_manifest() {
        let manifest = load_manifest(VENICE).expect("venice manifest loads");
        assert_eq!(manifest.id, "venice");
        assert_eq!(manifest.name, "Venice");
        assert_eq!(manifest.settings.len(), 1);
        assert_eq!(manifest.auth.as_ref().unwrap().secret, "VENICE_API_KEY");
    }

    #[test]
    fn every_bundled_plugin_has_a_valid_manifest() {
        for (name, source) in super::super::BUNDLED {
            let manifest = load_manifest(source).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!manifest.id.is_empty(), "{name}");
            assert!(!manifest.endpoints.is_empty(), "{name}");
        }
    }

    #[test]
    fn rejects_scripts_that_never_define_a_provider() {
        let err = load_manifest("const x = 1;").unwrap_err();
        assert!(matches!(err, PluginError::Load(m) if m.contains("never called defineProvider")));
    }

    #[test]
    fn classifies_prelude_failure_markers() {
        let err = classify_js_error("Error: __CODEXBAR_FAILURE_V2__:rate-limited:30:slow down");
        match err {
            PluginError::Classified(failure) => {
                assert_eq!(failure.kind, "rate-limited");
                assert_eq!(failure.message, "slow down");
                assert_eq!(failure.retry_after_seconds, Some(10.0), "clamped to 10s");
            }
            other => panic!("expected classified failure, got {other:?}"),
        }

        let err = classify_js_error("Error: __CODEXBAR_FAILURE_V2__:missing-credential::no key");
        match err {
            PluginError::Classified(failure) => {
                assert_eq!(failure.kind, "missing-credential");
                assert_eq!(failure.retry_after_seconds, None);
            }
            other => panic!("expected classified failure, got {other:?}"),
        }

        assert!(
            matches!(classify_js_error("TypeError: boom"), PluginError::Script(m) if m.contains("boom"))
        );
    }

    #[test]
    fn origin_gate_matches_fixed_and_configured_endpoints() {
        let mut manifest: Manifest = serde_json::from_str(
            r#"{ "id": "demo", "name": "Demo",
                 "endpoints": ["https://api.demo.test", { "setting": "BASE", "policy": "https-or-loopback-http" }],
                 "settings": [{ "key": "BASE", "title": "Base" }] }"#,
        )
        .unwrap();
        manifest.validate().unwrap();

        let mut settings = HashMap::new();
        settings.insert("BASE".to_string(), "http://127.0.0.1:9000".to_string());

        assert!(allowed_origin(
            &manifest,
            &settings,
            &Url::parse("https://api.demo.test/v1/usage").unwrap()
        ));
        assert!(allowed_origin(
            &manifest,
            &settings,
            &Url::parse("http://127.0.0.1:9000/quota").unwrap()
        ));
        assert!(!allowed_origin(
            &manifest,
            &settings,
            &Url::parse("https://evil.test/v1").unwrap()
        ));
        assert!(
            !allowed_origin(
                &manifest,
                &HashMap::new(),
                &Url::parse("http://127.0.0.1:9000/q").unwrap()
            ),
            "unconfigured setting endpoint must not match"
        );
    }

    #[test]
    fn secrets_are_redacted_from_log_output() {
        let mut secrets = HashMap::new();
        secrets.insert("K".to_string(), "super-secret-token".to_string());
        assert_eq!(
            redact("calling with super-secret-token", &secrets),
            "calling with <redacted>"
        );
    }

    #[test]
    fn request_timeout_bounds_match_upstream() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        context.with(|ctx| {
            let empty = Object::new(ctx.clone()).unwrap();
            assert_eq!(read_timeout(&empty).unwrap(), 15.0);

            let ok = Object::new(ctx.clone()).unwrap();
            ok.set("timeoutSeconds", 5.0).unwrap();
            assert_eq!(read_timeout(&ok).unwrap(), 5.0);

            for bad in [0.5f64, 31.0, f64::NAN] {
                let object = Object::new(ctx.clone()).unwrap();
                object.set("timeoutSeconds", bad).unwrap();
                assert!(read_timeout(&object).is_err(), "{bad}");
            }
        });
    }
}
