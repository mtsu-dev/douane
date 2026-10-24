use crate::{relay::error, state::AppState, subscriptions::SubscriptionProvider};
use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::Ordering},
};

static ASSETS: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../dash/build");
fn asset_response(path: &str) -> Response {
    let Some(file) = ASSETS.get_file(path) else {
        return error(StatusCode::NOT_FOUND, "asset not found");
    };
    let mime = if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CACHE_CONTROL,
                if path.contains("immutable/") {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        file.contents(),
    )
        .into_response()
}
pub async fn page() -> Response {
    asset_response(if ASSETS.get_file("index.html").is_some() {
        "index.html"
    } else {
        "200.html"
    })
}
pub async fn asset(Path(path): Path<String>) -> Response {
    asset_response(&format!("_app/{path}"))
}
pub async fn health() -> Json<Value> {
    Json(
        json!({"status":"ok","version":env!("CARGO_PKG_VERSION"),"service":"douane","websocket":true}),
    )
}

pub async fn guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    if let Some(expected) = &state.management_token {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .or_else(|| {
                request
                    .headers()
                    .get("x-management-key")
                    .and_then(|v| v.to_str().ok())
            })
            .or_else(|| {
                request
                    .headers()
                    .get("x-douane-admin")
                    .and_then(|v| v.to_str().ok())
            });
        if presented != Some(expected.as_str()) {
            return error(
                StatusCode::UNAUTHORIZED,
                "management authentication required",
            );
        }
    }
    if request.method() != axum::http::Method::GET {
        if let Some(origin) = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        {
            let host = request
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let matches = reqwest::Url::parse(origin)
                .ok()
                .is_some_and(|u| u.authority() == host);
            if !matches {
                return error(
                    StatusCode::FORBIDDEN,
                    "management writes require the same origin",
                );
            }
        }
    }
    next.run(request).await
}

pub async fn accounts(State(state): State<Arc<AppState>>) -> Json<Value> {
    let now = time::OffsetDateTime::now_utc();
    let mut accounts = vec![];
    for summary in state.accounts.list().await {
        accounts.push(account_value(&state, summary, now).await);
    }
    Json(json!({"accounts":accounts}))
}

/// An account summary with its reserve state, as listed and as returned after
/// a change. `reserve_measured` is null until a quota read has succeeded and
/// false only when the provider reports no account-wide window.
async fn account_value(
    state: &AppState,
    summary: crate::subscriptions::AccountSummary,
    now: time::OffsetDateTime,
) -> Value {
    let snapshot = state.quotas.get(&summary.id).await;
    let measured = snapshot
        .as_ref()
        .filter(|q| !(q.windows.is_empty() && q.error.is_some()))
        .map(|q| q.reports_reserve_window(&summary.reserve_windows));
    let reached = crate::state::reserve_reached_in(
        summary.reserve_percent,
        &summary.reserve_windows,
        snapshot.as_ref(),
        now,
        state.free_before_reset_seconds().await,
    );
    let mut value = json!(summary);
    value["reserve_measured"] = json!(measured);
    value["reserve_reached"] = json!(reached);
    value
}
pub async fn account_verification(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    verification_redirect(state.quotas.verification_url(&id).await)
}

pub async fn oauth_verification(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    verification_redirect(state.oauth.verification_url(&id).await)
}

fn verification_redirect(url: Option<String>) -> Response {
    let mut response = match url {
        Some(url) => Redirect::temporary(&url).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "account verification is not available",
        ),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    response
}
pub async fn import(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let files = body
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![json!({"content":body})]);
    if files.len() > 100 {
        return error(
            StatusCode::BAD_REQUEST,
            "import at most 100 accounts at once",
        );
    }
    let mut accounts = vec![];
    let mut errors = vec![];
    for file in files {
        let value = if let Some(s) = file["content"].as_str() {
            match serde_json::from_str(s) {
                Ok(v) => v,
                Err(_) => {
                    errors.push(json!({"name":file["name"],"error":"invalid JSON"}));
                    continue;
                }
            }
        } else {
            file["content"].clone()
        };
        match state
            .accounts
            .import(value, file["name"].as_str().map(str::to_owned))
            .await
        {
            Ok(a) => accounts.push(a),
            Err(e) => errors.push(json!({"name":file["name"],"error":e.to_string()})),
        }
    }
    state.refresh_models().await;
    let ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
    let worker = state.clone();
    tokio::spawn(async move {
        for id in ids {
            let _ = worker.quotas.refresh(&id).await;
        }
    });
    (
        if accounts.is_empty() && !errors.is_empty() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        },
        Json(json!({"imported":accounts.len(),"accounts":accounts,"errors":errors})),
    )
        .into_response()
}
/// Bundle format written by `export` and read by `restore`.
const EXPORT_FORMAT: &str = "douane-export";
const EXPORT_VERSION: u64 = 1;
/// Most accounts one restore accepts.
const MAX_RESTORE_ACCOUNTS: usize = 1000;

/// The whole configuration as one JSON file: settings, disabled models and
/// every stored account with its tokens. Refresh tokens rotate, so an
/// account restored elsewhere must leave this instance.
pub async fn export(State(state): State<Arc<AppState>>) -> Response {
    let (settings, disabled_models) = {
        let config = state.config.read().await;
        (config.settings.clone(), config.disabled_models.clone())
    };
    let exported_at = crate::subscriptions::now();
    let date = exported_at.get(..10).unwrap_or("export").to_owned();
    let body = json!({
        "format": EXPORT_FORMAT,
        "version": EXPORT_VERSION,
        "exported_at": exported_at,
        "settings": settings,
        "disabled_models": disabled_models,
        "accounts": state.accounts.export().await,
        "clients": state.clients.list(),
    });
    let mut response = Json(body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    if let Ok(value) = header::HeaderValue::from_str(&format!(
        "attachment; filename=\"douane-export-{date}.json\""
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// Restores an export. The body is the bundle itself, which restores its
/// accounts only, or `{"bundle": <bundle>, "settings": true}`, which also
/// applies its settings and disabled models. Settings are checked before any
/// account changes, so a refused bundle changes nothing.
pub async fn restore(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let (bundle, apply_settings) = match body.get("bundle") {
        Some(bundle) => match body.get("settings") {
            None => (bundle, false),
            Some(Value::Bool(apply)) => (bundle, *apply),
            Some(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "settings beside bundle must be true or false",
                );
            }
        },
        None => (&body, false),
    };
    if bundle.get("format").and_then(Value::as_str) != Some(EXPORT_FORMAT) {
        return error(
            StatusCode::BAD_REQUEST,
            "not a douane export: format must be \"douane-export\"",
        );
    }
    if bundle.get("version").and_then(Value::as_u64) != Some(EXPORT_VERSION) {
        return error(
            StatusCode::BAD_REQUEST,
            &format!(
                "unsupported export version {}; this douane reads version {EXPORT_VERSION}",
                bundle.get("version").unwrap_or(&Value::Null)
            ),
        );
    }
    let Some(records) = bundle.get("accounts").and_then(Value::as_array) else {
        return error(StatusCode::BAD_REQUEST, "accounts must be a list");
    };
    if records.len() > MAX_RESTORE_ACCOUNTS {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("restore at most {MAX_RESTORE_ACCOUNTS} accounts at once"),
        );
    }
    let clients: Option<Vec<crate::clients::Client>> = match bundle.get("clients") {
        None | Some(Value::Null) => None,
        Some(list) => match serde_json::from_value(list.clone()) {
            Ok(list) => Some(list),
            Err(_) => return error(StatusCode::BAD_REQUEST, "invalid clients in the export"),
        },
    };
    let configuration = if apply_settings {
        let settings: crate::settings::Settings =
            match serde_json::from_value(bundle.get("settings").cloned().unwrap_or(json!({}))) {
                Ok(settings) => settings,
                Err(_) => return error(StatusCode::BAD_REQUEST, "invalid settings in the export"),
            };
        if let Err(e) = settings.validate() {
            return error(StatusCode::BAD_REQUEST, &e.to_string());
        }
        let disabled: BTreeSet<String> = match serde_json::from_value(
            bundle.get("disabled_models").cloned().unwrap_or(json!([])),
        ) {
            Ok(disabled) => disabled,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "disabled_models must be a list of model ids",
                );
            }
        };
        Some((settings, disabled))
    } else {
        None
    };
    if let Some((settings, disabled)) = configuration {
        let _write = state.config_write.lock().await;
        let mut config = state.config.read().await.clone();
        config.settings = settings;
        config.disabled_models = disabled;
        if let Some(path) = &state.config_path {
            if config.save(path).await.is_err() {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "cannot save configuration",
                );
            }
        }
        *state.config.write().await = config;
    }
    // Clients are checked before anything changes, and stored after the
    // accounts, whose ids may differ here.
    if let Some(list) = &clients {
        if let Err(e) = state
            .clients
            .restore(list.clone(), &Default::default(), true)
        {
            return error(StatusCode::BAD_REQUEST, &e);
        }
    }
    let mut accounts = vec![];
    let mut errors = vec![];
    let mut renamed = std::collections::BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        match state.accounts.restore(record.clone()).await {
            Ok(account) => {
                if let Some(old) = record.get("id").and_then(Value::as_str) {
                    renamed.insert(old.to_owned(), account.id.clone());
                }
                accounts.push(account)
            }
            Err(e) => errors.push(json!({
                "index": index,
                "name": record.get("name").and_then(Value::as_str),
                "error": e.to_string(),
            })),
        }
    }
    let restored_clients = match clients {
        Some(list) => match state.clients.restore(list, &renamed, false) {
            Ok(count) => count,
            Err(e) => return error(StatusCode::BAD_REQUEST, &e),
        },
        None => 0,
    };
    state.refresh_models().await;
    let ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
    let worker = state.clone();
    tokio::spawn(async move {
        for id in ids {
            let _ = worker.quotas.refresh(&id).await;
        }
    });
    (
        if accounts.is_empty() && !errors.is_empty() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        },
        Json(json!({
            "restored": accounts.len(),
            "accounts": accounts,
            "errors": errors,
            "settings": apply_settings,
            "clients": restored_clients,
        })),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccountPatch {
    name: Option<String>,
    enabled: Option<bool>,
    /// Absent keeps the reserve; `null` or `0` removes it.
    #[serde(default, deserialize_with = "present")]
    reserve_percent: Option<Option<f64>>,
    /// Absent keeps the watched windows; `[]` watches the longest window.
    reserve_windows: Option<Vec<String>>,
    /// Custom endpoints only: model prefix, address, wire format (`null`
    /// forwards the client's own), served models (`[]` serves all) and key.
    route: Option<String>,
    base_url: Option<String>,
    #[serde(default, deserialize_with = "present_text")]
    dialect: Option<Option<String>>,
    models: Option<Vec<String>>,
    api_key: Option<String>,
}

fn present_text<'de, D: serde::Deserializer<'de>>(
    value: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(value).map(Some)
}

fn present<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<Option<f64>>, D::Error> {
    Option::<f64>::deserialize(value).map(Some)
}

pub async fn update_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<AccountPatch>,
) -> Response {
    let name = body.name.map(|name| name.trim().to_owned());
    if name.as_ref().is_some_and(|name| {
        name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control)
    }) {
        return error(
            StatusCode::BAD_REQUEST,
            "name must contain 1 to 120 characters without control characters",
        );
    }
    let reserve = body
        .reserve_percent
        .map(|reserve| reserve.filter(|percent| *percent != 0.0));
    if reserve
        .flatten()
        .is_some_and(|percent| !crate::subscriptions::valid_reserve(percent))
    {
        return error(
            StatusCode::BAD_REQUEST,
            &crate::subscriptions::SubscriptionError::InvalidReserve.to_string(),
        );
    }
    if body
        .reserve_windows
        .as_deref()
        .is_some_and(|kinds| crate::subscriptions::reserve_windows(kinds).is_none())
    {
        return error(
            StatusCode::BAD_REQUEST,
            &crate::subscriptions::SubscriptionError::InvalidReserveWindows.to_string(),
        );
    }
    let endpoint = crate::subscriptions::EndpointPatch {
        route: body.route,
        base_url: body.base_url,
        dialect: body.dialect,
        models: body.models,
        api_key: body.api_key,
    };
    let endpoint_change = endpoint.route.is_some()
        || endpoint.base_url.is_some()
        || endpoint.dialect.is_some()
        || endpoint.models.is_some()
        || endpoint.api_key.is_some();
    if name.is_none()
        && body.enabled.is_none()
        && reserve.is_none()
        && body.reserve_windows.is_none()
        && !endpoint_change
    {
        return error(
            StatusCode::BAD_REQUEST,
            "provide a name, enabled state, reserve or endpoint field",
        );
    }
    // The endpoint change is validated first, so a refused one leaves the
    // account untouched.
    if endpoint_change {
        if let Err(e) = state.accounts.update_endpoint(&id, endpoint).await {
            return error(
                match e {
                    crate::subscriptions::SubscriptionError::NotFound => StatusCode::NOT_FOUND,
                    crate::subscriptions::SubscriptionError::InvalidEndpoint(_) => {
                        StatusCode::BAD_REQUEST
                    }
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                },
                &e.to_string(),
            );
        }
        state.refresh_models().await;
    }
    let result = if name.is_none()
        && body.enabled.is_none()
        && reserve.is_none()
        && body.reserve_windows.is_none()
    {
        match state.accounts.get(&id).await {
            Some(account) => Ok(account.summary()),
            None => Err(crate::subscriptions::SubscriptionError::NotFound),
        }
    } else {
        state
            .accounts
            .update(&id, name, body.enabled, reserve, body.reserve_windows)
            .await
    };
    match result {
        Ok(a) => {
            let account = account_value(&state, a, time::OffsetDateTime::now_utc()).await;
            Json(json!({ "account": account })).into_response()
        }
        Err(e) => error(
            match e {
                crate::subscriptions::SubscriptionError::NotFound => StatusCode::NOT_FOUND,
                crate::subscriptions::SubscriptionError::InvalidReserve
                | crate::subscriptions::SubscriptionError::InvalidReserveWindows => {
                    StatusCode::BAD_REQUEST
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            &e.to_string(),
        ),
    }
}
pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.accounts.delete(&id).await {
        Ok(()) => {
            state.refresh_models().await;
            Json(json!({"deleted":true})).into_response()
        }
        Err(e) => error(StatusCode::NOT_FOUND, &e.to_string()),
    }
}
pub async fn quotas(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"quotas":state.quotas.list().await}))
}
pub async fn account_resets(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.resets.status(&id).await {
        Ok(status) => Json(json!(status)).into_response(),
        Err(error) => reset_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetConfirmation {
    #[serde(default)]
    confirmation: bool,
    request_id: String,
}

pub async fn redeem_reset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ResetConfirmation>,
) -> Response {
    if !body.confirmation {
        return error(
            StatusCode::BAD_REQUEST,
            "confirm before consuming a reset credit",
        );
    }
    match state.resets.redeem(&id, &body.request_id).await {
        Ok(result) => {
            let _ = state.quotas.refresh(&id).await;
            Json(json!(result)).into_response()
        }
        Err(error) => reset_error(error),
    }
}

fn reset_error(failure: crate::subscriptions::ResetError) -> Response {
    use crate::subscriptions::{ResetError, SubscriptionError};
    let status = match &failure {
        ResetError::InvalidRequest => StatusCode::BAD_REQUEST,
        ResetError::Pending | ResetError::MissingOrganization | ResetError::HistoryFull => {
            StatusCode::CONFLICT
        }
        ResetError::Cooldown => StatusCode::TOO_MANY_REQUESTS,
        ResetError::Subscription(SubscriptionError::NotFound) => StatusCode::NOT_FOUND,
        ResetError::Subscription(SubscriptionError::Unsupported) => StatusCode::BAD_REQUEST,
        ResetError::Subscription(SubscriptionError::InvalidImport(_)) => StatusCode::BAD_REQUEST,
        ResetError::Subscription(SubscriptionError::Storage) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_GATEWAY,
    };
    error(status, &failure.to_string())
}
pub async fn refresh_quotas(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.refresh_quotas(body["account_id"].as_str()).await;
    Json(json!({"quotas":state.quotas.list().await}))
}
pub async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    model_listing(&state).await
}
async fn model_listing(state: &AppState) -> Json<Value> {
    let models = state.models.read().await.clone();
    let listings = state
        .model_listings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(json!({"models":models,"listings":listings}))
}
#[derive(Default, Deserialize)]
pub struct RefreshModels {
    /// One pool (a provider or a custom endpoint route); every pool without it.
    pub pool: Option<String>,
}
pub async fn refresh_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RefreshModels>,
) -> Json<Value> {
    state.refresh_models_in(query.pool.as_deref()).await;
    model_listing(&state).await
}
pub async fn set_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(enabled) = body["enabled"].as_bool() else {
        return error(StatusCode::BAD_REQUEST, "enabled must be a boolean");
    };
    match set_models_enabled(&state, std::slice::from_ref(&id), enabled).await {
        Some(refusal) => refusal,
        None => Json(json!({"id":id,"enabled":enabled})).into_response(),
    }
}
#[derive(Deserialize)]
pub struct ModelsPatch {
    pub ids: Vec<String>,
    pub enabled: bool,
}
/// Turns several models on or off in one configuration write, such as a
/// whole provider from the Models page.
pub async fn set_models(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ModelsPatch>,
) -> Response {
    match set_models_enabled(&state, &body.ids, body.enabled).await {
        Some(refusal) => refusal,
        None => Json(json!({"ids":body.ids,"enabled":body.enabled})).into_response(),
    }
}
/// Saves the change, or returns the refusal to send.
async fn set_models_enabled(state: &AppState, ids: &[String], enabled: bool) -> Option<Response> {
    let _write = state.config_write.lock().await;
    let mut config = state.config.read().await.clone();
    {
        let models = state.models.read().await;
        if ids.iter().any(|id| !models.iter().any(|m| &m.id == id)) {
            return Some(error(StatusCode::NOT_FOUND, "model not found"));
        }
    }
    for id in ids {
        if enabled {
            config.disabled_models.remove(id);
        } else {
            config.disabled_models.insert(id.clone());
        }
    }
    if let Some(path) = &state.config_path
        && config.save(path).await.is_err()
    {
        return Some(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot save configuration",
        ));
    }
    *state.config.write().await = config;
    for model in state
        .models
        .write()
        .await
        .iter_mut()
        .filter(|m| ids.contains(&m.id))
    {
        model.enabled = enabled;
    }
    None
}
#[derive(Default, Deserialize)]
pub struct Params {
    pub limit: Option<usize>,
    pub provider: Option<String>,
    pub name: Option<String>,
    pub state: Option<String>,
    pub ids: Option<String>,
}
pub async fn public_models(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<Params>,
) -> Response {
    match state.clients.authenticate(&headers) {
        Ok(client) => Json(list_models(&state, &query, Some(&client)).await).into_response(),
        Err(refusal) => error(StatusCode::UNAUTHORIZED, refusal.message()),
    }
}
/// The management listing of client models: every model, as without keys.
pub async fn management_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<Params>,
) -> Json<Value> {
    Json(list_models(&state, &query, None).await)
}
async fn list_models(
    state: &AppState,
    query: &Params,
    client: Option<&crate::clients::Client>,
) -> Value {
    // A pool is a provider, or one custom endpoint route. A model shows
    // when an enabled account the client may use serves it.
    let ready: Vec<(String, Vec<String>)> = state
        .accounts
        .list()
        .await
        .into_iter()
        .filter(|a| a.enabled && client.is_none_or(|c| c.access.allows_summary(a)))
        .map(|a| {
            (
                a.route.unwrap_or_else(|| a.provider.as_str().to_owned()),
                a.models,
            )
        })
        .collect();
    let serves = |pool: &str, model: &str| {
        ready.iter().any(|(p, models)| {
            // Without a client the catalog already honours each
            // account's model list.
            p == pool
                && (client.is_none() || models.is_empty() || models.iter().any(|m| m == model))
        })
    };
    let upstream = query.ids.as_deref() == Some("upstream");
    let models = state.models.read().await;
    let mut listed: Vec<_> = models
        .iter()
        .filter(|m| {
            let pool = m.route.as_deref().unwrap_or(m.provider.as_str());
            m.enabled
                && serves(pool, &m.upstream_id)
                && query
                    .provider
                    .as_ref()
                    .is_none_or(|p| p.split(',').any(|p| p == pool || p == m.provider.as_str()))
        })
        .collect();
    if upstream {
        // The provider's own view: its ids, and its order. A client that
        // starts on the first model of a list, as the Grok CLI does, then
        // starts on the provider's default.
        listed.sort_by_key(|m| {
            (
                m.provider.as_str(),
                crate::state::catalog_rank(m.provider, &m.upstream_id),
            )
        });
    }
    let data: Vec<_> = listed
        .into_iter()
        .map(|m| public_model(m, upstream))
        .collect();
    json!({"object":"list","data":data})
}
/// One row of the client model list. `context_window` and `reasoning_efforts`
/// repeat `context_length` and `thinking.levels` under the names the Grok CLI
/// reads from an endpoint's list: without them it assumes a 256,000 token
/// window and offers no effort. `upstream` names the model as its provider
/// does, for a client that knows its own models by those names.
fn public_model(model: &crate::state::Model, upstream: bool) -> Value {
    let id = if upstream && !model.upstream_id.is_empty() {
        &model.upstream_id
    } else {
        &model.id
    };
    json!({"id":id,"object":"model","created":0,"owned_by":model.route.as_deref().unwrap_or(model.provider.as_str()),"name":model.name,"label":model.name,"context_length":model.context_length,"context_window":model.context_length,"max_output_tokens":model.max_output_tokens,"thinking":model.thinking,"reasoning_efforts":reasoning_efforts(model.thinking.as_ref()),"supported_endpoint_types":["anthropic","openai","openai-response"],"supports_websockets":true})
}
/// A model's effort levels as the Grok CLI lists them: highest first, `high`
/// flagged as the default when the model offers it. Null without levels.
fn reasoning_efforts(thinking: Option<&Value>) -> Option<Value> {
    let levels: Vec<&str> = thinking?
        .get("levels")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if levels.is_empty() {
        return None;
    }
    Some(
        levels
            .iter()
            .rev()
            .map(|level| {
                let label = match *level {
                    "xhigh" => "Extra high".to_owned(),
                    other => {
                        let mut letters = other.chars();
                        letters
                            .next()
                            .map(|first| first.to_uppercase().chain(letters).collect())
                            .unwrap_or_default()
                    }
                };
                let mut entry = json!({"value": level, "label": label});
                if *level == "high" {
                    entry["default"] = json!(true);
                }
                entry
            })
            .collect(),
    )
}
#[derive(Deserialize, Default)]
pub struct QuotaRead {
    /// `1` or `true`: the client's user asked for fresh numbers.
    refresh: Option<String>,
}

/// Longest a client waits for a refresh it asked for; slower reads land in
/// the next answer.
const CLIENT_REFRESH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn public_quotas(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(read): Query<QuotaRead>,
) -> Response {
    let client = match state.clients.authenticate(&headers) {
        Ok(client) => Some(client),
        Err(refusal) => return error(StatusCode::UNAUTHORIZED, refusal.message()),
    };
    if matches!(read.refresh.as_deref(), Some("1" | "true")) {
        // A read asked by hand: failures show at once, and a success clears
        // any shown error. The task outlives a timeout so no write is cut.
        let worker = state.clone();
        let task = tokio::spawn(async move { worker.refresh_quotas(None).await });
        let _ = tokio::time::timeout(CLIENT_REFRESH_WAIT, task).await;
    }
    let settings = state.config.read().await.settings.clone();
    let mut accounts = state.accounts.list().await;
    let mut snapshots = state.quotas.list().await;
    // A client sees its accounts, each window as the part of what it may
    // use that it has used.
    if let Some(client) = &client {
        accounts.retain(|a| client.access.allows_summary(a));
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        snapshots = snapshots
            .into_iter()
            .filter_map(|snapshot| {
                let account = accounts.iter().find(|a| a.id == snapshot.account_id)?;
                let pool = account
                    .route
                    .as_deref()
                    .unwrap_or(account.provider.as_str());
                Some(state.clients.client_view(
                    client,
                    snapshot,
                    pool,
                    now,
                    settings.free_before_reset_seconds(),
                ))
            })
            .collect();
    }
    Json(crate::quota_view::overview(
        accounts,
        snapshots,
        &settings,
        crate::subscriptions::now(),
    ))
    .into_response()
}

pub async fn clients(State(state): State<Arc<AppState>>) -> Json<Value> {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let list: Vec<Value> = state.clients.list().iter().map(|c| c.public()).collect();
    Json(json!({
        "clients": list,
        "usage": state.clients.usage_report(now),
    }))
}

#[derive(Deserialize)]
pub struct StatsPeriod {
    hours: Option<u32>,
}

/// Longest period the client statistics cover: 90 days.
const MAX_STATS_HOURS: u32 = 90 * 24;

/// What each client spent over the last `hours` (24 by default): totals by
/// provider and model, and calls and tokens by UTC hour.
pub async fn client_stats(
    State(state): State<Arc<AppState>>,
    Query(period): Query<StatsPeriod>,
) -> Json<Value> {
    let hours = period.hours.unwrap_or(24).clamp(1, MAX_STATS_HOURS);
    let since = crate::subscriptions::timestamp(
        time::OffsetDateTime::now_utc() - time::Duration::hours(hours.into()),
    );
    let stats = state.ledger.client_stats(since.clone()).await;
    Json(json!({"since": since, "totals": stats.totals, "hours": stats.hours}))
}

pub async fn create_client(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<crate::clients::ClientPatch>,
) -> Response {
    match state.clients.create(patch) {
        Ok((client, key)) => (
            StatusCode::CREATED,
            Json(json!({"client": client.public(), "key": key})),
        )
            .into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

pub async fn update_client(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(patch): Json<crate::clients::ClientPatch>,
) -> Response {
    match state.clients.update(&id, patch) {
        Ok((client, key)) => Json(json!({"client": client.public(), "key": key})).into_response(),
        Err(e) if e == "client not found" => error(StatusCode::NOT_FOUND, &e),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

pub async fn delete_client(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.clients.delete(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e == "client not found" => error(StatusCode::NOT_FOUND, &e),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}
pub async fn calls(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> Json<Value> {
    Json(json!({"calls":state.ledger.calls(q.limit.unwrap_or(100)).await}))
}
pub async fn summary(State(state): State<Arc<AppState>>) -> Json<Value> {
    let accounts = state.accounts.list().await;
    let l = &state.ledger;
    let client_api_base_url = state.config.read().await.server.client_api_base_url.clone();
    Json(
        json!({"accounts":accounts.len(),"enabled_accounts":accounts.iter().filter(|a|a.enabled).count(),"providers":accounts.iter().map(|a|a.provider).collect::<BTreeSet<_>>().len(),"requests":l.requests.load(Ordering::Relaxed),"errors":l.errors.load(Ordering::Relaxed),"input_tokens":l.input_tokens.load(Ordering::Relaxed),"output_tokens":l.output_tokens.load(Ordering::Relaxed),"cache_read_tokens":l.cache_read_tokens.load(Ordering::Relaxed),"cache_write_tokens":l.cache_write_tokens.load(Ordering::Relaxed),"in_flight":l.in_flight.load(Ordering::Relaxed),"dropped_logs":l.dropped_logs.load(Ordering::Relaxed),"uptime_seconds":l.started.elapsed().as_secs(),"version":env!("CARGO_PKG_VERSION"),"websocket":true,"client_api_base_url":client_api_base_url,"clients":state.clients.list().len()}),
    )
}
pub async fn settings(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!(state.config.read().await.settings))
}
pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> Response {
    let _write = state.config_write.lock().await;
    let mut config = state.config.read().await.clone();
    let mut value = serde_json::to_value(&config.settings).unwrap();
    let Some(fields) = patch.as_object() else {
        return error(StatusCode::BAD_REQUEST, "settings must be an object");
    };
    for (key, v) in fields {
        value[key] = v.clone();
    }
    let settings: crate::settings::Settings = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid settings"),
    };
    if let Err(e) = settings.validate() {
        return error(StatusCode::BAD_REQUEST, &e.to_string());
    }
    config.settings = settings.clone();
    if let Some(path) = &state.config_path {
        if config.save(path).await.is_err() {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot save configuration",
            );
        }
    }
    *state.config.write().await = config;
    Json(json!(settings)).into_response()
}
pub async fn providers(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut result = json!({"providers":[
        {"id":"claude","oauth":true,"quota":true,"docs_url":"https://claude.ai/settings/usage"},
        {"id":"codex","oauth":true,"quota":true,"docs_url":"https://chatgpt.com/codex/settings/usage"},
        {"id":"opencode_go","oauth":false,"quota":true,"docs_url":"https://opencode.ai/docs/go/"},
        {"id":"xai","oauth":true,"quota":true,"docs_url":"https://grok.com"},
        {"id":"copilot","oauth":true,"quota":true,"docs_url":"https://github.com/settings/copilot"},
        {"id":"meta","oauth":true,"quota":true,"docs_url":"https://muse.meta.com"},
        {"id":"antigravity","oauth":true,"quota":true,"docs_url":"https://antigravity.google"},
        {"id":"kimi","oauth":false,"quota":true,"docs_url":"https://kimi.com/code"},
        {"id":"qwen","oauth":false,"quota":false,"docs_url":"https://chat.qwen.ai"},
        {"id":"openai_compatible","oauth":false,"quota":false,"docs_url":"https://platform.openai.com"}
    ]});
    for provider in result["providers"].as_array_mut().unwrap() {
        let id = provider["id"]
            .as_str()
            .and_then(SubscriptionProvider::parse)
            .unwrap();
        provider["name"] = json!(crate::quota_view::provider_name(id));
        provider["oauth"] = json!(state.oauth.supports(id));
    }
    Json(result)
}
pub async fn oauth_start(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let Some(provider) = body["provider"]
        .as_str()
        .and_then(SubscriptionProvider::parse)
    else {
        return error(StatusCode::BAD_REQUEST, "unknown provider");
    };
    match state
        .oauth
        .start(provider, body["redirect_uri"].as_str().map(str::to_owned))
        .await
    {
        Ok(a) => Json(json!(a)).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}
pub async fn oauth_status(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.oauth.status(&id).await {
        Some(a) => {
            if a.status == "complete" {
                state.refresh_models().await;
                if let Some(id) = &a.account_id {
                    let worker = state.clone();
                    let id = id.clone();
                    tokio::spawn(async move {
                        let _ = worker.quotas.refresh(&id).await;
                    });
                }
            }
            Json(json!(a)).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "OAuth attempt not found"),
    }
}
pub async fn oauth_callback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    match state
        .oauth
        .complete(
            body["id"].as_str().unwrap_or(""),
            body["code"].as_str().unwrap_or(""),
            body["state"].as_str().unwrap_or(""),
        )
        .await
    {
        Ok(a) => {
            state.refresh_models().await;
            let worker = state.clone();
            let id = a.id.clone();
            tokio::spawn(async move {
                let _ = worker.quotas.refresh(&id).await;
            });
            Json(json!({"account":a})).into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

// CLIProxyAPI management discovery uses the same sanitized account data.
pub async fn auth_files(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(
        json!({"files":state.accounts.list().await.into_iter().map(|a|json!({"id":a.id,"auth_index":a.id,"name":format!("{}.json",a.id),"label":a.name,"provider":a.provider,"type":a.provider,"email":a.email,"disabled":!a.enabled,"status":a.status,"status_message":a.last_error,"created_at":a.created_at,"updated_at":a.updated_at})).collect::<Vec<_>>()}),
    )
}
pub async fn delete_auth_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Params>,
) -> Response {
    delete_account(
        State(state),
        Path(
            q.name
                .unwrap_or_default()
                .trim_end_matches(".json")
                .to_owned(),
        ),
    )
    .await
}
pub async fn patch_auth_file(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body["name"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches(".json")
        .to_owned();
    update_account(
        State(state),
        Path(id),
        Json(AccountPatch {
            name: None,
            enabled: Some(!body["disabled"].as_bool().unwrap_or(false)),
            ..AccountPatch::default()
        }),
    )
    .await
}
pub async fn compatible_config(State(state): State<Arc<AppState>>) -> Json<Value> {
    let config = state.config.read().await;
    Json(
        json!({"port":config.server.listen.rsplit(':').next().and_then(|p|p.parse::<u16>().ok()),"routing":{"strategy":config.settings.strategy},"access":{"api-keys":[]},"providers":[],"websocket-auth":false,"request-retry":1,"quota-exceeded":{"switch-project":true},"version":env!("CARGO_PKG_VERSION")}),
    )
}
pub async fn version() -> Json<Value> {
    Json(json!({"version":env!("CARGO_PKG_VERSION"),"latest-version":env!("CARGO_PKG_VERSION")}))
}
pub async fn auth_file_models(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Params>,
) -> Json<Value> {
    let id = q.name.unwrap_or_default();
    let account = state.accounts.get(id.trim_end_matches(".json")).await;
    let models: Vec<Value> = state
        .models
        .read()
        .await
        .iter()
        .filter(|m| account.as_ref().is_some_and(|a| a.provider == m.provider))
        .map(|m| json!({"id":m.id,"display_name":m.name,"context_length":m.context_length}))
        .collect();
    Json(json!({"models":models}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn account_listing_reports_reserve_state_per_quota_shape() {
        use crate::subscriptions::{QuotaSnapshot, QuotaWindow};
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = crate::settings::Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let window = |id: &str, remaining: f64, reset: &str| QuotaWindow {
            id: id.into(),
            label: id.into(),
            remaining_percent: Some(remaining),
            reset_at: Some(reset.into()),
            used: None,
            limit: Some(100.0),
        };
        let snapshot = |id: &str, windows: Vec<QuotaWindow>, error: Option<&str>| QuotaSnapshot {
            account_id: id.into(),
            provider: SubscriptionProvider::Claude,
            windows,
            updated_at: crate::subscriptions::now(),
            error: error.map(Into::into),
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        };
        let later = "2099-01-01T00:00:00Z";
        let past = "2020-01-01T00:00:00Z";
        // name, stored quota (windows, error), expected measured, expected reached
        let cases: Vec<(&str, Option<(Vec<QuotaWindow>, Option<&str>)>, Value, bool)> = vec![
            (
                "low",
                Some((vec![window("seven-day", 3.0, later)], None)),
                json!(true),
                true,
            ),
            (
                "five-hour-low",
                Some((
                    vec![
                        window("seven-day", 40.0, later),
                        window("five-hour", 0.0, later),
                    ],
                    None,
                )),
                json!(true),
                false,
            ),
            (
                "scoped",
                Some((vec![window("seven-day-opus", 0.0, later)], None)),
                json!(false),
                false,
            ),
            (
                "reset",
                Some((vec![window("seven-day", 1.0, past)], None)),
                json!(true),
                false,
            ),
            (
                "failed",
                Some((vec![], Some("provider request failed"))),
                Value::Null,
                false,
            ),
            ("unread", None, Value::Null, false),
        ];
        let mut ids = vec![];
        for (name, quota, _, _) in &cases {
            let account = state
                .accounts
                .import(
                    json!({"type":"claude","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            state
                .accounts
                .update(&account.id, None, None, Some(Some(5.0)), None)
                .await
                .unwrap();
            if let Some((windows, error)) = quota {
                state
                    .quotas
                    .insert_for_test(snapshot(&account.id, windows.clone(), *error))
                    .await;
            }
            ids.push(account.id);
        }
        let Json(body) = accounts(State(state.clone())).await;
        for ((name, _, measured, reached), id) in cases.iter().zip(&ids) {
            let row = body["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == *id)
                .unwrap();
            assert_eq!(row["reserve_measured"], *measured, "{name}");
            assert_eq!(row["reserve_reached"], *reached, "{name}");
        }
        // A change returns the same enriched shape as the listing.
        let response = update_account(
            State(state.clone()),
            Path(ids[0].clone()),
            Json(AccountPatch {
                name: Some("Renamed".into()),
                ..AccountPatch::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["account"]["name"], "Renamed");
        assert_eq!(body["account"]["reserve_measured"], true);
        assert_eq!(body["account"]["reserve_reached"], true);
    }
    #[tokio::test]
    async fn verification_redirect_exposes_challenge_only_as_no_store_location() {
        let challenge = "https://accounts.google.com/signin/continue?challenge=private-fixture";
        let response = verification_redirect(Some(challenge.into()));
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], challenge);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );

        let missing = verification_redirect(None);
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(!missing.headers().contains_key(header::LOCATION));
        assert_eq!(missing.headers()[header::CACHE_CONTROL], "no-store");
        let body = missing.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&body).contains("private-fixture"));
    }

    #[test]
    fn model_rows_carry_the_window_and_efforts_the_grok_cli_reads() {
        let model = |thinking: Option<Value>| crate::state::Model {
            id: "xai/grok-test".into(),
            provider: SubscriptionProvider::Xai,
            name: "Grok Test".into(),
            context_length: Some(500_000),
            max_output_tokens: None,
            enabled: true,
            upstream_id: "grok-test".into(),
            thinking,
            ..crate::state::Model::default()
        };
        let scaled = model(Some(
            json!({"levels":["none","low","high","xhigh"],"zero_allowed":true}),
        ));
        let row = public_model(&scaled, false);
        assert_eq!(row["id"], "xai/grok-test");
        assert_eq!(public_model(&scaled, true)["id"], "grok-test");
        assert_eq!(row["context_length"], 500_000);
        assert_eq!(row["context_window"], 500_000);
        assert_eq!(
            row["thinking"]["levels"],
            json!(["none", "low", "high", "xhigh"])
        );
        assert_eq!(
            row["reasoning_efforts"],
            json!([
                {"value":"xhigh","label":"Extra high"},
                {"value":"high","label":"High","default":true},
                {"value":"low","label":"Low"},
                {"value":"none","label":"None"}
            ])
        );
        // A model without a level scale offers no effort, and none is flagged
        // when `high` is not on the scale.
        for thinking in [None, Some(json!({"min":1024,"max":128000}))] {
            assert_eq!(
                public_model(&model(thinking), false)["reasoning_efforts"],
                Value::Null
            );
        }
        let unflagged = public_model(&model(Some(json!({"levels":["minimal","low"]}))), false);
        assert_eq!(
            unflagged["reasoning_efforts"],
            json!([{"value":"low","label":"Low"},{"value":"minimal","label":"Minimal"}])
        );
    }

    #[tokio::test]
    async fn a_provider_view_lists_its_own_ids_in_its_own_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::settings::Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        state
            .accounts
            .import(
                json!({"type":"xai","access_token":"fixture-access","account_id":"fixture-identity"}),
                None,
            )
            .await
            .unwrap();
        state.refresh_models().await;
        let ids = |ids: Option<&str>| {
            let query = Params {
                provider: Some("xai".into()),
                ids: ids.map(Into::into),
                ..Params::default()
            };
            let state = state.clone();
            async move {
                let Json(body) = management_models(State(state), Query(query)).await;
                body["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
        };
        let catalog: Vec<String> = crate::state::catalog()
            .iter()
            .filter(|model| model.provider == SubscriptionProvider::Xai)
            .map(|model| model.id.clone())
            .collect();
        assert!(catalog.len() > 1);
        assert_eq!(ids(Some("upstream")).await, catalog);
        let mut prefixed: Vec<String> = catalog.iter().map(|id| format!("xai/{id}")).collect();
        prefixed.sort();
        assert_eq!(ids(None).await, prefixed);
    }
}
