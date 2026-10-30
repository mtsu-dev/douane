use crate::{
    antigravity_replay::{Recorder, Scope},
    ledger::{Call, Ledger},
    state::AppState,
    subscriptions::{AccountCredential, SubscriptionProvider, antigravity_user_agent},
    translate::{self, Bridge, Dialect, Frames},
};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

/// 429 when shares hold the client back, so it waits; 503 otherwise.
pub(crate) fn refusal_status(message: &str) -> StatusCode {
    if message.starts_with(crate::clients::SHARE_USED_UP) {
        StatusCode::TOO_MANY_REQUESTS
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"type":"proxy_error","message":message}})),
    )
        .into_response()
}
/// GitHub Copilot's API. Enterprise accounts carry their own base URL.
pub const COPILOT_API: &str = "https://api.githubcopilot.com";
/// Copilot identifies the integration by the OAuth app and these headers;
/// douane signs in through the OpenCode app and presents itself the same way.
pub const COPILOT_USER_AGENT: &str = "opencode/1.19.5";
pub const COPILOT_API_VERSION: &str = "2026-06-01";
pub(crate) const CODEX_VERSION: &str = "0.166.0";
pub(crate) const CODEX_USER_AGENT: &str = "codex_cli_rs/0.166.0";
pub(crate) const CLAUDE_USER_AGENT: &str = "claude-cli/2.1.312 (external, cli)";
pub(crate) const GEMINI_USER_AGENT: &str = "GeminiCLI/0.66.0";

/// The wire format a custom endpoint was configured with.
fn configured_dialect(value: &str) -> Option<Dialect> {
    match value {
        "chat" => Some(Dialect::Chat),
        "responses" => Some(Dialect::Responses),
        "messages" => Some(Dialect::Messages),
        _ => None,
    }
}

fn copilot_path(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Messages => "/v1/messages",
        Dialect::Responses => "/responses",
        _ => "/chat/completions",
    }
}

/// Copilot serves each model on the paths its catalog lists. The client's
/// own format goes through untouched when the model accepts it; otherwise
/// Messages, then Responses, then Chat Completions.
fn copilot_wire(client: Dialect, endpoints: &[String]) -> Dialect {
    if endpoints.is_empty() {
        return Dialect::Chat;
    }
    let accepts = |dialect: Dialect| endpoints.iter().any(|e| e == copilot_path(dialect));
    if client != Dialect::Gemini && accepts(client) {
        return client;
    }
    [Dialect::Messages, Dialect::Responses, Dialect::Chat]
        .into_iter()
        .find(|dialect| accepts(*dialect))
        .unwrap_or(Dialect::Chat)
}

/// Copilot bills a premium request for each turn its user starts; tool
/// results and other follow-ups are the agent's own and say so.
pub(crate) fn copilot_initiator(body: &Value, wire: Dialect) -> &'static str {
    let user_turn = match wire {
        Dialect::Messages => body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .is_some_and(|last| {
                last["role"] == "user"
                    && (last["content"].is_string()
                        || last["content"].as_array().is_some_and(|parts| {
                            parts.iter().any(|part| part["type"] != "tool_result")
                        }))
            }),
        Dialect::Responses => {
            body["input"].is_string()
                || body["input"]
                    .as_array()
                    .and_then(|items| items.last())
                    .is_some_and(|last| last["role"] == "user")
        }
        _ => body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .is_some_and(|last| last["role"] == "user"),
    };
    if user_turn { "user" } else { "agent" }
}

/// Copilot needs to be told when a request carries an image.
pub(crate) fn copilot_vision(body: &Value, wire: Dialect) -> bool {
    let image = |part: &Value| {
        matches!(
            part["type"].as_str(),
            Some("image" | "image_url" | "input_image")
        ) || (part["type"] == "tool_result"
            && part["content"]
                .as_array()
                .is_some_and(|inner| inner.iter().any(|p| p["type"] == "image")))
    };
    let items = match wire {
        Dialect::Responses => body["input"].as_array(),
        _ => body["messages"].as_array(),
    };
    items.is_some_and(|items| {
        items.iter().any(|item| {
            item["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(image))
        })
    })
}

pub fn endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") && path.starts_with("/v1/") {
        format!("{base}{}", &path[3..])
    } else {
        format!("{base}{path}")
    }
}

pub(crate) fn upstream(
    account: &AccountCredential,
    client: Dialect,
    model: &str,
    endpoints: &[String],
) -> (String, Dialect) {
    use SubscriptionProvider::*;
    let (base, wire, path) = match account.provider {
        Codex => (
            "https://chatgpt.com/backend-api/codex",
            Dialect::Responses,
            "/responses",
        ),
        Claude => (
            "https://api.anthropic.com",
            Dialect::Messages,
            "/v1/messages",
        ),
        Xai => (
            "https://cli-chat-proxy.grok.com/v1",
            Dialect::Chat,
            "/v1/chat/completions",
        ),
        OpencodeGo => {
            // Muse only serves Responses. Translate at the provider boundary
            // even when the caller uses Messages or Chat Completions.
            let wire = if model.starts_with("muse-") {
                Dialect::Responses
            } else {
                Dialect::Chat
            };
            (
                "https://opencode.ai/zen/go",
                wire,
                if wire == Dialect::Responses {
                    "/v1/responses"
                } else {
                    "/v1/chat/completions"
                },
            )
        }
        Kimi => (
            "https://api.kimi.com/coding",
            Dialect::Messages,
            "/v1/messages",
        ),
        Qwen => (
            "https://portal.qwen.ai",
            Dialect::Chat,
            "/v1/chat/completions",
        ),
        Meta => ("https://api.meta.ai", Dialect::Responses, "/v1/responses"),
        Copilot => {
            let wire = copilot_wire(client, endpoints);
            (COPILOT_API, wire, copilot_path(wire))
        }
        OpenaiCompatible => {
            let wire = account
                .dialect
                .as_deref()
                .and_then(configured_dialect)
                .unwrap_or(client);
            (
                "https://api.openai.com",
                wire,
                match wire {
                    Dialect::Responses => "/v1/responses",
                    Dialect::Messages => "/v1/messages",
                    _ => "/v1/chat/completions",
                },
            )
        }
        Gemini => (
            "https://cloudcode-pa.googleapis.com",
            Dialect::Gemini,
            "/v1internal:generateContent",
        ),
        Antigravity => (
            "https://daily-cloudcode-pa.googleapis.com",
            Dialect::Gemini,
            "/v1internal:generateContent",
        ),
    };
    (
        endpoint(account.base_url.as_deref().unwrap_or(base), path),
        wire,
    )
}

pub(crate) fn upstream_headers(account: &AccountCredential, incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in incoming {
        if !matches!(
            name.as_str(),
            "host"
                | "authorization"
                | "x-api-key"
                | "x-goog-api-key"
                | "api-key"
                | "x-douane-admin"
                | "x-management-key"
                | "x-douane-provider"
                | "proxy-authorization"
                | "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "content-length"
                | "transfer-encoding"
                | "accept-encoding"
                | "cookie"
                | "origin"
                | "referer"
        ) && !name.as_str().starts_with("sec-websocket-")
        {
            out.insert(name.clone(), value.clone());
        }
    }
    out.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    if let Ok(token) = format!("Bearer {}", account.access_token).parse() {
        out.insert(header::AUTHORIZATION, token);
    }
    match account.provider {
        SubscriptionProvider::Claude | SubscriptionProvider::Kimi => {
            out.entry("anthropic-version")
                .or_insert(header::HeaderValue::from_static("2023-06-01"));
            if account.provider == SubscriptionProvider::Claude {
                let original = out
                    .get("anthropic-beta")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let beta = if original.split(',').any(|s| s.trim() == "oauth-2025-04-20") {
                    original.to_owned()
                } else {
                    format!("oauth-2025-04-20,claude-code-20250219,{original}")
                        .trim_end_matches(',')
                        .to_owned()
                };
                if let Ok(value) = beta.parse() {
                    out.insert("anthropic-beta", value);
                }
                out.entry("user-agent")
                    .or_insert(header::HeaderValue::from_static(CLAUDE_USER_AGENT));
                out.entry("x-app")
                    .or_insert(header::HeaderValue::from_static("cli"));
            }
        }
        SubscriptionProvider::Codex => {
            if let Some(id) = &account.account_id {
                if let Ok(value) = id.parse() {
                    out.insert("chatgpt-account-id", value);
                }
            }
            out.entry("originator")
                .or_insert(header::HeaderValue::from_static("codex_cli_rs"));
            out.entry("user-agent")
                .or_insert(header::HeaderValue::from_static(CODEX_USER_AGENT));
        }
        SubscriptionProvider::Xai => {
            out.insert(
                "x-xai-token-auth",
                header::HeaderValue::from_static("xai-grok-cli"),
            );
            out.insert(
                "x-grok-client-version",
                header::HeaderValue::from_static("1.0.44"),
            );
            out.insert(
                "user-agent",
                header::HeaderValue::from_static(
                    "grok-pager/1.0.44 grok-shell/1.0.44 (linux; x86_64)",
                ),
            );
            if let Some(id) = &account.account_id {
                if let Ok(v) = id.parse() {
                    out.insert("x-userid", v);
                }
            }
        }
        SubscriptionProvider::OpencodeGo => {
            out.entry("user-agent")
                .or_insert(header::HeaderValue::from_static(concat!(
                    "douane/",
                    env!("CARGO_PKG_VERSION")
                )));
            if !out.contains_key("x-opencode-session") {
                if let Some(value) = ["session_id", "session-id", "thread-id"]
                    .iter()
                    .find_map(|h| incoming.get(*h))
                {
                    out.insert("x-opencode-session", value.clone());
                }
            }
        }
        SubscriptionProvider::Meta => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(crate::subscriptions::meta::USER_AGENT),
            );
            out.insert("x-client-id", header::HeaderValue::from_static("tbh:tui"));
            out.insert(
                header::ACCEPT,
                header::HeaderValue::from_static("text/event-stream"),
            );
            out.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-cache"),
            );
        }
        SubscriptionProvider::Copilot => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(COPILOT_USER_AGENT),
            );
            out.insert(
                "x-github-api-version",
                header::HeaderValue::from_static(COPILOT_API_VERSION),
            );
            out.insert(
                "openai-intent",
                header::HeaderValue::from_static("conversation-edits"),
            );
        }
        SubscriptionProvider::Gemini => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(GEMINI_USER_AGENT),
            );
        }
        SubscriptionProvider::Antigravity => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(antigravity_user_agent()),
            );
            out.insert(
                "x-goog-api-client",
                header::HeaderValue::from_static("gl-node/22.21.1"),
            );
        }
        _ => {}
    }
    out
}

fn replay_session(session: &str, history: &Value) -> String {
    let system = serde_json::to_vec(&history["systemInstruction"]).unwrap_or_default();
    format!("{session}:{}", hex::encode(Sha256::digest(system)))
}

fn replay_rate_limit(seconds: u64) -> Response {
    let mut response = error(
        StatusCode::TOO_MANY_REQUESTS,
        "the recorded Antigravity account is temporarily unavailable; retry this conversation after Retry-After",
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        seconds.max(1).to_string().parse().unwrap(),
    );
    response
}

/// Account selection may rotate new sessions, but native history cannot move
/// between subscriptions. Lookup does not authorize or refresh the owner.
async fn replay_owner(
    state: &AppState,
    body: &Value,
    headers: &HeaderMap,
    from: Dialect,
) -> Result<Option<String>, String> {
    let Some(session) = crate::affinity::session_key(headers, body) else {
        return Ok(None);
    };
    let Some(requested) = body["model"].as_str() else {
        return Ok(None);
    };
    let model = if let Some(model) = requested.strip_prefix("antigravity/") {
        model
    } else {
        if let Some((prefix, _)) = requested.split_once('/') {
            if SubscriptionProvider::parse(prefix).is_some()
                || state.accounts.routes().await.contains(prefix)
            {
                return Ok(None);
            }
        }
        let config = state.config.read().await;
        let provider = headers
            .get("x-douane-provider")
            .and_then(|v| v.to_str().ok())
            .or(config.settings.default_provider.as_deref());
        if provider.is_some_and(|provider| {
            SubscriptionProvider::parse(provider) != Some(SubscriptionProvider::Antigravity)
        }) {
            return Ok(None);
        }
        requested
    };
    if !state
        .antigravity_replay
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .has_session(model, &session)
    {
        return Ok(None);
    }
    let mut history = translate::antigravity_history(body, from)?;
    crate::antigravity_system::normalize_system(&mut history);
    let session = replay_session(&session, &history);
    state
        .antigravity_replay
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owner(model, &session, &history)
}

pub(crate) async fn choose(
    state: &AppState,
    body: &Value,
    headers: &HeaderMap,
    client: Option<&crate::clients::Client>,
) -> Result<(String, Vec<AccountCredential>, Option<String>), String> {
    choose_for_replay(state, body, headers, client, None).await
}

async fn choose_for_replay(
    state: &AppState,
    body: &Value,
    headers: &HeaderMap,
    client: Option<&crate::clients::Client>,
    owner: Option<&str>,
) -> Result<(String, Vec<AccountCredential>, Option<String>), String> {
    let model = body["model"]
        .as_str()
        .filter(|m| !m.is_empty())
        .ok_or("model is required")?;
    let session = crate::affinity::session_key(headers, body);
    let (upstream, accounts) = state
        .candidates_for_replay(
            model,
            headers
                .get("x-douane-provider")
                .and_then(|v| v.to_str().ok()),
            session.as_deref(),
            client,
            owner,
        )
        .await?;
    Ok((upstream, accounts, session))
}

pub async fn forward(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    mut headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client = match state.clients.authenticate(&headers) {
        Ok(client) => Some(client),
        Err(refusal) => return error(StatusCode::UNAUTHORIZED, refusal.message()),
    };
    let count_tokens = uri.path().ends_with("/messages/count_tokens");
    let from = if uri.path().ends_with("/messages") || count_tokens {
        Dialect::Messages
    } else if uri.path().ends_with("/chat/completions") {
        Dialect::Chat
    } else {
        Dialect::Responses
    };
    let mut value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "body must be valid JSON"),
    };
    if uri.path().starts_with("/muse-code/") {
        // Native Muse uses bare model ids. Pin this endpoint to its own
        // subscription even when another provider serves the same model.
        headers.insert("x-douane-provider", "meta".parse().unwrap());
        if value["model"]
            .as_str()
            .is_some_and(|model| model.contains('/') && !model.starts_with("meta/"))
        {
            return error(StatusCode::BAD_REQUEST, "Muse Code requires a Meta model");
        }
    }
    let stream = value["stream"].as_bool().unwrap_or(false);
    let owner = match replay_owner(&state, &value, &headers, from).await {
        Ok(owner) => owner,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let (model, candidates, session) = match choose_for_replay(
        &state,
        &value,
        &headers,
        client.as_ref(),
        owner.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return error(refusal_status(&e), &e),
    };
    value["model"] = json!(model);
    // The Grok CLI repeats its model in a header, which gets the provider's id
    // as the body does: xAI does not know the gateway's prefix.
    let mut headers = headers;
    if headers.contains_key("x-grok-model-override") {
        if let Ok(named) = model.parse() {
            headers.insert("x-grok-model-override", named);
        }
    }
    let mut last_status = StatusCode::BAD_GATEWAY;
    // The last native refusal, returned as is when every account refused.
    let mut last_native: Option<Response> = None;
    // Google's reason for the last refusal, so a client sees more than a status.
    let mut last_reason: Option<String> = None;
    for candidate in candidates {
        if owner.is_some() {
            if let Some(until) = candidate
                .cooldown_until
                .as_deref()
                .and_then(crate::subscriptions::parse_timestamp)
            {
                let remaining = until - time::OffsetDateTime::now_utc();
                if remaining.is_positive() {
                    return replay_rate_limit(remaining.whole_seconds() as u64 + 1);
                }
            }
        }
        let mut account = match state.oauth.credential(&candidate.id).await {
            Ok(a) => a,
            Err(_) => {
                let _ = state
                    .accounts
                    .cooldown(&candidate.id, 300, "authentication needs reconnecting")
                    .await;
                continue;
            }
        };
        let endpoints: Vec<String> = if account.provider == SubscriptionProvider::Copilot {
            state
                .models
                .read()
                .await
                .iter()
                .find(|m| m.provider == account.provider && m.upstream_id == model)
                .map(|m| m.endpoints.clone())
                .unwrap_or_default()
        } else {
            vec![]
        };
        let (mut url, to) = upstream(&account, from, &model, &endpoints);
        if uri.path().ends_with("/responses/compact") {
            if to != Dialect::Responses {
                return error(
                    StatusCode::BAD_REQUEST,
                    "this provider does not offer native compaction",
                );
            }
            url.push_str("/compact");
        }
        if count_tokens {
            if account.provider != SubscriptionProvider::Claude {
                return error(
                    StatusCode::NOT_FOUND,
                    "token counting is offered only by Claude accounts",
                );
            }
            url.push_str("/count_tokens");
        }
        let custom = translate::custom_tools(&value, from);
        let mut translated = match translate::request(&value, from, to) {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_REQUEST, &e),
        };
        let mut replay = if account.provider == SubscriptionProvider::Antigravity {
            crate::antigravity_system::normalize_system(&mut translated);
            let mut history = match translate::antigravity_history(&value, from) {
                Ok(history) => history,
                Err(e) => return error(StatusCode::BAD_REQUEST, &e),
            };
            crate::antigravity_system::normalize_system(&mut history);
            let (capabilities, output_limit) = state
                .models
                .read()
                .await
                .iter()
                .find(|m| m.provider == account.provider && m.upstream_id == model)
                .map(|m| (m.thinking.clone(), m.max_output_tokens))
                .unwrap_or_default();
            if translated
                .pointer("/generationConfig/maxOutputTokens")
                .is_none()
            {
                if let Some(limit) = output_limit.filter(|limit| *limit > 0) {
                    translated["generationConfig"]["maxOutputTokens"] = json!(limit);
                }
            }
            if let Err(e) = crate::antigravity_thinking::configure(
                &mut translated,
                &value,
                from,
                &model,
                capabilities.as_ref(),
            ) {
                return error(StatusCode::BAD_REQUEST, &e);
            }
            let scope = Scope {
                account: account.id.clone(),
                model: model.clone(),
                // Bind the root instructions to the session. Inline instructions
                // remain client-owned context and may change during compaction.
                session: session
                    .as_ref()
                    .map(|session| replay_session(session, &history))
                    .unwrap_or_default(),
            };
            // Future context identity follows the client projection, whose
            // tool IDs differ from the original provider IDs restored below.
            let client_contents = history["contents"].as_array().cloned().unwrap_or_default();
            let started = match state
                .antigravity_replay
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .begin(&scope, &mut history)
            {
                Ok(started) => started,
                Err(e) => return error(StatusCode::BAD_REQUEST, &e),
            };
            let native_replay = NativeReplay {
                state: state.clone(),
                scope: scope.clone(),
                pinned: started.pinned,
                recorder: Some(Recorder::new(scope, client_contents).with_parent(started.parent)),
                terminal_output: None,
            };
            // Only model parts cross from the chronological cache projection
            // into the provider request. Instructions retain their system role.
            let mut restored_models = history["contents"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|content| content["role"] == "model");
            if let Some(contents) = translated["contents"].as_array_mut() {
                for content in contents
                    .iter_mut()
                    .filter(|content| content["role"] == "model")
                {
                    let Some(restored) = restored_models.next() else {
                        return error(
                            StatusCode::BAD_REQUEST,
                            "signed-history model projection does not match native contents",
                        );
                    };
                    content["parts"] = restored["parts"].clone();
                }
            }
            if restored_models.next().is_some() {
                return error(
                    StatusCode::BAD_REQUEST,
                    "signed-history model projection does not match native contents",
                );
            }
            // Only tool-result IDs cross from the restored user projection;
            // preserve every client result value, text and instruction.
            let mut restored_responses = history["contents"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|content| content["parts"].as_array())
                .flatten()
                .filter_map(|part| part.get("functionResponse"));
            if let Some(contents) = translated["contents"].as_array_mut() {
                for response in contents
                    .iter_mut()
                    .filter_map(|content| content["parts"].as_array_mut())
                    .flatten()
                    .filter_map(|part| part.get_mut("functionResponse"))
                {
                    let Some(restored) = restored_responses.next() else {
                        return error(
                            StatusCode::BAD_REQUEST,
                            "signed-history tool-result projection does not match native contents",
                        );
                    };
                    if let Some(object) = response.as_object_mut() {
                        if let Some(id) = restored.get("id") {
                            object.insert("id".into(), id.clone());
                        } else {
                            object.remove("id");
                        }
                    }
                }
            }
            if restored_responses.next().is_some() {
                return error(
                    StatusCode::BAD_REQUEST,
                    "signed-history tool-result projection does not match native contents",
                );
            }
            Some(native_replay)
        } else {
            None
        };
        if account.provider == SubscriptionProvider::Meta {
            crate::subscriptions::meta::prepare_request(&mut translated);
        }
        let codex_sse =
            account.provider == SubscriptionProvider::Codex && !uri.path().ends_with("/compact");
        // Same dialect: the agent talks to its own provider and reads its
        // answers, limits and refusals exactly as a direct subscription would.
        let native = from == to;
        if codex_sse {
            translated["stream"] = json!(true);
            translated["store"] = json!(false);
            if translated.get("instructions").is_none() {
                translated["instructions"] = json!("");
            }
            // Codex's backend refuses a plain text input: "Input must be a list".
            if let Some(text) = translated["input"].as_str() {
                translated["input"] =
                    json!([{"role":"user","content":[{"type":"input_text","text":text}]}]);
            }
            for key in ["max_output_tokens", "max_tokens", "temperature", "top_p"] {
                translated.as_object_mut().map(|m| m.remove(key));
            }
            if translated.get("prompt_cache_key").is_none() {
                if let Some(key) = &session {
                    translated["prompt_cache_key"] = json!(key);
                }
            }
        }
        if to == Dialect::Gemini {
            if stream {
                url = url.replace(":generateContent", ":streamGenerateContent?alt=sse");
            }
            translated = json!({"model":model,"project":account.project_id,"request":translated});
            if account.provider == SubscriptionProvider::Antigravity {
                translated["userAgent"] = json!("antigravity");
                translated["requestType"] = json!("agent");
                translated["requestId"] = json!(format!("douane-{:016x}", rand::random::<u64>()));
            }
        }
        // Headers that depend on the request itself, not on the account.
        let mut request_headers = HeaderMap::new();
        if account.provider == SubscriptionProvider::Copilot {
            request_headers.insert(
                "x-initiator",
                header::HeaderValue::from_static(copilot_initiator(&translated, to)),
            );
            if copilot_vision(&translated, to) {
                request_headers.insert(
                    "copilot-vision-request",
                    header::HeaderValue::from_static("true"),
                );
            }
            if let Some(id) = session.as_deref().and_then(|s| s.parse().ok()) {
                request_headers.insert("x-interaction-id", id);
            }
            if to == Dialect::Messages {
                request_headers.insert(
                    "anthropic-version",
                    headers
                        .get("anthropic-version")
                        .cloned()
                        .unwrap_or(header::HeaderValue::from_static("2023-06-01")),
                );
            }
        }
        let bytes = match serde_json::to_vec(&translated) {
            Ok(v) => Bytes::from(v),
            Err(_) => return error(StatusCode::BAD_REQUEST, "cannot serialize request"),
        };
        let mut call = CallGuard::new(state.ledger.clone(), &account, &model)
            .for_client(&state.clients, client.as_ref());
        let mut retry_auth = true;
        // The setting bounds each wait on the upstream, never the whole answer:
        // a long generation keeps streaming as long as its events keep coming.
        let idle = Duration::from_secs(state.config.read().await.settings.request_timeout_seconds);
        let response = loop {
            let send = state
                .client
                .post(&url)
                .headers({
                    let mut sent = upstream_headers(&account, &headers);
                    sent.extend(request_headers.clone());
                    sent
                })
                .body(bytes.clone())
                .send();
            let result = match tokio::time::timeout(idle, send).await {
                Ok(sent) => sent.map_err(|_| "upstream connection failed"),
                Err(_) => Err("upstream did not answer in time"),
            };
            match result {
                Ok(r) if r.status() == StatusCode::UNAUTHORIZED && retry_auth => {
                    retry_auth = false;
                    match state.oauth.force_refresh(&account.id).await {
                        Ok(fresh) => account = fresh,
                        Err(_) => break Ok(r),
                    }
                }
                result => break result,
            }
        };
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                call.fail(e);
                let _ = state.accounts.cooldown(&account.id, 10, e).await;
                continue;
            }
        };
        last_status = response.status();
        call.call.status = last_status.as_u16();
        state
            .quotas
            .observe_claude_headers(&account, response.headers())
            .await;
        state.observe_usage(&account.id).await;
        if last_status.is_success() {
            state.accounts.clear_stale_error(&account.id).await;
        }
        let passed = if native {
            passthrough_headers(response.headers())
        } else {
            HeaderMap::new()
        };
        if !last_status.is_success() {
            let advised = cooldown_seconds(response.headers(), unix_now());
            // Copilot refuses a request it cannot serve with 400; the account
            // is fine, and a model its plan lacks leaves the list.
            if account.provider == SubscriptionProvider::Copilot
                && last_status == StatusCode::BAD_REQUEST
            {
                let body = read_bounded(response, 64 * 1024, idle)
                    .await
                    .unwrap_or_default();
                let refusal: Option<Value> = serde_json::from_slice(&body).ok();
                let code = refusal
                    .as_ref()
                    .and_then(|v| v.pointer("/error/code"))
                    .and_then(Value::as_str);
                if code == Some("model_not_supported") {
                    const UNSERVED: &str =
                        "GitHub Copilot does not serve this model on this account's plan";
                    call.fail(UNSERVED);
                    state
                        .model_refused(account.provider, &account.id, &model)
                        .await;
                    last_reason = Some(UNSERVED.into());
                    continue;
                }
                call.fail("upstream request refused");
                if native && refusal.is_some() {
                    let mut answer = (last_status, Body::from(body)).into_response();
                    answer.headers_mut().extend(passed);
                    answer.headers_mut().insert(
                        header::CONTENT_TYPE,
                        header::HeaderValue::from_static("application/json"),
                    );
                    return answer;
                }
                let message: Option<String> = refusal
                    .as_ref()
                    .and_then(|v| v.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .map(|m| m.chars().take(300).collect());
                return error(
                    last_status,
                    &message.unwrap_or_else(|| "upstream request refused".into()),
                );
            }
            let (response, refusal) = if to == Dialect::Gemini && !native {
                let body = read_bounded(response, 64 * 1024, idle)
                    .await
                    .unwrap_or_default();
                (None, google_refusal(&body))
            } else {
                (Some(response), None)
            };
            // An Antigravity plan answers NOT_FOUND for a model it lacks
            // (the newest Claude on a trial or partner plan) while another
            // account of the pool may serve it: the account itself is fine.
            if account.provider == SubscriptionProvider::Antigravity
                && last_status == StatusCode::NOT_FOUND
            {
                const UNSERVED: &str = "this Antigravity account does not serve this model";
                call.fail(UNSERVED);
                state
                    .model_refused(account.provider, &account.id, &model)
                    .await;
                last_reason = Some(UNSERVED.into());
                continue;
            }
            let summary = match last_status {
                StatusCode::TOO_MANY_REQUESTS => "upstream quota or rate limit reached",
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    "upstream authentication refused"
                }
                _ => "upstream request refused",
            };
            match &refusal {
                Some(refusal) => {
                    call.fail(&format!("{summary} ({})", refusal.reason));
                    last_reason = Some(refusal.reason.clone());
                }
                None => call.fail(summary),
            }
            if last_status == StatusCode::TOO_MANY_REQUESTS || last_status.is_server_error() {
                let wait = advised
                    .or_else(|| refusal.as_ref().and_then(|r| r.retry_seconds))
                    .unwrap_or(30);
                let _ = state
                    .accounts
                    .cooldown(&account.id, wait, "upstream temporarily unavailable")
                    .await;
                if owner.is_some() && last_status == StatusCode::TOO_MANY_REQUESTS {
                    return replay_rate_limit(wait);
                }
                if let (true, Some(response)) = (
                    native && account.provider != SubscriptionProvider::Meta,
                    response,
                ) {
                    last_native = native_refusal(last_status, passed, response, idle).await;
                }
                continue;
            }
            if matches!(
                last_status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                let _ = state
                    .accounts
                    .cooldown(&account.id, 300, "authentication needs reconnecting")
                    .await;
                continue;
            }
            // A native refusal of the request itself (too long, invalid) is the
            // agent's to read and act on, compaction included; the account is fine.
            if account.provider == SubscriptionProvider::Meta {
                return error(last_status, summary);
            }
            if let (true, Some(response)) = (native, response) {
                if let Some(refusal) = native_refusal(last_status, passed, response, idle).await {
                    return refusal;
                }
            } else {
                let _ = state
                    .accounts
                    .mark_error(&account.id, Some("upstream request refused".into()))
                    .await;
            }
            return error(
                last_status,
                call.call
                    .error
                    .as_deref()
                    .unwrap_or("upstream request refused"),
            );
        }
        if let Some(key) = &session {
            state
                .affinity
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bind(account.pool(), key, &account.id);
        }
        // Codex was asked for a stream above, and its backend can answer one
        // without a content type.
        let is_sse = codex_sse
            || response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("text/event-stream"));
        if stream && is_sse {
            let mut streamed = stream_response(
                response,
                from,
                to,
                model,
                call,
                idle,
                custom,
                replay,
                state.clone(),
            );
            streamed.headers_mut().extend(passed);
            return streamed;
        }
        let result = if is_sse {
            collect_response(response, to, &mut call, idle, &mut replay, &state).await
        } else {
            match read_bounded(response, 32 * 1024 * 1024, idle).await {
                Ok(b) => serde_json::from_slice::<Value>(&b)
                    .map_err(|_| "upstream returned invalid JSON".into()),
                Err(e) => Err(e),
            }
        };
        let mut result = match result {
            Ok(v) => v,
            Err(e) => {
                call.fail(&e);
                return error(StatusCode::BAD_GATEWAY, &e);
            }
        };
        if !is_sse {
            if let Some(replay) = &mut replay {
                replay.observe(&mut result);
            }
        }
        if to == Dialect::Gemini {
            if let Some(message) = translate::gemini_refusal(&result) {
                call.fail(message);
                return error(StatusCode::BAD_GATEWAY, message);
            }
        }
        if to == Dialect::Gemini && !gemini_finished(&result) {
            call.fail("upstream response did not contain a Gemini completion");
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream response did not contain a Gemini completion",
            );
        }
        call.observe(&result);
        if call.call.error.is_some() {
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream reported an error in its response",
            );
        }
        let result = match translate::response(&result, to, from, &model, &custom) {
            Ok(v) => v,
            Err(e) => {
                call.fail(&e);
                return error(StatusCode::BAD_GATEWAY, &e);
            }
        };
        if let Some(replay) = &mut replay {
            if let Err(e) = replay.commit() {
                call.fail(&e);
                return error(StatusCode::BAD_GATEWAY, &e);
            }
        }
        call.complete = true;
        let mut answer = Json(result).into_response();
        answer.headers_mut().extend(passed);
        return answer;
    }
    if let Some(refusal) = last_native {
        return refusal;
    }
    match last_reason {
        Some(reason) => error(
            last_status,
            &format!(
                "all accounts for this provider are unavailable; last upstream refusal: {reason}"
            ),
        ),
        None => error(
            last_status,
            "all accounts for this provider are unavailable; check their quotas and connection status",
        ),
    }
}

/// Keep opaque native parts in bounded memory only after a clean completion.
struct NativeReplay {
    state: Arc<AppState>,
    scope: Scope,
    pinned: bool,
    recorder: Option<Recorder>,
    terminal_output: Option<String>,
}
impl NativeReplay {
    fn observe(&mut self, event: &mut Value) {
        if let Some(recorder) = &mut self.recorder {
            recorder.observe(event);
        }
    }
    fn commit(&mut self) -> Result<(), String> {
        self.recorder
            .as_ref()
            .ok_or("native Gemini history recording was already finalized")?
            .validate_completion()
            .map_err(|failure| failure.to_string())?;
        // Anonymous first calls retain their existing behavior; subsequent
        // unsigned history requires a stable session and fails at restore.
        if self.scope.session.trim().is_empty() {
            self.recorder.take();
            return Ok(());
        }
        let turn = self
            .recorder
            .take()
            .ok_or("native Gemini history recording was already finalized")?
            .finish()
            .map_err(|failure| failure.to_string())?;
        // A completion the cache cannot keep is still delivered: the next
        // request replays this turn from the client's projection instead.
        if !self
            .state
            .antigravity_replay
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(turn)
        {
            tracing::warn!(
                model = %self.scope.model,
                "signed Gemini history cache is full; this turn will replay without its native parts"
            );
        }
        Ok(())
    }
}
impl Drop for NativeReplay {
    fn drop(&mut self) {
        if !self.pinned {
            return;
        }
        self.state
            .antigravity_replay
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release(&self.scope);
    }
}

fn gemini_finished(event: &Value) -> bool {
    let body = event.get("response").unwrap_or(event);
    body.pointer("/candidates/0/finishReason")
        .and_then(Value::as_str)
        .is_some_and(|reason| !reason.is_empty() && reason != "FINISH_REASON_UNSPECIFIED")
}

/// A Google (Gemini, Antigravity) refusal reduced to its enum fields. The
/// free-text message is dropped: it can name the project or the account.
#[derive(Debug, PartialEq)]
struct GoogleRefusal {
    /// `error.status`, then the first `details[].reason`, such as
    /// `RESOURCE_EXHAUSTED/MODEL_CAPACITY_EXHAUSTED`.
    reason: String,
    /// The delay Google advises (`RetryInfo.retryDelay` or `quotaResetDelay`).
    retry_seconds: Option<u64>,
}

fn google_refusal(body: &[u8]) -> Option<GoogleRefusal> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error")?;
    let token = |v: &Value| {
        v.as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            })
            .map(str::to_owned)
    };
    let details = error["details"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut parts: Vec<String> = token(&error["status"]).into_iter().collect();
    parts.extend(details.iter().find_map(|d| token(&d["reason"])));
    if parts.is_empty() {
        return None;
    }
    let retry_seconds = details.iter().find_map(|d| {
        d["retryDelay"]
            .as_str()
            .or_else(|| {
                d.pointer("/metadata/quotaResetDelay")
                    .and_then(Value::as_str)
            })
            .and_then(google_duration)
    });
    Some(GoogleRefusal {
        reason: parts.join("/"),
        retry_seconds,
    })
}

/// Seconds in a Google duration: `2.5s`, `1h2m3.4s`, `750ms`; rounded up.
fn google_duration(text: &str) -> Option<u64> {
    let mut total = 0.0;
    let mut number = String::new();
    let mut chars = text.trim().chars().peekable();
    let mut seen = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let value: f64 = number.parse().ok()?;
        number.clear();
        let unit = match (c, chars.peek()) {
            ('m', Some('s')) => {
                chars.next();
                0.001
            }
            ('h', _) => 3600.0,
            ('m', _) => 60.0,
            ('s', _) => 1.0,
            _ => return None,
        };
        total += value * unit;
        seen = true;
    }
    (seen && number.is_empty() && total.is_finite()).then(|| total.ceil() as u64)
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How long a refusing account rests, from what its provider says: an
/// explicit `retry-after`, else the reset of the exhausted subscription
/// window. Retrying an exhausted window every 30 seconds only adds refusals
/// to the account's record.
pub(crate) fn cooldown_seconds(headers: &HeaderMap, now: u64) -> Option<u64> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    if let Some(seconds) = number("retry-after") {
        return Some(seconds.ceil() as u64);
    }
    if headers
        .get("anthropic-ratelimit-unified-status")
        .and_then(|v| v.to_str().ok())
        == Some("rejected")
        && let Some(reset) = number("anthropic-ratelimit-unified-reset")
    {
        return Some((reset as u64).saturating_sub(now).max(1));
    }
    ["primary", "secondary"].iter().find_map(|window| {
        let used = number(&format!("x-codex-{window}-used-percent"))?;
        (used >= 100.0)
            .then(|| number(&format!("x-codex-{window}-reset-after-seconds")))
            .flatten()
            .map(|seconds| seconds.ceil() as u64)
    })
}

/// Response headers an agent reads from its own provider: rate-limit state
/// (Claude's unified limits drive its fast-mode fallback and usage warnings,
/// Codex's drive its status line) and request ids. No cookie, no credential.
pub(crate) fn passthrough_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream {
        let name_text = name.as_str();
        if name_text.starts_with("anthropic-ratelimit-")
            || name_text.starts_with("x-codex-")
            || name_text.starts_with("x-ratelimit-")
            || matches!(name_text, "request-id" | "x-request-id" | "retry-after")
        {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// The provider's own JSON refusal with its status and limit headers, or
/// nothing when its body is not a bounded JSON document.
async fn native_refusal(
    status: StatusCode,
    headers: HeaderMap,
    response: reqwest::Response,
    idle: Duration,
) -> Option<Response> {
    let body = read_bounded(response, 256 * 1024, idle).await.ok()?;
    serde_json::from_slice::<Value>(&body).ok()?;
    let mut refusal = (status, Body::from(body)).into_response();
    refusal.headers_mut().extend(headers);
    refusal.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Some(refusal)
}

/// The upstream went quiet for longer than the request timeout.
const STALLED: &str = "upstream stream stalled";

/// The next piece of an upstream body. Silence longer than `idle` ends the
/// body as `STALLED`; a transport failure ends it as `interrupted`.
async fn next_chunk<S>(
    body: &mut S,
    idle: Duration,
    interrupted: &str,
) -> Option<Result<Bytes, String>>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    match tokio::time::timeout(idle, body.next()).await {
        Ok(Some(Ok(chunk))) => Some(Ok(chunk)),
        Ok(Some(Err(_))) => Some(Err(interrupted.into())),
        Ok(None) => None,
        Err(_) => Some(Err(STALLED.into())),
    }
}

pub(crate) async fn read_bounded(
    response: reqwest::Response,
    cap: usize,
    idle: Duration,
) -> Result<Vec<u8>, String> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = next_chunk(&mut stream, idle, "upstream body interrupted").await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > cap {
            return Err("upstream response exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Subscription telemetry can follow a completed Muse answer. Finish the client
/// promptly, then read only the existing response for its private quota event.
fn meta_tail<S>(mut upstream: S, mut frames: Frames, state: Arc<AppState>, account: String)
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(5), async move {
            loop {
                let chunk = upstream.next().await;
                let eof = chunk.is_none();
                let events = match chunk {
                    Some(Ok(chunk)) => frames.feed(&chunk),
                    Some(Err(_)) => return,
                    None => frames.finish(),
                };
                let Ok(events) = events else {
                    return;
                };
                for event in events {
                    if crate::subscriptions::meta::subscription_event(&event) {
                        state.quotas.observe_meta_event(&account, &event).await;
                        state.observe_usage(&account).await;
                        return;
                    }
                }
                if eof {
                    return;
                }
                // A finished decoder has no further data; the timeout bounds
                // a provider keeping its transport open without telemetry.
            }
        })
        .await;
    });
}

async fn collect_response(
    response: reqwest::Response,
    to: Dialect,
    call: &mut CallGuard,
    idle: Duration,
    replay: &mut Option<NativeReplay>,
    quota_state: &Arc<AppState>,
) -> Result<Value, String> {
    let mut decoder = Frames::default();
    let mut upstream = response.bytes_stream();
    let mut collected = Collected::default();
    let mut meta_complete = false;
    while let Some(chunk) = next_chunk(&mut upstream, idle, "upstream stream interrupted").await {
        for mut frame in decoder.feed(&chunk?)? {
            if call.call.provider == "meta" {
                quota_state
                    .quotas
                    .observe_meta_event(&call.call.account_id, &frame)
                    .await;
                quota_state.observe_usage(&call.call.account_id).await;
            }
            if let Some(replay) = replay {
                replay.observe(&mut frame);
            }
            collected.frame(&frame, to, call)?;
        }
        if call.call.provider == "meta" && collected.result.is_some() {
            meta_complete = true;
            break;
        }
    }
    let tail = if meta_complete {
        meta_tail(
            upstream,
            decoder,
            quota_state.clone(),
            call.call.account_id.clone(),
        );
        Vec::new()
    } else {
        decoder.finish()?
    };
    for mut frame in tail {
        if call.call.provider == "meta" {
            quota_state
                .quotas
                .observe_meta_event(&call.call.account_id, &frame)
                .await;
            quota_state.observe_usage(&call.call.account_id).await;
        }
        if let Some(replay) = replay {
            replay.observe(&mut frame);
        }
        collected.frame(&frame, to, call)?;
    }
    let mut result = collected
        .result
        .ok_or("upstream stream closed before completing its response")?;
    // Codex sends its items as `response.output_item.done` and completes with
    // an empty `output`; the finished items are the answer.
    let empty = result["output"].as_array().is_none_or(Vec::is_empty);
    if empty && !collected.items.is_empty() {
        collected.items.sort_by_key(|(index, _)| *index);
        result["output"] =
            Value::Array(collected.items.into_iter().map(|(_, item)| item).collect());
    }
    Ok(result)
}

#[derive(Default)]
struct Collected {
    result: Option<Value>,
    items: Vec<(u64, Value)>,
    gemini: Option<Value>,
    gemini_bytes: usize,
}

impl Collected {
    fn frame(&mut self, frame: &Value, to: Dialect, call: &mut CallGuard) -> Result<(), String> {
        call.observe(frame);
        if frame["type"] == "response.failed" || frame["type"] == "error" {
            return Err("upstream reported a stream error".into());
        }
        if to == Dialect::Gemini {
            let body = frame.get("response").unwrap_or(frame);
            if let Some(message) = translate::gemini_refusal(body) {
                return Err(message.into());
            }
            if body.get("error").is_some() {
                return Err("upstream reported a stream error".into());
            }
            let result = self.gemini.get_or_insert_with(
                || json!({"candidates":[{"content":{"role":"model","parts":[]}}]}),
            );
            if let Some(parts) = body
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
            {
                self.gemini_bytes += serde_json::to_vec(parts)
                    .map_err(|_| "invalid native Gemini parts")?
                    .len();
                if self.gemini_bytes > 32 * 1024 * 1024 {
                    return Err("upstream response exceeds size limit".into());
                }
                result["candidates"][0]["content"]["parts"]
                    .as_array_mut()
                    .unwrap()
                    .extend(parts.iter().cloned());
            }
            if let Some(usage) = body.get("usageMetadata") {
                result["usageMetadata"] = usage.clone();
            }
            if gemini_finished(frame) {
                result["candidates"][0]["finishReason"] =
                    body["candidates"][0]["finishReason"].clone();
            }
            if gemini_finished(result) {
                self.result = Some(result.clone());
            }
            return Ok(());
        }
        if to != Dialect::Responses {
            return Ok(());
        }
        match frame["type"].as_str() {
            Some("response.output_item.done") if frame["item"].is_object() => {
                let index = frame["output_index"]
                    .as_u64()
                    .unwrap_or(self.items.len() as u64);
                self.items.push((index, frame["item"].clone()));
            }
            Some("response.completed" | "response.incomplete") => {
                self.result = Some(frame["response"].clone());
            }
            _ => {}
        }
        Ok(())
    }
}

fn stream_response(
    response: reqwest::Response,
    from: Dialect,
    to: Dialect,
    model: String,
    call: CallGuard,
    idle: Duration,
    custom: std::collections::BTreeSet<String>,
    replay: Option<NativeReplay>,
    quota_state: Arc<AppState>,
) -> Response {
    let meta = call.call.provider == "meta";
    let bridge = if from == to {
        None
    } else {
        Some(Bridge::new(to, from, &model).with_custom_tools(custom))
    };
    let state = (
        Some(Box::pin(response.bytes_stream())),
        Frames::default(),
        bridge,
        call,
        false,
        replay,
        quota_state,
    );
    let stream = futures_util::stream::unfold(
        state,
        move |(
            mut upstream,
            mut frames,
            mut bridge,
            mut call,
            mut done,
            mut replay,
            quota_state,
        )| async move {
            if done {
                return None;
            }
            loop {
                let next = next_chunk(
                    upstream.as_mut().expect("active upstream"),
                    idle,
                    "upstream stream interrupted",
                )
                .await;
                let output: Result<Option<Bytes>, String> = match next {
                    Some(Ok(chunk)) => match frames.feed(&chunk) {
                        Ok(events) => {
                            let mut translated = String::new();
                            let mut failure = None;
                            for mut event in events {
                                if meta && crate::subscriptions::meta::subscription_event(&event) {
                                    quota_state
                                        .quotas
                                        .observe_meta_event(&call.call.account_id, &event)
                                        .await;
                                    quota_state.observe_usage(&call.call.account_id).await;
                                    continue;
                                }
                                if meta {
                                    crate::subscriptions::meta::sanitize_error_event(&mut event);
                                }
                                if meta && bridge.is_none() {
                                    let kind = event["type"].as_str().unwrap_or("message");
                                    translated
                                        .push_str(&format!("event: {kind}\ndata: {event}\n\n"));
                                }
                                if let Some(replay) = &mut replay {
                                    replay.observe(&mut event);
                                }
                                call.observe(&event);
                                let defer = replay.as_ref().is_some_and(|replay| {
                                    gemini_finished(&event) || replay.terminal_output.is_some()
                                });
                                if let Some(b) = bridge.as_mut() {
                                    match b.feed(event) {
                                        Ok(s) if defer => {
                                            let held = replay
                                                .as_mut()
                                                .unwrap()
                                                .terminal_output
                                                .get_or_insert_default();
                                            if held.len().saturating_add(s.len()) > 32 * 1024 * 1024
                                            {
                                                failure = Some(
                                                    "upstream response exceeds size limit".into(),
                                                );
                                                break;
                                            }
                                            held.push_str(&s);
                                        }
                                        Ok(s) => translated.push_str(&s),
                                        Err(e) => {
                                            failure = Some(e);
                                            break;
                                        }
                                    }
                                }
                            }
                            if meta && call.complete && failure.is_none() {
                                done = true;
                                if let Some(body) = upstream.take() {
                                    meta_tail(
                                        body,
                                        std::mem::take(&mut frames),
                                        quota_state.clone(),
                                        call.call.account_id.clone(),
                                    );
                                }
                            }
                            if let Some(e) = failure {
                                Err(e)
                            } else if bridge.is_some() || meta {
                                Ok((!translated.is_empty()).then(|| Bytes::from(translated)))
                            } else {
                                Ok(Some(chunk))
                            }
                        }
                        Err(e) => Err(e),
                    },
                    Some(Err(e)) => Err(e),
                    None => {
                        done = true;
                        if let Some(b) = bridge.as_mut() {
                            let ending = frames.finish();
                            if meta && let Ok(events) = &ending {
                                for event in events {
                                    quota_state
                                        .quotas
                                        .observe_meta_event(&call.call.account_id, event)
                                        .await;
                                    quota_state.observe_usage(&call.call.account_id).await;
                                }
                            }
                            let tail = (|| -> Result<String, String> {
                                let mut output = String::new();
                                // Completion stays private until decoding and
                                // native-history storage have both succeeded.
                                if let Some(replay) = &mut replay {
                                    output.push_str(
                                        &replay.terminal_output.take().unwrap_or_default(),
                                    );
                                }
                                for mut event in ending? {
                                    if meta
                                        && crate::subscriptions::meta::subscription_event(&event)
                                    {
                                        continue;
                                    }
                                    if meta {
                                        crate::subscriptions::meta::sanitize_error_event(
                                            &mut event,
                                        );
                                    }
                                    if let Some(replay) = &mut replay {
                                        replay.observe(&mut event);
                                    }
                                    call.observe(&event);
                                    output.push_str(&b.feed(event)?);
                                }
                                output.push_str(&b.finish()?);
                                if call.call.error.is_none() {
                                    if let Some(replay) = &mut replay {
                                        replay.commit()?;
                                    }
                                }
                                Ok(output)
                            })();
                            match tail {
                                Ok(t) => {
                                    call.complete = true;
                                    Ok((!t.is_empty()).then(|| Bytes::from(t)))
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            match frames.finish() {
                                Ok(events) => {
                                    let mut tail = String::new();
                                    for mut event in events {
                                        if meta
                                            && crate::subscriptions::meta::subscription_event(
                                                &event,
                                            )
                                        {
                                            quota_state
                                                .quotas
                                                .observe_meta_event(&call.call.account_id, &event)
                                                .await;
                                            quota_state.observe_usage(&call.call.account_id).await;
                                            continue;
                                        }
                                        if meta {
                                            crate::subscriptions::meta::sanitize_error_event(
                                                &mut event,
                                            );
                                        }
                                        if meta {
                                            let kind = event["type"].as_str().unwrap_or("message");
                                            tail.push_str(&format!(
                                                "event: {kind}\ndata: {event}\n\n"
                                            ));
                                        }
                                        call.observe(&event);
                                    }
                                    if call.complete {
                                        Ok((!tail.is_empty()).then(|| Bytes::from(tail)))
                                    } else {
                                        Err("upstream stream closed before completion".into())
                                    }
                                }
                                Err(e) => Err(e),
                            }
                        }
                    }
                };
                match output {
                    Ok(Some(bytes)) => {
                        return Some((
                            Ok::<Bytes, std::io::Error>(bytes),
                            (upstream, frames, bridge, call, done, replay, quota_state),
                        ));
                    }
                    Ok(None) if done => return None,
                    Ok(None) => continue,
                    Err(e) => {
                        call.fail(&e);
                        done = true;
                        let body =
                            json!({"type":"error","error":{"type":"upstream_error","message":e}});
                        return Some((
                            Ok(Bytes::from(format!("event: error\ndata: {body}\n\n"))),
                            (upstream, frames, bridge, call, done, replay, quota_state),
                        ));
                    }
                }
            }
        },
    );
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

pub(crate) struct CallGuard {
    pub call: Call,
    pub complete: bool,
    start: Instant,
    ledger: Arc<Ledger>,
    uncached_input: Option<u64>,
    clients: Option<Arc<crate::clients::Clients>>,
}
impl CallGuard {
    pub fn new(ledger: Arc<Ledger>, account: &AccountCredential, model: &str) -> Self {
        ledger.in_flight.fetch_add(1, Ordering::Relaxed);
        Self {
            call: Call {
                id: format!("{:032x}", rand::random::<u128>()),
                started_at: crate::subscriptions::now(),
                provider: account.pool().into(),
                account_id: account.id.clone(),
                model: model.into(),
                status: 200,
                ..Call::default()
            },
            complete: false,
            start: Instant::now(),
            ledger,
            uncached_input: None,
            clients: None,
        }
    }
    /// Records the call under its client and counts it in the client usage
    /// of its account.
    pub fn for_client(
        mut self,
        clients: &Arc<crate::clients::Clients>,
        client: Option<&crate::clients::Client>,
    ) -> Self {
        self.call.client_id = client.map(|c| c.id.clone());
        clients.call_started(&self.call.account_id, self.call.client_id.as_deref());
        self.clients = Some(clients.clone());
        self
    }
    pub fn fail(&mut self, error: &str) {
        self.call.error = Some(error.to_owned());
        self.complete = true;
    }
    pub fn observe(&mut self, value: &Value) {
        let usage = value
            .get("usage")
            .or_else(|| value.pointer("/message/usage"))
            .or_else(|| value.pointer("/response/usage"))
            .or_else(|| value.get("usageMetadata"))
            .or_else(|| value.pointer("/response/usageMetadata"));
        if let Some(u) = usage {
            self.call.input_tokens = u["input_tokens"]
                .as_u64()
                .or_else(|| u["prompt_tokens"].as_u64())
                .or_else(|| u["promptTokenCount"].as_u64())
                .unwrap_or(self.call.input_tokens);
            self.call.output_tokens = u["output_tokens"]
                .as_u64()
                .or_else(|| u["completion_tokens"].as_u64())
                .or_else(|| u["candidatesTokenCount"].as_u64())
                .unwrap_or(self.call.output_tokens);
            self.call.cache_read_tokens = u["cache_read_input_tokens"]
                .as_u64()
                .or_else(|| {
                    u.pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                })
                .or_else(|| {
                    u.pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                })
                .or_else(|| u["cachedContentTokenCount"].as_u64())
                .unwrap_or(self.call.cache_read_tokens);
            self.call.cache_write_tokens = u["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(self.call.cache_write_tokens);
            // Anthropic input_tokens excludes cache reads/writes; other dialects include them.
            if u.get("cache_read_input_tokens").is_some()
                || u.get("cache_creation_input_tokens").is_some()
            {
                self.uncached_input = u["input_tokens"].as_u64().or(self.uncached_input);
            }
            if let Some(uncached) = self.uncached_input {
                self.call.input_tokens =
                    uncached + self.call.cache_read_tokens + self.call.cache_write_tokens;
            }
        }
        if matches!(
            value["type"].as_str(),
            Some(
                "message_stop"
                    | "response.completed"
                    | "response.incomplete"
                    | "__douane_stream_done"
            )
        ) {
            self.complete = true;
        }
        if value["type"] == "error"
            || value["type"] == "response.failed"
            || value.get("error").is_some_and(|v| !v.is_null())
        {
            // The provider's error code (rate_limit_exceeded, usage_limit_reached)
            // tells a rate limit from an empty window; its message stays out.
            let code = ["/error/code", "/response/error/code", "/error/type"]
                .iter()
                .find_map(|path| value.pointer(path).and_then(Value::as_str))
                .filter(|code| {
                    code.len() <= 64 && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                });
            match code {
                Some(code) => self.fail(&format!(
                    "upstream reported an error in its stream ({code})"
                )),
                None => self.fail("upstream reported an error in its stream"),
            }
        }
    }
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.ledger.in_flight.fetch_sub(1, Ordering::Relaxed);
        if !self.complete {
            self.call.error =
                Some("client disconnected or upstream response was interrupted".into());
        }
        self.call.latency_ms = self.start.elapsed().as_millis() as u64;
        if let Some(clients) = &self.clients {
            clients.call_ended(
                &self.call.account_id,
                self.call.client_id.as_deref(),
                crate::clients::call_weight(&self.call),
            );
        }
        self.ledger.record(std::mem::take(&mut self.call));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_client_key_header_reaches_the_provider() {
        let account: AccountCredential = serde_json::from_value(json!({
            "id": "a", "name": "a", "provider": "antigravity", "access_token": "upstream",
            "enabled": true, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let mut incoming = HeaderMap::new();
        for name in ["authorization", "x-api-key", "x-goog-api-key", "api-key"] {
            incoming.insert(name, "dk_client".parse().unwrap());
        }
        let sent = upstream_headers(&account, &incoming);
        assert!(sent.values().all(|v| v != "dk_client"), "{sent:?}");
    }

    #[test]
    fn an_exhausted_window_rests_until_the_provider_reset() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, value.parse().unwrap());
            }
            map
        };
        assert_eq!(
            cooldown_seconds(&headers(&[("retry-after", "12")]), 0),
            Some(12)
        );
        let claude = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "1000"),
        ]);
        assert_eq!(cooldown_seconds(&claude, 400), Some(600));
        let codex = headers(&[
            ("x-codex-primary-used-percent", "62"),
            ("x-codex-primary-reset-after-seconds", "100"),
            ("x-codex-secondary-used-percent", "100"),
            ("x-codex-secondary-reset-after-seconds", "7200"),
        ]);
        assert_eq!(cooldown_seconds(&codex, 0), Some(7200));
        assert_eq!(
            cooldown_seconds(
                &headers(&[("anthropic-ratelimit-unified-status", "allowed")]),
                0
            ),
            None
        );
    }

    fn fixture_account(provider: SubscriptionProvider) -> AccountCredential {
        serde_json::from_value(json!({
            "id":"fixture-account", "name":"fixture", "provider":provider,
            "access_token":"fixture-not-a-real-token", "enabled":true,
            "created_at":"2026-10-03T12:00:00Z", "updated_at":"2026-10-03T12:00:00Z"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn anthropic_cache_write_then_read_counts_input_once_across_usage_events() {
        let ledger = Ledger::open(std::path::Path::new(":memory:")).unwrap();
        let account = fixture_account(SubscriptionProvider::Claude);
        let mut writing = CallGuard::new(ledger.clone(), &account, "claude-sonnet-4-6");
        let write_id = writing.call.id.clone();
        writing.observe(&json!({"type":"message_start","message":{"usage":{
            "input_tokens":13,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":7231
        }}}));
        assert_eq!(writing.call.input_tokens, 7244);
        writing.observe(&json!({"type":"message_delta","usage":{"output_tokens":22}}));
        writing.observe(&json!({"type":"message_delta","usage":{"output_tokens":22,"cache_creation_input_tokens":7231}}));
        assert_eq!(writing.call.input_tokens, 7244);
        writing.observe(&json!({"type":"message_stop"}));
        drop(writing);

        let mut reading = CallGuard::new(ledger.clone(), &account, "claude-sonnet-4-6");
        let read_id = reading.call.id.clone();
        reading.observe(&json!({"type":"message_start","message":{"usage":{
            "input_tokens":19,"output_tokens":0,"cache_read_input_tokens":7231,"cache_creation_input_tokens":0
        }}}));
        assert_eq!(reading.call.input_tokens, 7250);
        reading.observe(&json!({"type":"message_delta","usage":{"output_tokens":16}}));
        reading.observe(&json!({"type":"message_delta","usage":{"output_tokens":16,"cache_read_input_tokens":7231}}));
        assert_eq!(reading.call.input_tokens, 7250);
        reading.observe(&json!({"type":"message_stop"}));
        drop(reading);

        assert_eq!(ledger.requests.load(Ordering::Relaxed), 2);
        assert_eq!(ledger.errors.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.input_tokens.load(Ordering::Relaxed), 14494);
        assert_eq!(ledger.output_tokens.load(Ordering::Relaxed), 38);
        assert_eq!(ledger.cache_read_tokens.load(Ordering::Relaxed), 7231);
        assert_eq!(ledger.cache_write_tokens.load(Ordering::Relaxed), 7231);
        assert_eq!(ledger.in_flight.load(Ordering::Relaxed), 0);
        let calls = ledger.calls(2).await;
        let written = calls.iter().find(|call| call.id == write_id).unwrap();
        let read = calls.iter().find(|call| call.id == read_id).unwrap();
        assert_eq!(
            (
                written.input_tokens,
                written.cache_read_tokens,
                written.cache_write_tokens
            ),
            (7244, 0, 7231)
        );
        assert_eq!(
            (
                read.input_tokens,
                read.cache_read_tokens,
                read.cache_write_tokens
            ),
            (7250, 7231, 0)
        );
    }

    #[test]
    fn google_refusal_keeps_enum_fields_and_retry_delay_only() {
        let body = br#"{"error":{"code":429,"message":"No capacity on project secret-project-42 for you@example.com","status":"RESOURCE_EXHAUSTED",
            "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"MODEL_CAPACITY_EXHAUSTED","domain":"cloudcode-pa.googleapis.com","metadata":{"model":"gemini-3.8-flash-high"}},
            {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"2.5s"}]}}"#;
        let refusal = super::google_refusal(body).unwrap();
        assert_eq!(
            refusal.reason,
            "RESOURCE_EXHAUSTED/MODEL_CAPACITY_EXHAUSTED"
        );
        assert_eq!(refusal.retry_seconds, Some(3));
        let quota = br#"{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"reason":"QUOTA_EXHAUSTED","metadata":{"quotaResetDelay":"1h2m3.4s"}}]}}"#;
        assert_eq!(
            super::google_refusal(quota).unwrap().retry_seconds,
            Some(3724)
        );
        assert_eq!(super::google_duration("750ms"), Some(1));
        assert_eq!(super::google_duration("soon"), None);
        let free_text = br#"{"error":{"status":"Project secret-project-42 is blocked"}}"#;
        assert_eq!(super::google_refusal(free_text), None);
        assert_eq!(super::google_refusal(b"not json"), None);
    }

    #[tokio::test]
    async fn codex_input_total_already_includes_cache_reads_and_is_not_added_again() {
        let ledger = Ledger::open(std::path::Path::new(":memory:")).unwrap();
        let account = fixture_account(SubscriptionProvider::Codex);
        let mut call = CallGuard::new(ledger.clone(), &account, "gpt-fixture");
        let completed = json!({"type":"response.completed","response":{"usage":{
            "input_tokens":900,"output_tokens":40,"input_tokens_details":{"cached_tokens":800}
        }}});
        // Buffered collection can observe both the terminal event and its response.
        call.observe(&completed);
        call.observe(&completed["response"]);
        assert_eq!(call.call.input_tokens, 900);
        assert_eq!(call.call.cache_read_tokens, 800);
        assert_eq!(call.call.cache_write_tokens, 0);
        drop(call);
        assert_eq!(ledger.input_tokens.load(Ordering::Relaxed), 900);
        assert_eq!(ledger.cache_read_tokens.load(Ordering::Relaxed), 800);
        assert_eq!(ledger.requests.load(Ordering::Relaxed), 1);
        assert_eq!(ledger.errors.load(Ordering::Relaxed), 0);
        let recorded = ledger.calls(1).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            (
                recorded[0].input_tokens,
                recorded[0].output_tokens,
                recorded[0].cache_read_tokens
            ),
            (900, 40, 800)
        );
    }
}
