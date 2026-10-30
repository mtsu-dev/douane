//! Explicit, idempotent redemption of provider-granted rate-limit resets.

use super::{
    AccountCredential, AccountStore, OAuthManager, SubscriptionError, SubscriptionProvider,
    parse_timestamp, random_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, io::Write, path::Path, sync::Arc};
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum ResetError {
    #[error(transparent)]
    Subscription(#[from] SubscriptionError),
    #[error("reset request_id must be a UUID")]
    InvalidRequest,
    #[error("a previous reset is unconfirmed; retry its original request")]
    Pending,
    #[error("this Claude account has no organization identifier; reconnect it")]
    MissingOrganization,
    #[error("the provider could not confirm the reset; retry the same request")]
    Unconfirmed,
    #[error("the provider is cooling down resets; try again later")]
    Cooldown,
    #[error("the reset confirmation history is full; retained requests can still be retried")]
    HistoryFull,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResetStatus {
    pub account_id: String,
    pub provider: SubscriptionProvider,
    pub status: String,
    pub available_count: Option<u64>,
    pub next_expires_at: Option<String>,
    pub pending_request_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetOutcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResetRedemption {
    pub request_id: String,
    pub outcome: ResetOutcome,
}

// Grant IDs stay private, including through errors and management responses.
#[derive(Clone, Serialize, Deserialize)]
struct Attempt {
    request_id: String,
    credit_id: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct AccountAttempts {
    pending: Option<Attempt>,
    #[serde(default)]
    completed: BTreeMap<String, ResetOutcome>,
}

type Attempts = BTreeMap<String, AccountAttempts>;

struct Credit {
    id: String,
    expires_at: Option<String>,
    count: u64,
}

struct Reading {
    eligible: bool,
    count: u64,
    credits: Vec<Credit>,
}

/// The user confirms in the dashboard; the broker owns credit selection and retries.
pub struct ResetManager {
    client: reqwest::Client,
    accounts: Arc<AccountStore>,
    oauth: Arc<OAuthManager>,
    attempts: Mutex<Option<Attempts>>,
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    #[cfg(test)]
    endpoints: Option<String>,
}

impl ResetManager {
    pub fn new(
        accounts: Arc<AccountStore>,
        oauth: Arc<OAuthManager>,
    ) -> Result<Self, SubscriptionError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(25))
            .pool_idle_timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|_| SubscriptionError::Transport)?;
        Ok(Self {
            client,
            accounts,
            oauth,
            attempts: Mutex::new(None),
            locks: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            endpoints: None,
        })
    }

    /// Always probes the provider. Failed probes leave reset counts unknown.
    pub async fn status(&self, id: &str) -> Result<ResetStatus, ResetError> {
        let account = self
            .accounts
            .get(id)
            .await
            .ok_or(SubscriptionError::NotFound)?;
        let mut status = ResetStatus {
            account_id: id.into(),
            provider: account.provider,
            status: "unsupported".into(),
            available_count: None,
            next_expires_at: None,
            pending_request_id: None,
            error: None,
        };
        if !supported(account.provider) {
            return Ok(status);
        }
        let key = native_key(&account);
        let mut attempts = self.attempts.lock().await;
        self.load(&mut attempts)?;
        status.pending_request_id = attempts
            .as_ref()
            .and_then(|state| state.get(&key))
            .and_then(|state| state.pending.as_ref())
            .map(|attempt| attempt.request_id.clone());
        drop(attempts);
        if !account.enabled {
            status.status = "disabled".into();
            return Ok(status);
        }
        let result = match self.oauth.credential(id).await {
            Ok(account) => self.read(&account).await,
            Err(error) => Err(error.into()),
        };
        match result {
            Ok(reading) => {
                status.available_count = Some(reading.count);
                if account.provider == SubscriptionProvider::Codex
                    && reading
                        .credits
                        .iter()
                        .map(|credit| credit.count)
                        .sum::<u64>()
                        != reading.count
                {
                    status.status = "unavailable".into();
                    status.error =
                        Some("reset inventory is incomplete; refresh before using a credit".into());
                    return Ok(status);
                }
                status.status = if reading.eligible {
                    "ready"
                } else {
                    "ineligible"
                }
                .into();
                status.next_expires_at =
                    next_credit(&reading).and_then(|credit| credit.expires_at.clone());
            }
            Err(error) => {
                status.status = "unavailable".into();
                status.error = Some(error.to_string());
            }
        }
        Ok(status)
    }

    /// Repeated confirmations with one request ID can never consume another credit.
    pub async fn redeem(&self, id: &str, request_id: &str) -> Result<ResetRedemption, ResetError> {
        if !valid_request_id(request_id) {
            return Err(ResetError::InvalidRequest);
        }
        let mut account = self.oauth.credential(id).await?;
        if !supported(account.provider) {
            return Err(SubscriptionError::Unsupported.into());
        }
        if !account.enabled {
            return Err(SubscriptionError::InvalidImport("account is disabled").into());
        }
        if account.provider == SubscriptionProvider::Claude && account.organization_id.is_none() {
            account.organization_id = self.organization(&account).await?;
            let organization = account.organization_id.clone();
            self.accounts
                .modify(id, |current| current.organization_id = organization)
                .await?;
        }
        let key = native_key(&account);
        let lock = self
            .locks
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let previous = self.previous(&key, request_id).await?;
        let outcome = match previous {
            Previous::Completed(outcome) => outcome,
            Previous::Pending(attempt) => self.consume_and_settle(&account, &key, attempt).await?,
            Previous::New => {
                let reading = self.read(&account).await?;
                match next_credit(&reading) {
                    Some(credit) => {
                        // Selecting from incomplete details cannot prove earliest expiry.
                        if account.provider == SubscriptionProvider::Codex
                            && reading
                                .credits
                                .iter()
                                .map(|credit| credit.count)
                                .sum::<u64>()
                                != reading.count
                        {
                            return Err(SubscriptionError::InvalidResponse.into());
                        }
                        let attempt = Attempt {
                            request_id: request_id.into(),
                            credit_id: credit.id.clone(),
                        };
                        self.update(&key, |state| state.pending = Some(attempt.clone()))
                            .await?;
                        self.consume_and_settle(&account, &key, attempt).await?
                    }
                    None if reading.count == 0 => {
                        self.update(&key, |state| {
                            state
                                .completed
                                .insert(request_id.into(), ResetOutcome::NoCredit);
                        })
                        .await?;
                        ResetOutcome::NoCredit
                    }
                    None => return Err(SubscriptionError::InvalidResponse.into()),
                }
            }
        };
        Ok(ResetRedemption {
            request_id: request_id.into(),
            outcome,
        })
    }

    async fn previous(&self, key: &str, request_id: &str) -> Result<Previous, ResetError> {
        let mut state = self.attempts.lock().await;
        self.load(&mut state)?;
        let Some(account) = state.as_ref().and_then(|state| state.get(key)) else {
            return Ok(Previous::New);
        };
        if let Some(outcome) = account.completed.get(request_id) {
            return Ok(Previous::Completed(*outcome));
        }
        match &account.pending {
            Some(attempt) if attempt.request_id == request_id => {
                Ok(Previous::Pending(attempt.clone()))
            }
            Some(_) => Err(ResetError::Pending),
            None if account.completed.len() >= 1_024 => Err(ResetError::HistoryFull),
            None => Ok(Previous::New),
        }
    }

    fn load(&self, state: &mut Option<Attempts>) -> Result<(), ResetError> {
        if state.is_none() {
            let path = self.accounts.reset_state_path();
            *state = Some(match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(SubscriptionError::Storage.into());
                }
                Ok(_) => {
                    serde_json::from_slice(&fs::read(path).map_err(|_| SubscriptionError::Storage)?)
                        .map_err(|_| SubscriptionError::Storage)?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(_) => return Err(SubscriptionError::Storage.into()),
            });
        }
        Ok(())
    }

    async fn update(
        &self,
        key: &str,
        edit: impl FnOnce(&mut AccountAttempts),
    ) -> Result<(), ResetError> {
        let mut state = self.attempts.lock().await;
        self.load(&mut state)?;
        let current = state.as_mut().ok_or(SubscriptionError::Storage)?;
        // Keep the in-memory state unchanged if disk persistence fails.
        let mut changed = current.clone();
        edit(changed.entry(key.into()).or_default());
        persist(&self.accounts.reset_state_path(), &changed)?;
        *current = changed;
        Ok(())
    }

    async fn consume_and_settle(
        &self,
        account: &AccountCredential,
        key: &str,
        attempt: Attempt,
    ) -> Result<ResetOutcome, ResetError> {
        match self.consume(account, &attempt).await {
            Ok(outcome) => {
                self.update(key, |state| {
                    state.completed.insert(attempt.request_id.clone(), outcome);
                    state.pending = None;
                })
                .await?;
                Ok(outcome)
            }
            Err(error) => {
                if matches!(
                    &error,
                    ResetError::Cooldown
                        | ResetError::Subscription(SubscriptionError::Upstream(401 | 403 | 429))
                ) {
                    self.update(key, |state| state.pending = None).await?;
                }
                // Transport failures, server errors and malformed replies keep the claim.
                Err(error)
            }
        }
    }

    async fn read(&self, account: &AccountCredential) -> Result<Reading, ResetError> {
        let url = match account.provider {
            SubscriptionProvider::Claude => {
                self.url("https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1")
            }
            SubscriptionProvider::Codex => {
                self.url("https://chatgpt.com/backend-api/wham/rate-limit-reset-credits")
            }
            _ => return Err(SubscriptionError::Unsupported.into()),
        };
        let value = send_json(self.headers(self.client.get(url), account)).await?;
        match account.provider {
            SubscriptionProvider::Claude => parse_claude(&value),
            SubscriptionProvider::Codex => parse_codex(&value),
            _ => Err(SubscriptionError::Unsupported.into()),
        }
    }

    async fn organization(
        &self,
        account: &AccountCredential,
    ) -> Result<Option<String>, ResetError> {
        let value = send_json(
            self.headers(
                self.client
                    .get(self.url("https://api.anthropic.com/api/oauth/profile")),
                account,
            ),
        )
        .await?;
        let organization = value
            .get("organization")
            .and_then(|value| value.get("uuid").or_else(|| value.get("id")))
            .and_then(Value::as_str)
            .filter(|value| valid_organization(value))
            .map(str::to_owned);
        organization
            .map(Some)
            .ok_or(ResetError::MissingOrganization)
    }

    async fn consume(
        &self,
        account: &AccountCredential,
        attempt: &Attempt,
    ) -> Result<ResetOutcome, ResetError> {
        let (url, body) = match account.provider {
            SubscriptionProvider::Codex => (
                self.url("https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume"),
                json!({"redeem_request_id": attempt.request_id, "credit_id": attempt.credit_id}),
            ),
            SubscriptionProvider::Claude => {
                let organization = account
                    .organization_id
                    .as_deref()
                    .filter(|value| valid_organization(value))
                    .ok_or(ResetError::MissingOrganization)?;
                let mut url = reqwest::Url::parse("https://api.anthropic.com/api/organizations/")
                    .map_err(|_| SubscriptionError::InvalidResponse)?;
                url.path_segments_mut()
                    .map_err(|_| SubscriptionError::InvalidResponse)?
                    .pop_if_empty()
                    .push(organization)
                    .push("reset_rate_limits");
                (
                    self.url(url.as_str()),
                    json!({"program":"cedar_ember", "grant_id":attempt.credit_id, "request_id":attempt.request_id}),
                )
            }
            _ => return Err(SubscriptionError::Unsupported.into()),
        };
        let value = send_json(self.headers(self.client.post(url).json(&body), account)).await?;
        parse_outcome(account.provider, &value)
    }

    fn headers(
        &self,
        request: reqwest::RequestBuilder,
        account: &AccountCredential,
    ) -> reqwest::RequestBuilder {
        let request = request
            .bearer_auth(&account.access_token)
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(25));
        match account.provider {
            SubscriptionProvider::Codex => {
                let request = request.header("User-Agent", crate::relay::CODEX_USER_AGENT);
                if let Some(id) = &account.account_id {
                    request.header("ChatGPT-Account-Id", id)
                } else {
                    request
                }
            }
            SubscriptionProvider::Claude => request
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("User-Agent", crate::relay::CLAUDE_USER_AGENT),
            _ => request,
        }
    }

    fn url(&self, production: &str) -> String {
        #[cfg(test)]
        if let Some(base) = &self.endpoints {
            let url = reqwest::Url::parse(production).expect("provider constant URL");
            return format!(
                "{base}{}{}",
                url.path(),
                url.query()
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default()
            );
        }
        production.into()
    }
}

enum Previous {
    New,
    Pending(Attempt),
    Completed(ResetOutcome),
}

fn supported(provider: SubscriptionProvider) -> bool {
    matches!(
        provider,
        SubscriptionProvider::Claude | SubscriptionProvider::Codex
    )
}

fn native_key(account: &AccountCredential) -> String {
    let identity = if account.provider == SubscriptionProvider::Claude {
        account.organization_id.as_deref()
    } else {
        account.account_id.as_deref()
    };
    format!(
        "{}:{}",
        account.provider.as_str(),
        identity.or(account.email.as_deref()).unwrap_or(&account.id)
    )
}

fn valid_request_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn valid_organization(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn expiration(value: Option<&Value>) -> Result<Option<String>, ResetError> {
    match value {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if parse_timestamp(value).is_some() => Ok(Some(value.clone())),
        _ => Err(SubscriptionError::InvalidResponse.into()),
    }
}

fn next_credit(reading: &Reading) -> Option<&Credit> {
    reading
        .credits
        .iter()
        .filter(|credit| credit.count > 0)
        .min_by_key(|credit| {
            credit
                .expires_at
                .as_deref()
                .and_then(parse_timestamp)
                .map(|at| at.unix_timestamp())
                .unwrap_or(i64::MAX)
        })
}

fn parse_claude(value: &Value) -> Result<Reading, ResetError> {
    let ember = value
        .get("cedar_ember")
        .ok_or(SubscriptionError::InvalidResponse)?;
    let eligible = ember
        .get("eligible")
        .and_then(Value::as_bool)
        .ok_or(SubscriptionError::InvalidResponse)?;
    if !eligible {
        return Ok(Reading {
            eligible,
            count: 0,
            credits: Vec::new(),
        });
    }
    let rows = ember
        .get("grants")
        .and_then(Value::as_array)
        .ok_or(SubscriptionError::InvalidResponse)?;
    let mut credits = Vec::new();
    for row in rows {
        if row.get("usable_now").and_then(Value::as_bool) != Some(true)
            || row
                .get("paused")
                .is_some_and(|value| value != &Value::Bool(false))
        {
            continue;
        }
        let Some(count) = row
            .get("resets_left")
            .and_then(Value::as_u64)
            .filter(|count| *count > 0)
        else {
            continue;
        };
        let Some(id) = row.get("id").and_then(Value::as_str).filter(|id| {
            !id.is_empty()
                && id.len() <= 40
                && id.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'
                        || byte == b'-'
                })
        }) else {
            continue;
        };
        let Ok(expires_at) = expiration(row.get("ends_at")) else {
            continue;
        };
        if expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|at| at <= time::OffsetDateTime::now_utc())
        {
            continue;
        }
        credits.push(Credit {
            id: id.into(),
            expires_at,
            count,
        });
    }
    let count = credits
        .iter()
        .try_fold(0u64, |sum, credit| sum.checked_add(credit.count))
        .ok_or(SubscriptionError::InvalidResponse)?;
    Ok(Reading {
        eligible,
        count,
        credits,
    })
}

fn parse_codex(value: &Value) -> Result<Reading, ResetError> {
    let count = value
        .get("available_count")
        .and_then(Value::as_u64)
        .ok_or(SubscriptionError::InvalidResponse)?;
    let rows = value
        .get("credits")
        .and_then(Value::as_array)
        .ok_or(SubscriptionError::InvalidResponse)?;
    let mut credits = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        if row.get("status").and_then(Value::as_str) != Some("available")
            || row.get("reset_type").and_then(Value::as_str) != Some("codex_rate_limits")
        {
            continue;
        }
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty() && id.len() <= 512)
        else {
            continue;
        };
        let Ok(expires_at) = expiration(row.get("expires_at")) else {
            continue;
        };
        if expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|at| at <= time::OffsetDateTime::now_utc())
        {
            continue;
        }
        if seen.insert(id) {
            credits.push(Credit {
                id: id.into(),
                expires_at,
                count: 1,
            });
        }
    }
    Ok(Reading {
        eligible: true,
        count,
        credits,
    })
}

fn parse_outcome(
    provider: SubscriptionProvider,
    value: &Value,
) -> Result<ResetOutcome, ResetError> {
    let code = value
        .get(if provider == SubscriptionProvider::Claude {
            "result"
        } else {
            "code"
        })
        .and_then(Value::as_str);
    match code {
        Some("reset") => Ok(ResetOutcome::Reset),
        Some("nothing_to_reset" | "not_limited") => Ok(ResetOutcome::NothingToReset),
        Some("no_credit" | "ineligible") => Ok(ResetOutcome::NoCredit),
        Some("already_redeemed" | "already_used") => Ok(ResetOutcome::AlreadyRedeemed),
        Some("cooldown") => Err(ResetError::Cooldown),
        _ => Err(ResetError::Unconfirmed),
    }
}

async fn send_json(request: reqwest::RequestBuilder) -> Result<Value, ResetError> {
    let response = request
        .send()
        .await
        .map_err(|_| SubscriptionError::Transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(SubscriptionError::Upstream(status.as_u16()).into());
    }
    response
        .json()
        .await
        .map_err(|_| SubscriptionError::InvalidResponse.into())
}

fn persist(path: &Path, state: &Attempts) -> Result<(), ResetError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(SubscriptionError::Storage.into());
    }
    let temporary = path.with_file_name(format!(".reset-{}.tmp", random_id()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| SubscriptionError::Storage)?;
        file.write_all(&serde_json::to_vec(state).map_err(|_| SubscriptionError::Storage)?)
            .map_err(|_| SubscriptionError::Storage)?;
        file.sync_all().map_err(|_| SubscriptionError::Storage)?;
        drop(file);
        fs::rename(&temporary, path).map_err(|_| SubscriptionError::Storage)?;
        #[cfg(unix)]
        {
            fs::File::open(path.parent().ok_or(SubscriptionError::Storage)?)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| SubscriptionError::Storage)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const REQUEST: &str = "dc1d4f50-18e0-4878-a714-0031d1971205";
    const OTHER: &str = "997e0ad5-8a39-496f-bf3e-09ad707e2701";

    #[derive(Default)]
    struct Backend {
        posts: Mutex<Vec<Value>>,
        reads: AtomicUsize,
        fail_first: AtomicBool,
        unavailable: AtomicBool,
        partial: AtomicBool,
    }

    async fn codex_read(
        State(state): State<Arc<Backend>>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        assert_eq!(
            headers.get("chatgpt-account-id").unwrap(),
            "native-fixture-account"
        );
        state.reads.fetch_add(1, Ordering::SeqCst);
        if state.unavailable.load(Ordering::SeqCst) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"secret":"never echo this provider response"})),
            );
        }
        let credits = if state.partial.load(Ordering::SeqCst) {
            json!([{"id":"private-early-credit", "status":"available", "reset_type":"codex_rate_limits", "expires_at":"2098-01-01T00:00:00Z"}])
        } else {
            json!([
                {"id":"private-late-credit", "status":"available", "reset_type":"codex_rate_limits", "expires_at":"2099-01-01T00:00:00Z"},
                {"id":"private-early-credit", "status":"available", "reset_type":"codex_rate_limits", "expires_at":"2098-01-01T00:00:00Z"}
            ])
        };
        (
            StatusCode::OK,
            Json(json!({"available_count":2, "credits":credits})),
        )
    }

    async fn codex_consume(
        State(state): State<Arc<Backend>>,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        let mut posts = state.posts.lock().await;
        posts.push(body);
        if posts.len() == 1 && state.fail_first.load(Ordering::SeqCst) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"secret":"private-upstream-error"})),
            );
        }
        (
            StatusCode::OK,
            Json(
                json!({"code":if posts.len() > 1 {"already_redeemed"} else {"reset"}, "windows_reset":2}),
            ),
        )
    }

    async fn fixture(
        state: Arc<Backend>,
        provider: SubscriptionProvider,
    ) -> (
        tempfile::TempDir,
        Arc<AccountStore>,
        Arc<OAuthManager>,
        ResetManager,
        String,
        tokio::task::JoinHandle<()>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let accounts = Arc::new(AccountStore::open(directory.path().join("accounts")).unwrap());
        let account = accounts
            .import(
                json!({"provider":provider.as_str(), "access_token":"reset-fixture-token",
            "account_id":"native-fixture-account", "organization_id":"fixture-organization"}),
                None,
            )
            .await
            .unwrap();
        let oauth = Arc::new(OAuthManager::new(reqwest::Client::new(), accounts.clone()));
        let mut manager = ResetManager::new(accounts.clone(), oauth.clone()).unwrap();
        let router = Router::new()
            .route("/backend-api/wham/rate-limit-reset-credits", get(codex_read))
            .route("/backend-api/wham/rate-limit-reset-credits/consume", post(codex_consume))
            .route("/api/oauth/usage", get(|| async { Json(json!({"cedar_ember":{"eligible":true,"grants":[
                {"id":"claude_early", "resets_left":1, "usable_now":true, "ends_at":"2098-01-01T00:00:00Z"},
                {"id":"claude_late", "resets_left":1, "usable_now":true, "ends_at":null}
            ]}})) }))
            .route("/api/organizations/fixture-organization/reset_rate_limits", post(|State(state): State<Arc<Backend>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers.get("anthropic-beta").unwrap(), "oauth-2025-04-20");
                state.posts.lock().await.push(body);
                Json(json!({"result":"reset"}))
            }))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        manager.endpoints = Some(format!("http://{}", listener.local_addr().unwrap()));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (directory, accounts, oauth, manager, account.id, task)
    }

    #[tokio::test]
    async fn duplicate_confirmation_consumes_once_including_after_restart() {
        let backend = Arc::new(Backend::default());
        let (_directory, accounts, oauth, manager, id, task) =
            fixture(backend.clone(), SubscriptionProvider::Codex).await;
        let (first, repeated) =
            tokio::join!(manager.redeem(&id, REQUEST), manager.redeem(&id, REQUEST));
        assert_eq!(first.unwrap().outcome, ResetOutcome::Reset);
        assert_eq!(repeated.unwrap().outcome, ResetOutcome::Reset);
        let mut restored = ResetManager::new(accounts.clone(), oauth).unwrap();
        restored.endpoints = manager.endpoints.clone();
        assert_eq!(
            restored.redeem(&id, REQUEST).await.unwrap().outcome,
            ResetOutcome::Reset
        );
        let posts = backend.posts.lock().await;
        assert_eq!(posts.len(), 1);
        assert_eq!(
            posts[0],
            json!({"redeem_request_id":REQUEST, "credit_id":"private-early-credit"})
        );
        let file = fs::read_to_string(accounts.reset_state_path()).unwrap();
        assert!(!file.contains("reset-fixture-token"));
        assert!(AccountStore::open(accounts.reset_state_path().parent().unwrap()).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(accounts.reset_state_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        task.abort();
    }

    #[tokio::test]
    async fn unanswered_claim_reuses_credit_and_request_after_restart() {
        let backend = Arc::new(Backend::default());
        backend.fail_first.store(true, Ordering::SeqCst);
        let (_directory, accounts, oauth, manager, id, task) =
            fixture(backend.clone(), SubscriptionProvider::Codex).await;
        let error = manager.redeem(&id, REQUEST).await.unwrap_err();
        assert!(!error.to_string().contains("private-upstream-error"));
        assert!(matches!(
            manager.redeem(&id, OTHER).await,
            Err(ResetError::Pending)
        ));
        let status = manager.status(&id).await.unwrap();
        assert_eq!(status.pending_request_id.as_deref(), Some(REQUEST));
        let mut restored = ResetManager::new(accounts, oauth).unwrap();
        restored.endpoints = manager.endpoints.clone();
        assert_eq!(
            restored.redeem(&id, REQUEST).await.unwrap().outcome,
            ResetOutcome::AlreadyRedeemed
        );
        let posts = backend.posts.lock().await;
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0], posts[1]);
        assert_eq!(backend.reads.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn disabled_account_reports_disabled_without_error_or_probe() {
        let backend = Arc::new(Backend::default());
        let (_directory, accounts, _oauth, manager, id, task) =
            fixture(backend.clone(), SubscriptionProvider::Codex).await;
        accounts
            .modify(&id, |account| account.enabled = false)
            .await
            .unwrap();
        let status = manager.status(&id).await.unwrap();
        assert_eq!(status.status, "disabled");
        assert_eq!(status.error, None);
        assert_eq!(backend.reads.load(Ordering::SeqCst), 0);
        task.abort();
    }

    #[tokio::test]
    async fn unavailable_reset_inventory_is_unknown_and_partial_inventory_cannot_spend() {
        let backend = Arc::new(Backend::default());
        let (_directory, _accounts, _oauth, manager, id, task) =
            fixture(backend.clone(), SubscriptionProvider::Codex).await;
        backend.unavailable.store(true, Ordering::SeqCst);
        let status = manager.status(&id).await.unwrap();
        assert_eq!(status.available_count, None);
        assert_eq!(status.status, "unavailable");
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("never echo")
        );
        backend.unavailable.store(false, Ordering::SeqCst);
        backend.partial.store(true, Ordering::SeqCst);
        let partial = manager.status(&id).await.unwrap();
        assert_eq!(partial.available_count, Some(2));
        assert_eq!(partial.status, "unavailable");
        assert_eq!(partial.next_expires_at, None);
        assert!(manager.redeem(&id, REQUEST).await.is_err());
        assert!(backend.posts.lock().await.is_empty());
        task.abort();
    }

    #[tokio::test]
    async fn claude_claim_uses_native_organization_and_private_earliest_grant() {
        let backend = Arc::new(Backend::default());
        let (_directory, _accounts, _oauth, manager, id, task) =
            fixture(backend.clone(), SubscriptionProvider::Claude).await;
        let status = manager.status(&id).await.unwrap();
        assert_eq!(status.available_count, Some(2));
        assert_eq!(
            status.next_expires_at.as_deref(),
            Some("2098-01-01T00:00:00Z")
        );
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("claude_early")
        );
        assert_eq!(
            manager.redeem(&id, REQUEST).await.unwrap().outcome,
            ResetOutcome::Reset
        );
        assert_eq!(
            backend.posts.lock().await[0],
            json!({"program":"cedar_ember", "grant_id":"claude_early", "request_id":REQUEST})
        );
        task.abort();
    }

    #[test]
    fn unusable_and_expired_claude_grants_never_count() {
        let reading = parse_claude(&json!({"cedar_ember":{"eligible":true,"grants":[
            {"id":"expired", "resets_left":1, "usable_now":true,"ends_at":"2000-01-01T00:00:00Z"},
            {"id":"paused", "resets_left":3, "usable_now":true,"paused":true,"ends_at":null},
            {"id":"unusable", "resets_left":2, "usable_now":false,"ends_at":null},
            {"id":"missing_date", "resets_left":1, "usable_now":true},
            {"id":"invalid_date", "resets_left":1, "usable_now":true,"ends_at":"2098-02-30T00:00:00Z"},
            {"id":"usable", "resets_left":2, "usable_now":true,"ends_at":null}
        ]}})).unwrap();
        assert_eq!(reading.count, 2);
        assert_eq!(next_credit(&reading).unwrap().id, "usable");
        assert!(
            parse_outcome(
                SubscriptionProvider::Claude,
                &json!({"result":"unavailable"})
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn full_history_replays_old_confirmation_and_refuses_fresh_spend() {
        let backend = Arc::new(Backend::default());
        let (_directory, accounts, _oauth, manager, id, task) =
            fixture(backend.clone(), SubscriptionProvider::Codex).await;
        let account = accounts.get(&id).await.unwrap();
        let key = native_key(&account);
        manager
            .update(&key, |state| {
                state
                    .completed
                    .insert(REQUEST.into(), ResetOutcome::NoCredit);
                for index in 1..1024 {
                    state.completed.insert(
                        format!("{index:08x}-0000-4000-8000-000000000000"),
                        ResetOutcome::NoCredit,
                    );
                }
            })
            .await
            .unwrap();
        assert_eq!(
            manager.redeem(&id, REQUEST).await.unwrap().outcome,
            ResetOutcome::NoCredit
        );
        assert!(matches!(
            manager.redeem(&id, OTHER).await,
            Err(ResetError::HistoryFull)
        ));
        assert!(backend.posts.lock().await.is_empty());
        assert_eq!(backend.reads.load(Ordering::SeqCst), 0);
        task.abort();
    }
}
