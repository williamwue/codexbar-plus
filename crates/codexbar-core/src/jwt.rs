//! Unverified JWT payload decoding.
//!
//! Upstream reads identity claims out of the Codex `id_token` without validating the
//! signature (`CodexOAuthCredentials.swift:490-518`, `CodexReconciledState.swift:157+`):
//! the token came from a file only this user can read, and it is used for display only.
//! Never use this for authorization decisions.

use base64::Engine;
use serde_json::Value;

/// Decodes the payload segment of a JWT. Signature is NOT verified.
pub fn decode_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `email` claim, or the nested `https://api.openai.com/profile.email`
/// (upstream `CodexReconciledState.swift:157+`).
pub fn openai_email(payload: &Value) -> Option<String> {
    string_at(payload, &["email"])
        .or_else(|| string_at(payload, &["https://api.openai.com/profile", "email"]))
}

/// `chatgpt_account_id`, the nested auth claim, or the first organization id
/// (upstream `CodexOAuthCredentials.swift:490-518`).
pub fn chatgpt_account_id(payload: &Value) -> Option<String> {
    if let Some(v) = string_at(payload, &["chatgpt_account_id"]) {
        return Some(v);
    }
    if let Some(v) = string_at(
        payload,
        &["https://api.openai.com/auth", "chatgpt_account_id"],
    ) {
        return Some(v);
    }
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("organizations"))
        .and_then(|orgs| orgs.as_array())
        .and_then(|orgs| orgs.first())
        .and_then(|org| org.get("id"))
        .and_then(|id| id.as_str())
        .map(str::to_owned)
}

/// `https://api.openai.com/auth.chatgpt_plan_type` when present.
pub fn chatgpt_plan_type(payload: &Value) -> Option<String> {
    string_at(
        payload,
        &["https://api.openai.com/auth", "chatgpt_plan_type"],
    )
    .or_else(|| string_at(payload, &["chatgpt_plan_type"]))
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn make_token(payload: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn reads_flat_and_nested_identity_claims() {
        let token = make_token(serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-123",
                "chatgpt_plan_type": "plus"
            }
        }));
        let payload = decode_payload(&token).expect("payload decodes");
        assert_eq!(openai_email(&payload).as_deref(), Some("dev@example.com"));
        assert_eq!(chatgpt_account_id(&payload).as_deref(), Some("acct-123"));
        assert_eq!(chatgpt_plan_type(&payload).as_deref(), Some("plus"));
    }

    #[test]
    fn falls_back_to_profile_email_and_first_organization() {
        let token = make_token(serde_json::json!({
            "https://api.openai.com/profile": { "email": "team@example.com" },
            "https://api.openai.com/auth": { "organizations": [{ "id": "org-9" }] }
        }));
        let payload = decode_payload(&token).unwrap();
        assert_eq!(openai_email(&payload).as_deref(), Some("team@example.com"));
        assert_eq!(chatgpt_account_id(&payload).as_deref(), Some("org-9"));
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert!(decode_payload("not-a-jwt").is_none());
        assert!(decode_payload("aGVhZGVy.%%%.sig").is_none());
        // Valid base64 that is not JSON must not decode either.
        let bogus = format!("aGVhZGVy.{}.sig", URL_SAFE_NO_PAD.encode(b"plain text"));
        assert!(decode_payload(&bogus).is_none());
    }
}
