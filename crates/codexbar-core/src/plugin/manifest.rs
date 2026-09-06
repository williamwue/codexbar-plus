//! `defineProvider(definition)` manifest.
//!
//! Port of `Plugins/ProviderPluginManifest.swift:3-252,370-416`. The definition object is
//! JSON-serialised in JS (functions drop out) and validated here, exactly like upstream.

use serde::Deserialize;
use url::Url;

use super::PluginError;

/// Upstream caps: `ProviderPluginManifest.swift:81-252`.
const MAX_NAME_LEN: usize = 80;
const MAX_ENDPOINTS: usize = 16;
const MAX_SETTINGS: usize = 32;
const MAX_ID_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingKind {
    Plain,
    Secure,
}

impl SettingKind {
    fn label(&self) -> &'static str {
        match self {
            SettingKind::Plain => "plain",
            SettingKind::Secure => "secure",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Setting {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default = "default_setting_kind", rename = "type")]
    pub kind: SettingKind,
}

fn default_setting_kind() -> SettingKind {
    SettingKind::Plain
}

/// Which schemes a configured endpoint may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointPolicy {
    Https,
    HttpsOrLoopbackHttp,
    HttpsOrPrivateNetworkHttp,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Endpoint {
    /// A literal HTTPS origin baked into the plugin.
    Fixed(String),
    /// An origin the user configures through a declared plain setting.
    Setting {
        setting: String,
        policy: EndpointPolicy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthType {
    Bearer,
    XApiKey,
    Header,
    AuthorizationScheme,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Auth {
    #[serde(rename = "type")]
    pub auth_type: AuthType,
    /// Only meaningful for `type: "header"`.
    #[serde(default)]
    pub header: Option<String>,
    /// Only meaningful for `type: "authorization-scheme"`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Key of the secure setting holding the credential.
    pub secret: String,
}

impl Auth {
    /// Header the credential is written to (upstream `ProviderPluginManifest` auth header).
    pub fn header_name(&self) -> &str {
        match self.auth_type {
            AuthType::Bearer | AuthType::AuthorizationScheme => "Authorization",
            AuthType::XApiKey => "x-api-key",
            AuthType::Header => self.header.as_deref().unwrap_or("Authorization"),
        }
    }

    /// Header value for a resolved credential.
    pub fn header_value(&self, credential: &str) -> String {
        match self.auth_type {
            AuthType::Bearer => format!("Bearer {credential}"),
            AuthType::AuthorizationScheme => {
                format!(
                    "{} {credential}",
                    self.scheme.as_deref().unwrap_or("Bearer")
                )
            }
            AuthType::XApiKey | AuthType::Header => credential.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    BrowserCookies,
    HttpStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Icon {
    #[serde(default)]
    pub monogram: Option<String>,
    #[serde(default)]
    pub tint: Option<String>,
}

/// The validated manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: Icon,
    pub endpoints: Vec<Endpoint>,
    #[serde(default)]
    pub auth: Option<Auth>,
    #[serde(default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default, rename = "cookieDomains")]
    pub cookie_domains: Vec<String>,
}

impl Manifest {
    pub fn has_capability(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key == key)
    }

    /// Validates every rule upstream enforces at load time.
    pub fn validate(&mut self) -> Result<(), PluginError> {
        self.id = self.id.trim().to_lowercase();
        if self.id.is_empty() || self.id.len() > MAX_ID_LEN {
            return Err(PluginError::InvalidManifest(format!(
                "id must be 1-{MAX_ID_LEN} characters"
            )));
        }
        if !self
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err(PluginError::InvalidManifest(
                "id may only contain lowercase letters, digits, '.', '_' and '-'".into(),
            ));
        }

        self.name = self.name.trim().to_string();
        if self.name.is_empty() || self.name.chars().count() > MAX_NAME_LEN {
            return Err(PluginError::InvalidManifest(format!(
                "name must be 1-{MAX_NAME_LEN} characters"
            )));
        }

        if self.endpoints.is_empty() || self.endpoints.len() > MAX_ENDPOINTS {
            return Err(PluginError::InvalidManifest(format!(
                "endpoints must contain 1-{MAX_ENDPOINTS} entries"
            )));
        }

        if self.settings.len() > MAX_SETTINGS {
            return Err(PluginError::InvalidManifest(format!(
                "settings must contain at most {MAX_SETTINGS} entries"
            )));
        }
        let mut seen = Vec::with_capacity(self.settings.len());
        for setting in &self.settings {
            if setting.key.trim().is_empty() {
                return Err(PluginError::InvalidManifest(
                    "setting key must not be empty".into(),
                ));
            }
            if seen.contains(&setting.key) {
                return Err(PluginError::InvalidManifest(format!(
                    "duplicate setting key '{}'",
                    setting.key
                )));
            }
            seen.push(setting.key.clone());
        }

        // Fixed endpoints must be normalizable HTTPS origins; configured endpoints must
        // point at a declared *plain* setting.
        for endpoint in &self.endpoints {
            match endpoint {
                Endpoint::Fixed(raw) => {
                    normalized_origin(raw)?;
                }
                Endpoint::Setting { setting, .. } => match self.setting(setting) {
                    Some(declared) if declared.kind == SettingKind::Plain => {}
                    Some(declared) => {
                        return Err(PluginError::InvalidManifest(format!(
                            "endpoint setting '{setting}' must be plain, found {}",
                            declared.kind.label()
                        )))
                    }
                    None => {
                        return Err(PluginError::InvalidManifest(format!(
                            "endpoint setting '{setting}' is not declared"
                        )))
                    }
                },
            }
        }

        if let Some(auth) = &self.auth {
            match self.setting(&auth.secret) {
                Some(declared) if declared.kind == SettingKind::Secure => {}
                Some(_) => {
                    return Err(PluginError::InvalidManifest(format!(
                        "auth secret '{}' must be a secure setting",
                        auth.secret
                    )))
                }
                None => {
                    return Err(PluginError::InvalidManifest(format!(
                        "auth secret '{}' is not declared",
                        auth.secret
                    )))
                }
            }
            if auth.auth_type == AuthType::Header
                && auth.header.as_deref().unwrap_or("").trim().is_empty()
            {
                return Err(PluginError::InvalidManifest(
                    "header auth requires a header name".into(),
                ));
            }
            if auth.auth_type == AuthType::AuthorizationScheme
                && auth.scheme.as_deref().unwrap_or("").trim().is_empty()
            {
                return Err(PluginError::InvalidManifest(
                    "authorization-scheme auth requires a scheme".into(),
                ));
            }
        }

        self.cookie_domains = self
            .cookie_domains
            .iter()
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if self.has_capability(Capability::BrowserCookies) && self.cookie_domains.is_empty() {
            return Err(PluginError::InvalidManifest(
                "browser-cookies capability requires cookieDomains".into(),
            ));
        }

        Ok(())
    }
}

/// Normalises a declared HTTPS origin (upstream `ProviderPluginOrigin.normalizedOrigin`).
///
/// Rejects anything carrying credentials, a query, a fragment or a path beyond `/`.
pub fn normalized_origin(raw: &str) -> Result<String, PluginError> {
    let url = Url::parse(raw.trim()).map_err(|_| {
        PluginError::InvalidManifest(format!("endpoint '{raw}' must be an HTTPS origin"))
    })?;
    if url.scheme() != "https"
        || url.host_str().unwrap_or("").is_empty()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.path().is_empty() || url.path() == "/")
    {
        return Err(PluginError::InvalidManifest(format!(
            "endpoint '{raw}' must be an HTTPS origin"
        )));
    }
    Ok(format_origin(&url))
}

/// Origin of a request URL under a declared policy
/// (upstream `normalizedOrigin(of:policy:)`).
pub fn request_origin(url: &Url, policy: EndpointPolicy) -> Result<String, PluginError> {
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(PluginError::NetworkPolicy(
            "URL does not satisfy the declared endpoint policy".into(),
        ));
    }
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| PluginError::NetworkPolicy("request URL has no host".into()))?;

    let allowed = match url.scheme() {
        "https" => true,
        "http" => match policy {
            EndpointPolicy::Https => false,
            EndpointPolicy::HttpsOrLoopbackHttp => is_loopback_host(host),
            EndpointPolicy::HttpsOrPrivateNetworkHttp => {
                is_loopback_host(host) || is_private_host(host)
            }
        },
        _ => false,
    };
    if !allowed {
        return Err(PluginError::NetworkPolicy(
            "URL does not satisfy the declared endpoint policy".into(),
        ));
    }
    Ok(format_origin(url))
}

fn format_origin(url: &Url) -> String {
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or_default().to_lowercase();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    match url.port() {
        Some(port) if port != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn is_private_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return ip.is_private() || ip.is_link_local();
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        // fc00::/7 unique-local plus fe80::/10 link-local.
        let seg = ip.segments()[0];
        return (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80;
    }
    host.ends_with(".local") || host.ends_with(".internal")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(extra: &str) -> String {
        format!(
            r#"{{ "id": "demo", "name": "Demo", "endpoints": ["https://api.demo.test"], {extra} "settings": [] }}"#
        )
    }

    fn parse(json: &str) -> Result<Manifest, PluginError> {
        let mut manifest: Manifest =
            serde_json::from_str(json).map_err(|e| PluginError::InvalidManifest(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    #[test]
    fn accepts_the_shape_bundled_plugins_use() {
        let manifest = parse(
            r#"{
                "id": "Venice", "name": "Venice",
                "endpoints": ["https://api.venice.ai"],
                "auth": { "type": "bearer", "secret": "VENICE_API_KEY" },
                "settings": [{ "key": "VENICE_API_KEY", "title": "API key", "type": "secure" }]
            }"#,
        )
        .expect("valid manifest");
        assert_eq!(manifest.id, "venice", "id is lowercased");
        assert_eq!(
            manifest.auth.as_ref().unwrap().header_name(),
            "Authorization"
        );
        assert_eq!(
            manifest.auth.as_ref().unwrap().header_value("k"),
            "Bearer k"
        );
    }

    #[test]
    fn auth_header_names_follow_the_declared_type() {
        let x_api: Auth =
            serde_json::from_str(r#"{ "type": "x-api-key", "secret": "K" }"#).unwrap();
        assert_eq!(x_api.header_name(), "x-api-key");
        assert_eq!(x_api.header_value("abc"), "abc");

        let custom: Auth =
            serde_json::from_str(r#"{ "type": "header", "header": "X-Token", "secret": "K" }"#)
                .unwrap();
        assert_eq!(custom.header_name(), "X-Token");

        let scheme: Auth = serde_json::from_str(
            r#"{ "type": "authorization-scheme", "scheme": "Token", "secret": "K" }"#,
        )
        .unwrap();
        assert_eq!(scheme.header_value("abc"), "Token abc");
    }

    #[test]
    fn rejects_undeclared_or_wrongly_typed_auth_secret() {
        let err = parse(&manifest_json(
            r#""auth": { "type": "bearer", "secret": "MISSING" },"#,
        ))
        .unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(m) if m.contains("not declared")));

        let err = parse(
            r#"{ "id": "demo", "name": "Demo", "endpoints": ["https://a.test"],
                 "auth": { "type": "bearer", "secret": "K" },
                 "settings": [{ "key": "K", "title": "k", "type": "plain" }] }"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidManifest(m) if m.contains("must be a secure setting"))
        );
    }

    #[test]
    fn configured_endpoints_must_reference_a_plain_setting() {
        let ok = parse(
            r#"{ "id": "demo", "name": "Demo",
                 "endpoints": [{ "setting": "BASE", "policy": "https-or-loopback-http" }],
                 "settings": [{ "key": "BASE", "title": "Base URL" }] }"#,
        );
        assert!(ok.is_ok());

        let err = parse(
            r#"{ "id": "demo", "name": "Demo",
                 "endpoints": [{ "setting": "BASE", "policy": "https" }],
                 "settings": [{ "key": "BASE", "title": "Base URL", "type": "secure" }] }"#,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(m) if m.contains("must be plain")));
    }

    #[test]
    fn cookie_capability_requires_domains() {
        let err = parse(
            r#"{ "id": "demo", "name": "Demo", "endpoints": ["https://a.test"],
                 "capabilities": ["browser-cookies"], "settings": [] }"#,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(m) if m.contains("cookieDomains")));
    }

    #[test]
    fn origin_normalisation_strips_default_port_and_rejects_paths() {
        assert_eq!(
            normalized_origin("https://api.demo.test").unwrap(),
            "https://api.demo.test"
        );
        assert_eq!(
            normalized_origin("https://API.demo.test:443/").unwrap(),
            "https://api.demo.test"
        );
        assert_eq!(
            normalized_origin("https://api.demo.test:8443").unwrap(),
            "https://api.demo.test:8443"
        );
        assert!(normalized_origin("https://api.demo.test/v1").is_err());
        assert!(normalized_origin("http://api.demo.test").is_err());
        assert!(normalized_origin("https://user:pw@api.demo.test").is_err());
    }

    #[test]
    fn request_origin_enforces_the_endpoint_policy() {
        let https = Url::parse("https://api.demo.test/v1/usage?x=1").unwrap();
        assert_eq!(
            request_origin(&https, EndpointPolicy::Https).unwrap(),
            "https://api.demo.test"
        );

        let loopback = Url::parse("http://127.0.0.1:4000/v1").unwrap();
        assert!(request_origin(&loopback, EndpointPolicy::Https).is_err());
        assert_eq!(
            request_origin(&loopback, EndpointPolicy::HttpsOrLoopbackHttp).unwrap(),
            "http://127.0.0.1:4000"
        );

        let private = Url::parse("http://192.168.1.10:8080/").unwrap();
        assert!(request_origin(&private, EndpointPolicy::HttpsOrLoopbackHttp).is_err());
        assert_eq!(
            request_origin(&private, EndpointPolicy::HttpsOrPrivateNetworkHttp).unwrap(),
            "http://192.168.1.10:8080"
        );

        let fragment = Url::parse("https://api.demo.test/#x").unwrap();
        assert!(request_origin(&fragment, EndpointPolicy::Https).is_err());
    }
}
