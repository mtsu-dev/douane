//! Persistent native Responses sockets. Backpressure propagates to the upstream socket.

use crate::{
    relay::{self, CallGuard},
    state::AppState,
    subscriptions::{AccountCredential, SubscriptionProvider},
    translate::Dialect,
};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Error as WsError, Message as UpMessage, client::IntoClientRequest},
};

type NativeSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn downstream_send(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(30), socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn upstream_send(socket: &mut NativeSocket, message: UpMessage) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(30), socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn close_for_recovery(socket: &mut WebSocket) {
    downstream_send(
        socket,
        Message::Close(Some(CloseFrame {
            code: 1013,
            reason: "reconnect with full input and previous_response_id:null".into(),
        })),
    )
    .await;
}

fn has_previous(value: &Value) -> bool {
    value
        .get("previous_response_id")
        .is_some_and(|id| !id.is_null())
}

async fn previous_not_found(socket: &mut WebSocket, request: &Value) {
    let mut error = json!({"type":"error","status":400,"error":{"type":"invalid_request_error","code":"previous_response_not_found","param":"previous_response_id","message":"The subscription connection changed. Reconnect with full input and previous_response_id:null."}});
    if let Some(lane) = request.get("stream_id") {
        error["stream_id"] = lane.clone();
    }
    downstream_send(socket, Message::Text(error.to_string().into())).await;
    close_for_recovery(socket).await;
}

#[derive(Clone, Copy)]
struct NativeFailure {
    status: u16,
    seconds: u64,
    reason: &'static str,
}

fn failure_status(status: u16) -> Option<NativeFailure> {
    match status {
        429 => Some(NativeFailure {
            status,
            seconds: 60,
            reason: "websocket upstream rate limited",
        }),
        401 | 403 => Some(NativeFailure {
            status,
            seconds: 300,
            reason: "websocket upstream authentication failed",
        }),
        _ => None,
    }
}

fn native_failure(value: &Value) -> Option<NativeFailure> {
    if !matches!(value["type"].as_str(), Some("error" | "response.failed")) {
        return None;
    }
    let error = value
        .get("error")
        .or_else(|| value.pointer("/response/error"))
        .unwrap_or(value);
    let status = value["status"]
        .as_u64()
        .or_else(|| error["status"].as_u64())
        .and_then(|n| u16::try_from(n).ok());
    let mut failure = [error["code"].as_str(), error["type"].as_str()]
        .into_iter()
        .flatten()
        .find_map(|code| match code {
            "rate_limit_exceeded" | "slow_down" => failure_status(429),
            "usage_limit_reached"
            | "insufficient_quota"
            | "credit_balance_exhausted"
            | "organization_spend_limit_exceeded"
            | "project_spend_limit_exceeded"
            | "quota_exceeded" => failure_status(429).map(|failure| NativeFailure {
                reason: "websocket upstream quota exceeded",
                ..failure
            }),
            "invalid_api_key"
            | "authentication_error"
            | "invalid_authentication"
            | "invalid_token"
            | "token_expired"
            | "unauthorized" => failure_status(401),
            _ => None,
        })
        .or_else(|| status.and_then(failure_status))?;
    let seconds = error["resets_in_seconds"]
        .as_u64()
        .or_else(|| error["retry_after"].as_u64())
        .or_else(|| {
            error["resets_at"].as_i64().map(|at| {
                at.saturating_sub(time::OffsetDateTime::now_utc().unix_timestamp())
                    .max(1) as u64
            })
        });
    if let Some(seconds) = seconds {
        failure.seconds = seconds.clamp(1, 86_400);
    }
    Some(failure)
}

fn account_eligible(account: &AccountCredential, model: &str) -> bool {
    matches!(account.summary().status.as_str(), "ready" | "error") && account.serves(model)
}

/// Selection offers a resting account only when its pool has nothing ready.
fn account_offered(account: &AccountCredential, model: &str) -> bool {
    account_eligible(account, model) || (account.resting() && account.serves(model))
}

fn bind_account(state: &AppState, account: &AccountCredential, session: Option<&str>) {
    if let Some(key) = session {
        state
            .affinity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bind(account.provider.as_str(), key, &account.id);
    }
}

fn previous_owner(
    state: &AppState,
    provider: SubscriptionProvider,
    request: &Value,
) -> Option<String> {
    let response = request["previous_response_id"].as_str()?;
    state
        .affinity
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .response_owner(provider.as_str(), response)
}

fn remember_response(state: &AppState, account: &AccountCredential, event: &Value) {
    if matches!(
        event["type"].as_str(),
        Some("response.created" | "response.completed" | "response.incomplete" | "response.failed")
    ) {
        if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
            state
                .affinity
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bind_response(account.provider.as_str(), id, &account.id);
        }
    }
}

async fn connect_pool(
    state: &AppState,
    headers: &HeaderMap,
    session: Option<&str>,
    model: &str,
    provider: SubscriptionProvider,
    candidates: &mut VecDeque<AccountCredential>,
    owner: Option<&str>,
    attempted: &mut BTreeSet<String>,
) -> Option<(AccountCredential, NativeSocket)> {
    while let Some(candidate) = candidates.pop_front() {
        if candidate.provider != provider {
            continue;
        }
        // A continuation belongs to its original subscription, including on reconnect.
        if owner.is_some_and(|id| id != candidate.id) {
            continue;
        }
        if attempted.len() >= 64 {
            return None;
        }
        if !attempted.insert(candidate.id.clone()) {
            continue;
        }
        let account = match state.oauth.credential(&candidate.id).await {
            Ok(account) if account_offered(&account, model) => account,
            _ => continue,
        };
        let (url, _) = relay::upstream(&account, Dialect::Responses, model, &[]);
        let url = url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        let Ok(mut request) = url.into_client_request() else {
            continue;
        };
        for (name, value) in relay::upstream_headers(&account, headers) {
            if let Some(name) = name {
                request.headers_mut().insert(name, value);
            }
        }
        request
            .headers_mut()
            .entry("openai-beta")
            .or_insert(axum::http::HeaderValue::from_static(
                "responses_websockets=2026-02-06",
            ));
        if !request.headers().contains_key("session_id") {
            if let Some(value) = session.and_then(|key| key.parse::<axum::http::HeaderValue>().ok())
            {
                request.headers_mut().insert("session_id", value);
            }
        }
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(64 * 1024 * 1024))
            .max_frame_size(Some(64 * 1024 * 1024))
            .write_buffer_size(0)
            .max_write_buffer_size(64 * 1024 * 1024 + 4096);
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            tokio_tungstenite::connect_async_with_config(request, Some(config), true),
        )
        .await;
        match result {
            Ok(Ok((socket, _))) => return Some((account, socket)),
            Ok(Err(WsError::Http(response))) => {
                let mut failure =
                    failure_status(response.status().as_u16()).unwrap_or(NativeFailure {
                        status: 502,
                        seconds: 15,
                        reason: "websocket upstream connection failed",
                    });
                // Rest until the provider's own reset, as the HTTP relay does.
                if let Some(seconds) =
                    crate::relay::cooldown_seconds(response.headers(), crate::relay::unix_now())
                {
                    failure.seconds = seconds;
                }
                let _ = state
                    .accounts
                    .cooldown(&account.id, failure.seconds, failure.reason)
                    .await;
            }
            _ => {
                let _ = state
                    .accounts
                    .cooldown(&account.id, 15, "websocket upstream connection failed")
                    .await;
            }
        }
    }
    None
}

async fn connect_replacement(
    state: &AppState,
    headers: &HeaderMap,
    session: Option<&str>,
    model: &str,
    provider: SubscriptionProvider,
    attempted: &mut BTreeSet<String>,
) -> Option<(AccountCredential, NativeSocket)> {
    // Only failures replan. Added, re-enabled or reset accounts become visible here.
    // A key revoked since the socket opened gets no replacement account.
    let client = state.clients.authenticate(headers).ok()?;
    let (_, candidates) = state
        .candidates_for(model, Some(provider.as_str()), session, Some(&client))
        .await
        .ok()?;
    connect_pool(
        state,
        headers,
        session,
        model,
        provider,
        &mut candidates.into(),
        None,
        attempted,
    )
    .await
}

pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(refusal) = state.clients.authenticate(&headers) {
        return relay::error(axum::http::StatusCode::UNAUTHORIZED, refusal.message());
    }
    upgrade
        .max_message_size(64 * 1024 * 1024)
        .max_frame_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| run(socket, state, headers))
}

async fn reject(socket: &mut WebSocket, message: &str) {
    let body = json!({"type":"error","error":{"type":"proxy_error","message":message}});
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(body.to_string().into())),
    )
    .await;
    downstream_send(
        socket,
        Message::Close(Some(CloseFrame {
            code: 1011,
            reason: "upstream unavailable".into(),
        })),
    )
    .await;
}

async fn run(mut downstream: WebSocket, state: Arc<AppState>, headers: HeaderMap) {
    let first = loop {
        match tokio::time::timeout(Duration::from_secs(60), downstream.recv()).await {
            Ok(Some(Ok(Message::Text(text)))) => break text,
            Ok(Some(Ok(Message::Ping(data)))) => {
                if !downstream_send(&mut downstream, Message::Pong(data)).await {
                    return;
                }
            }
            Ok(Some(Ok(Message::Pong(_)))) => {}
            _ => return,
        }
    };
    let mut initial: Value = match serde_json::from_str(&first) {
        Ok(v) => v,
        Err(_) => {
            reject(
                &mut downstream,
                "first websocket message must be valid JSON",
            )
            .await;
            return;
        }
    };
    if initial["type"] != "response.create" {
        reject(
            &mut downstream,
            "first websocket message must be response.create",
        )
        .await;
        return;
    }
    let mut client = match state.clients.authenticate(&headers) {
        Ok(client) => Some(client),
        Err(refusal) => {
            reject(&mut downstream, refusal.message()).await;
            return;
        }
    };
    let (model, candidates, session) =
        match relay::choose(&state, &initial, &headers, client.as_ref()).await {
            Ok(v) => v,
            Err(e) => {
                if has_previous(&initial) {
                    previous_not_found(&mut downstream, &initial).await;
                } else {
                    reject(&mut downstream, &e).await;
                }
                return;
            }
        };
    // Codex and a plain OpenAI endpoint keep a native socket; every other
    // pool, custom routes included, goes through the HTTP bridge.
    if let Some(account) = candidates.first().filter(|a| {
        !(a.provider == SubscriptionProvider::Codex
            || (a.provider == SubscriptionProvider::OpenaiCompatible && a.route.is_none()))
    }) {
        initial["model"] = json!(format!("{}/{}", account.pool(), model));
        crate::socket_compat::run(downstream, state, headers, initial).await;
        return;
    }
    initial["model"] = json!(model);
    let provider = candidates[0].provider;
    let initial_owner = previous_owner(&state, provider, &initial);
    if has_previous(&initial) {
        let pinned = session.as_deref().and_then(|key| {
            state
                .affinity
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(provider.as_str(), key)
        });
        if initial_owner.is_none() || pinned.is_some_and(|id| Some(&id) != initial_owner.as_ref()) {
            // A recovered account must not steal a conversation from its replacement.
            previous_not_found(&mut downstream, &initial).await;
            return;
        }
    }
    let mut candidates: VecDeque<_> = candidates.into();
    let mut attempted = BTreeSet::new();
    let Some((mut account, mut upstream)) = connect_pool(
        &state,
        &headers,
        session.as_deref(),
        &model,
        provider,
        &mut candidates,
        initial_owner.as_deref(),
        &mut attempted,
    )
    .await
    else {
        if has_previous(&initial) {
            previous_not_found(&mut downstream, &initial).await;
        } else {
            reject(
                &mut downstream,
                "no native Responses websocket is available for this model",
            )
            .await;
        }
        return;
    };
    bind_account(&state, &account, session.as_deref());
    if initial.get("prompt_cache_key").is_none() {
        if let Some(key) = &session {
            initial["prompt_cache_key"] = json!(key);
        }
    }
    if !upstream_send(&mut upstream, UpMessage::Text(initial.to_string().into())).await {
        reject(&mut downstream, "upstream websocket send failed").await;
        return;
    }
    let mut calls: BTreeMap<String, VecDeque<Option<CallGuard>>> = BTreeMap::new();
    track_call(&mut calls, &state, &account, &initial, client.as_ref());
    // Retain at most one request, and release its payload on the first forwarded event.
    let mut retry_request = Some(initial);
    let beat = Duration::from_millis(
        state
            .socket_heartbeat_ms
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let silence = beat * 18 / 5;
    let mut heartbeat = tokio::time::interval(beat);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let mut last_upstream = tokio::time::Instant::now();
    let mut last_downstream = tokio::time::Instant::now();
    loop {
        tokio::select! {
            message = downstream.recv() => {
                last_downstream = tokio::time::Instant::now();
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let mut body: Value = match serde_json::from_str(&text) {
                            Ok(value) => value,
                            Err(_) => { reject(&mut downstream, "websocket message must be valid JSON").await; break; }
                        };
                        if body["type"] == "response.create" {
                            let requested = body["model"].as_str().unwrap_or(&model).to_owned();
                            let requested = requested.strip_prefix(&format!("{}/", provider.as_str())).unwrap_or(&requested).to_owned();
                            if (requested.contains('/') || !account.models.is_empty()) && !account.models.contains(&requested) {
                                reject(&mut downstream, "a websocket session cannot switch provider or subscription").await;
                                break;
                            }
                            let qualified = format!("{}/{}", provider.as_str(), requested);
                            if state.config.read().await.disabled_models.contains(&qualified) {
                                reject(&mut downstream, "this model is disabled").await;
                                break;
                            }
                            let pending = calls.values().map(VecDeque::len).sum::<usize>();
                            if has_previous(&body) && previous_owner(&state, provider, &body).as_deref() != Some(&account.id) {
                                previous_not_found(&mut downstream, &body).await;
                                break;
                            }
                            if pending == 0 { attempted.clear(); }
                            // Each request reads the client again: a revoked key stops here,
                            // and changed access or shares apply to the next request.
                            client = match state.clients.authenticate(&headers) {
                                Ok(client) => Some(client),
                                Err(refusal) => {
                                    reject(&mut downstream, refusal.message()).await;
                                    break;
                                }
                            };
                            let current = state.accounts.get(&account.id).await;
                            // A resting account stays only while its pool still has nothing else.
                            let kept = match &current {
                                Some(a) if account_eligible(a, &requested) => true,
                                Some(a) if account_offered(a, &requested) => state
                                    .candidates_for(&requested, Some(provider.as_str()), session.as_deref(), client.as_ref())
                                    .await
                                    .is_ok_and(|(_, c)| c.first().is_some_and(|first| first.id == a.id)),
                                _ => false,
                            };
                            let usable = match current.filter(|_| kept) {
                                Some(current) => !state.reserve_excludes(&current).await
                                    && !state.client_excludes(client.as_ref(), &current).await
                                    && !state.credits_avoidable(&current, &requested).await,
                                None => false,
                            };
                            if !usable {
                                if has_previous(&body) {
                                    previous_not_found(&mut downstream, &body).await;
                                    break;
                                }
                                // A new full request can move before it is sent; pending lanes cannot.
                                if pending != 0 {
                                    reject(&mut downstream, "subscription unavailable; reconnect with full input").await;
                                    break;
                                }
                                let _ = tokio::time::timeout(Duration::from_secs(2), upstream.close(None)).await;
                                attempted.insert(account.id.clone());
                                let Some((replacement, socket)) = connect_replacement(
                                    &state, &headers, session.as_deref(), &requested, provider, &mut attempted,
                                ).await else {
                                    reject(&mut downstream, "no eligible replacement subscription; reconnect with full input").await;
                                    break;
                                };
                                account = replacement;
                                upstream = socket;
                                bind_account(&state, &account, session.as_deref());
                                last_upstream = tokio::time::Instant::now();
                            }
                            body["model"] = json!(requested);
                            if body.get("prompt_cache_key").is_none() {
                                if let Some(key) = &session { body["prompt_cache_key"] = json!(key); }
                            }
                            if pending >= 64 {
                                reject(&mut downstream, "too many pending websocket responses").await;
                                break;
                            }
                            retry_request = if pending == 0 { Some(body.clone()) } else { None };
                            track_call(&mut calls, &state, &account, &body, client.as_ref());
                        } else {
                            // A cancellation/steering message makes replaying the retained create unsafe.
                            retry_request = None;
                        }
                        if !upstream_send(&mut upstream, UpMessage::Text(body.to_string().into())).await {
                            reject(&mut downstream, "upstream websocket send failed; reconnect with full input").await;
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if !downstream_send(&mut downstream, Message::Pong(data)).await { break; }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => { break; }
                    Some(Ok(Message::Binary(_))) => {
                        reject(&mut downstream, "Responses websocket accepts JSON text messages").await;
                        break;
                    }
                    Some(Err(_)) => break,
                }
            }
            message = upstream.next() => {
                last_upstream = tokio::time::Instant::now();
                match message {
                    Some(Ok(UpMessage::Text(text))) => {
                        if let Ok(value) = serde_json::from_str::<Value>(&text) {
                            let failure = native_failure(&value);
                            let pending = calls.values().map(VecDeque::len).sum::<usize>();
                            let can_retry = failure.is_some() && pending == 1
                                && retry_request.is_some();
                            let lane = value["stream_id"].as_str().unwrap_or("");
                            let terminal = matches!(value["type"].as_str(), Some("response.completed" | "response.incomplete" | "response.failed" | "error"));
                            // Native terminals may omit stream_id. Never guess between concurrent lanes.
                            let fallback_lane = if terminal && pending == 1 && !calls.contains_key(lane) {
                                calls.iter().find(|(_, q)| !q.is_empty()).map(|(lane, _)| lane.clone())
                            } else { None };
                            let observed_lane = fallback_lane.as_deref().unwrap_or(lane);
                            let mut empty = false;
                            if let Some(queue) = calls.get_mut(observed_lane) {
                                if let Some(Some(call)) = queue.front_mut() {
                                    call.observe(&value);
                                    if let Some(failure) = failure { call.call.status = failure.status; }
                                }
                                if terminal {
                                    queue.pop_front();
                                }
                                empty = queue.is_empty();
                            }
                            if empty { calls.remove(observed_lane); }
                            if let Some(failure) = failure {
                                let _ = state.accounts.cooldown(&account.id, failure.seconds, failure.reason).await;
                                if can_retry {
                                    let request = retry_request.take().unwrap();
                                    if has_previous(&request) {
                                        previous_not_found(&mut downstream, &request).await;
                                        break;
                                    }
                                    let _ = tokio::time::timeout(Duration::from_secs(2), upstream.close(None)).await;
                                    let requested = request["model"].as_str().unwrap_or(&model);
                                    attempted.insert(account.id.clone());
                                    if let Some((replacement, socket)) = connect_replacement(
                                        &state, &headers, session.as_deref(), requested, provider, &mut attempted,
                                    ).await {
                                        account = replacement;
                                        upstream = socket;
                                        bind_account(&state, &account, session.as_deref());
                                        if !upstream_send(&mut upstream, UpMessage::Text(request.to_string().into())).await {
                                            reject(&mut downstream, "replacement websocket send failed; reconnect with full input").await;
                                            break;
                                        }
                                        track_call(&mut calls, &state, &account, &request, client.as_ref());
                                        retry_request = Some(request);
                                        last_upstream = tokio::time::Instant::now();
                                        // Retry never consumes a client's already forwarded event or previous response.
                                        continue;
                                    }
                                }
                                remember_response(&state, &account, &value);
                                downstream_send(&mut downstream, Message::Text(text.to_string().into())).await;
                                close_for_recovery(&mut downstream).await;
                                break;
                            }
                            remember_response(&state, &account, &value);
                        }
                        // Even a metadata/created frame makes replay unsafe. Healthy turns retain no request history.
                        retry_request = None;
                        if !downstream_send(&mut downstream, Message::Text(text.to_string().into())).await { break; }
                    }
                    Some(Ok(UpMessage::Ping(data))) => {
                        if !upstream_send(&mut upstream, UpMessage::Pong(data)).await { break; }
                    }
                    Some(Ok(UpMessage::Pong(_))) => {}
                    Some(Ok(UpMessage::Close(close))) => {
                        let frame = close.map(|f| CloseFrame { code: f.code.into(), reason: "upstream closed connection".into() });
                        downstream_send(&mut downstream, Message::Close(frame)).await;
                        break;
                    }
                    Some(Ok(UpMessage::Binary(data))) => {
                        retry_request = None;
                        if !downstream_send(&mut downstream, Message::Binary(data)).await { break; }
                    }
                    Some(Ok(UpMessage::Frame(_))) => {}
                    Some(Err(_)) | None => {
                        reject(&mut downstream, "upstream websocket disconnected; reconnect with the full conversation if needed").await;
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                if last_downstream.elapsed() > silence {
                    close_for_recovery(&mut downstream).await;
                    break;
                }
                if last_upstream.elapsed() > silence {
                    reject(&mut downstream, "upstream websocket heartbeat timed out").await;
                    break;
                }
                if !upstream_send(&mut upstream, UpMessage::Ping(Bytes::new())).await
                    || !downstream_send(&mut downstream, Message::Ping(Bytes::new())).await {
                    break;
                }
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), upstream.close(None)).await;
}

use bytes::Bytes;
fn track_call(
    calls: &mut BTreeMap<String, VecDeque<Option<CallGuard>>>,
    state: &AppState,
    account: &AccountCredential,
    value: &Value,
    client: Option<&crate::clients::Client>,
) {
    // Warmup prepares upstream cache state and consumes no inference quota.
    let lane = value["stream_id"].as_str().unwrap_or("").to_owned();
    let call = (value["generate"] != false).then(|| {
        CallGuard::new(
            state.ledger.clone(),
            account,
            value["model"].as_str().unwrap_or(""),
        )
        .for_client(&state.clients, client)
    });
    calls.entry(lane).or_default().push_back(call);
}
