//! Native Muse Code subscription credentials. The device credential never
//! doubles as an inference key, and provider bodies never become errors.
use super::oauth::send_json;
use super::store::text;
use super::{AccountCredential, SubscriptionError, SubscriptionTier};
use serde_json::{Value, json};

pub(crate) const CLIENT: &str = "1031625952748946";
pub(crate) const API: &str = "https://api.meta.ai/v1";
pub(crate) const USER_AGENT: &str =
    "muse-build/1.3.0 (interactive; macos-aarch64; build ac7280f2aca67769d1455a8847bb502b617d50f6)";

/// Exact provider origin; loopback remains available for isolated test servers.
pub(crate) fn valid_api_base(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && ((url.scheme() == "https"
                && url.host_str() == Some("api.meta.ai")
                && url.port().is_none())
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))))
    })
}

pub(crate) fn valid_verification(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("auth.meta.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.fragment().is_none()
    })
}

pub(crate) async fn mint(
    client: &reqwest::Client,
    endpoint: &str,
    dca: &str,
) -> Result<Value, SubscriptionError> {
    let value = send_json(
        client
            .post(endpoint)
            .bearer_auth(dca)
            .header("User-Agent", "muse-code/1.0.2")
            .header("x-api-version", "1.0.0")
            .json(&json!({"dca_token":dca, "onboard":false})),
    )
    .await?;
    if value["is_subs_active"] != true || value["require_payment"] == true {
        return Err(SubscriptionError::InvalidImport(
            "Muse Code requires an active subscription",
        ));
    }
    text(&value, &["api_key"]).ok_or(SubscriptionError::InvalidResponse)?;
    let base = text(&value, &["base_url"]).unwrap_or_else(|| API.into());
    if !valid_api_base(&base) {
        return Err(SubscriptionError::InvalidResponse);
    }
    // A remote mint server can never redirect credentials to a local service.
    let local = reqwest::Url::parse(endpoint).ok().is_some_and(|url| {
        url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
    });
    if !base.starts_with("https://api.meta.ai/") && !local {
        return Err(SubscriptionError::InvalidResponse);
    }
    Ok(value)
}

pub(crate) fn apply_minted(
    account: &mut AccountCredential,
    value: &Value,
) -> Result<(), SubscriptionError> {
    account.access_token = text(value, &["api_key"]).ok_or(SubscriptionError::InvalidResponse)?;
    account.base_url = Some(text(value, &["base_url"]).unwrap_or_else(|| API.into()));
    if let Some(email) = text(value, &["user_email"]) {
        account.email = Some(email);
    }
    account.plan = text(value, &["subs_tier_name"]);
    account.subscription = text(value, &["subs_tier_id"])
        .zip(account.plan.clone())
        .map(|(id, label)| SubscriptionTier {
            id,
            label,
            multiplier: None,
            source: "provider_profile".into(),
        });
    account.expires_at = None;
    account.last_error = None;
    account.cooldown_until = None;
    Ok(())
}

pub(crate) fn prepare_request(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        for key in [
            "generate",
            "prompt_cache_retention",
            "safety_identifier",
            "stream_options",
            "client_metadata",
        ] {
            object.remove(key);
        }
        object.insert("stream".into(), json!(true));
        object.entry("instructions").or_insert(json!(""));
    }
}

pub(crate) fn subscription_event(value: &Value) -> bool {
    value["type"] == "response.subscription_usage"
        || (value.get("type").is_none() && value["subscription"].is_object())
}

pub(crate) fn sanitize_error_event(value: &mut Value) {
    if value.get("error").is_some_and(Value::is_object) {
        value["error"] = json!({"type":"upstream_error", "message":"upstream stream failed"});
    }
    if value
        .pointer("/response/error")
        .is_some_and(Value::is_object)
    {
        value["response"]["error"] =
            json!({"type":"upstream_error", "message":"upstream stream failed"});
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mint_rejects_unsafe_metadata_and_never_copies_upstream_errors() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        for payload in [
            json!({"api_key":"secret-key", "is_subs_active":true,"base_url":"https://evil.test/v1"}),
            json!({"api_key":"secret-key", "is_subs_active":false,"base_url":API}),
            json!({"api_key":"secret-key", "is_subs_active":true,"require_payment":true,"base_url":API}),
            json!({"error":"dca:secret"}),
        ] {
            let response = payload.clone();
            let app = Router::new().route(
                "/mint",
                post(move || {
                    let value = response.clone();
                    async move {
                        let status = if value.get("error").is_some() {
                            StatusCode::UNAUTHORIZED
                        } else {
                            StatusCode::OK
                        };
                        (status, Json(value))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/mint", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let error = mint(&reqwest::Client::new(), &endpoint, "dca:secret")
                .await
                .unwrap_err();
            for text in [error.to_string(), format!("{error:?}")] {
                assert!(
                    !text.contains("dca:secret")
                        && !text.contains("secret-key")
                        && !text.contains("evil.test")
                );
            }
            server.abort();
        }
    }

    #[test]
    fn trusted_origins_and_filters_preserve_native_semantics() {
        assert!(valid_api_base(API));
        for bad in [
            "https://api.meta.ai.evil.test/v1",
            "https://evil.test/v1",
            "http://api.meta.ai/v1",
            "https://token@api.meta.ai/v1",
            "https://api.meta.ai/v1?token=secret",
            "https://api.meta.ai/v1#secret",
        ] {
            assert!(!valid_api_base(bad), "{bad}");
        }
        let mut body = json!({"model":"muse", "prompt_cache_key":"native-cache", "previous_response_id":"resp_1", "tool_choice":"required", "input":[{"type":"reasoning","encrypted_content":"opaque-signature"}], "client_metadata":{},"safety_identifier":"remove"});
        prepare_request(&mut body);
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["prompt_cache_key"], "native-cache");
        assert_eq!(body["previous_response_id"], "resp_1");
        assert_eq!(body["input"][0]["encrypted_content"], "opaque-signature");
        assert!(body.get("client_metadata").is_none() && body.get("safety_identifier").is_none());
        let mut error =
            json!({"type":"response.failed","response":{"error":{"message":"dca:secret"}}});
        sanitize_error_event(&mut error);
        assert!(!error.to_string().contains("dca:secret"));
    }
}
