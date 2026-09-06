//! Shared HTTP transport with the upstream retry contract.
//!
//! Port of `Sources/CodexBarCore/ProviderHTTPClient.swift:6-183`:
//! retry only idempotent methods, only on 408/429/5xx or transport errors,
//! exponential backoff capped at 10s, honour `Retry-After`.

use std::time::Duration;

use reqwest::{header::HeaderMap, Method, StatusCode};
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("unauthorized (HTTP {0})")]
    Unauthorized(u16),
    #[error("rate limited; retry after {0:?}")]
    RateLimited(Option<Duration>),
    #[error("HTTP {status}: {body}")]
    Server { status: u16, body: String },
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("decode error: {0}")]
    Decode(String),
}

/// Retry rules (upstream `RetryPolicy`, `ProviderHTTPClient.swift:38-95`).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl RetryPolicy {
    /// Upstream retries 408/429/500/502/503/504 only.
    pub fn should_retry_status(status: StatusCode) -> bool {
        matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
    }

    /// Upstream retries GET/HEAD/OPTIONS only; anything with side effects is left alone.
    pub fn is_idempotent(method: &Method) -> bool {
        matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
    }

    /// Exponential backoff `base * 2^(attempt-1)`, clamped to `max_delay`.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let factor = 1u64 << attempt.saturating_sub(1).min(16);
        let delay = self.base_delay.saturating_mul(factor as u32);
        delay.min(self.max_delay)
    }
}

/// Parses `Retry-After` as delta-seconds (the only form providers send here).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Shared client. One connection pool for every provider, like upstream's singleton.
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    policy: RetryPolicy,
}

impl HttpClient {
    pub fn new() -> Result<Self, HttpError> {
        let inner = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            inner,
            policy: RetryPolicy::default(),
        })
    }

    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.inner
    }

    /// Sends a request, retrying per policy. `build` is called per attempt because
    /// `reqwest::Request` is not cloneable when it carries a streaming body.
    pub async fn send<F>(&self, method: Method, build: F) -> Result<Response, HttpError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempt = 1u32;
        loop {
            let outcome = build().send().await;
            match outcome {
                Ok(resp) => {
                    let status = resp.status();
                    let headers = resp.headers().clone();
                    if RetryPolicy::should_retry_status(status)
                        && RetryPolicy::is_idempotent(&method)
                        && attempt < self.policy.max_attempts
                    {
                        let wait = retry_after(&headers)
                            .unwrap_or_else(|| self.policy.delay_for_attempt(attempt))
                            .min(self.policy.max_delay);
                        tracing::debug!(
                            attempt,
                            status = status.as_u16(),
                            ?wait,
                            "retrying request"
                        );
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    let body = resp.bytes().await?;
                    return Ok(Response {
                        status,
                        headers,
                        body: body.to_vec(),
                    });
                }
                Err(err) => {
                    let retryable = err.is_timeout() || err.is_connect() || err.is_request();
                    if retryable
                        && RetryPolicy::is_idempotent(&method)
                        && attempt < self.policy.max_attempts
                    {
                        let wait = self.policy.delay_for_attempt(attempt);
                        tracing::debug!(attempt, error = %err, ?wait, "retrying after transport error");
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(HttpError::Network(err));
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, HttpError> {
        serde_json::from_slice(&self.body).map_err(|e| {
            HttpError::Decode(format!(
                "{e}; body starts with: {}",
                self.text().chars().take(200).collect::<String>()
            ))
        })
    }

    /// Maps status to the upstream error taxonomy: 401/403 → reauth, 429 → rate limited.
    pub fn error_for_status(&self) -> Option<HttpError> {
        match self.status.as_u16() {
            200..=299 => None,
            401 | 403 => Some(HttpError::Unauthorized(self.status.as_u16())),
            429 => Some(HttpError::RateLimited(retry_after(&self.headers))),
            code => Some(HttpError::Server {
                status: code,
                body: self.text().chars().take(400).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_only_transient_statuses() {
        for code in [408u16, 429, 500, 502, 503, 504] {
            assert!(
                RetryPolicy::should_retry_status(StatusCode::from_u16(code).unwrap()),
                "{code}"
            );
        }
        for code in [200u16, 201, 400, 401, 403, 404, 422, 501] {
            assert!(
                !RetryPolicy::should_retry_status(StatusCode::from_u16(code).unwrap()),
                "{code}"
            );
        }
    }

    #[test]
    fn retries_only_idempotent_methods() {
        assert!(RetryPolicy::is_idempotent(&Method::GET));
        assert!(RetryPolicy::is_idempotent(&Method::HEAD));
        assert!(RetryPolicy::is_idempotent(&Method::OPTIONS));
        assert!(!RetryPolicy::is_idempotent(&Method::POST));
        assert!(!RetryPolicy::is_idempotent(&Method::DELETE));
    }

    #[test]
    fn backoff_grows_then_saturates_at_cap() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for_attempt(1), Duration::from_millis(500));
        assert_eq!(p.delay_for_attempt(2), Duration::from_millis(1000));
        assert_eq!(p.delay_for_attempt(3), Duration::from_millis(2000));
        assert_eq!(p.delay_for_attempt(30), Duration::from_secs(10));
    }

    #[test]
    fn parses_retry_after_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "42".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(42)));
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }
}
