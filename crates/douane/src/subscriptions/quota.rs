use super::oauth::send_json;
use super::store::{subscription_from_value, text};
use super::{
    AccountCredential, AccountStore, OAuthManager, SubscriptionError, SubscriptionProvider,
    SubscriptionTier, now, parse_timestamp, timestamp,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::RwLock;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<String>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub account_id: String,
    pub provider: SubscriptionProvider,
    pub windows: Vec<QuotaWindow>,
    pub updated_at: String,
    pub error: Option<String>,
    #[serde(default)]
    pub subscription: Option<SubscriptionTier>,
    #[serde(default)]
    pub credits: Vec<QuotaCredits>,
    #[serde(default)]
    pub verification_required: bool,
    #[serde(default)]
    pub verification_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaCredits {
    pub id: String,
    pub label: String,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub unit: String,
    pub reset_at: Option<String>,
    pub unlimited: Option<bool>,
    pub enabled: Option<bool>,
}

struct Observation {
    windows: Vec<QuotaWindow>,
    plan: Option<String>,
    subscription: Option<SubscriptionTier>,
    credits: Vec<QuotaCredits>,
    organization_id: Option<String>,
}

impl QuotaSnapshot {
    pub fn weekly_reset(&self) -> Option<&str> {
        self.windows
            .iter()
            .filter(|window| is_weekly(&window.id))
            .filter_map(|window| window.reset_at.as_deref())
            .filter(|reset| parse_timestamp(reset).is_some())
            .min_by_key(|reset| parse_timestamp(reset))
    }

    /// The longest account-wide window, when it lasts a week or more, resets
    /// within `seconds` after `now`: what is left of it is lost unless spent.
    /// Windows count as the default reserve counts them, `window-<n>s` included.
    pub fn longest_window_ends_within(&self, seconds: i64, now: i64) -> bool {
        if seconds <= 0 {
            return false;
        }
        let windows = || {
            self.windows
                .iter()
                .filter_map(|window| Some((window, window_seconds(&window.id)?)))
        };
        let Some(longest) = windows().map(|(_, length)| length).reduce(f64::max) else {
            return false;
        };
        longest >= 604_800.0
            && windows()
                .filter(|(_, length)| *length == longest)
                .filter_map(|(window, _)| window.reset_at.as_deref().and_then(parse_timestamp))
                .map(|reset| reset.unix_timestamp())
                .any(|reset| reset > now && reset - now <= seconds)
    }

    pub fn remaining_percent(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter(|window| is_global(&window.id))
            .filter_map(|window| window.remaining_percent)
            .reduce(f64::min)
    }

    pub fn weekly_remaining_percent(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter(|window| is_weekly(&window.id))
            .filter_map(|window| window.remaining_percent)
            .reduce(f64::min)
    }

    /// The provider reported a paid balance that keeps serving requests once
    /// the subscription windows are empty: Codex credits or Claude extra usage.
    /// Only an explicit `enabled: true` counts; an unknown balance on an
    /// enabled pool is the provider's to refuse, a known zero is not usable.
    pub fn usable_credits(&self) -> bool {
        self.credits.iter().any(|credits| {
            credits.enabled == Some(true)
                && (credits.unlimited == Some(true)
                    || credits.remaining.is_none_or(|remaining| remaining > 0.0))
        })
    }

    /// A subscription-wide window is empty and has not reset yet.
    pub fn window_exhausted(&self, now: time::OffsetDateTime) -> bool {
        self.windows.iter().any(|window| {
            is_global(&window.id)
                && window
                    .remaining_percent
                    .is_some_and(|percent| percent <= 0.0)
                && window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_none_or(|reset| reset > now)
        })
    }

    /// Requests would be paid from credits rather than the subscription.
    pub fn credits_only(&self, now: time::OffsetDateTime) -> bool {
        self.window_exhausted(now) && self.usable_credits()
    }

    /// A watched window, not yet reset, is at or below the account's reserve.
    /// With no `windows` chosen, only the longest window (monthly over weekly
    /// over daily over five-hour) is watched; otherwise each chosen kind is.
    /// Model-scoped windows and unknown values never count.
    pub fn reserve_reached(
        &self,
        reserve: f64,
        windows: &[String],
        now: time::OffsetDateTime,
    ) -> bool {
        self.reserve_window_remaining(windows, now)
            .is_some_and(|remaining| remaining <= reserve)
    }

    /// Lowest remaining share among the watched windows, or `None` when they
    /// are unknown or have reset since the read.
    pub fn reserve_window_remaining(
        &self,
        windows: &[String],
        now: time::OffsetDateTime,
    ) -> Option<f64> {
        let known = || {
            self.windows
                .iter()
                .filter(|window| window.remaining_percent.is_some_and(f64::is_finite))
                .filter_map(|window| Some((window, window_seconds(&window.id)?)))
        };
        let longest = if windows.is_empty() {
            Some(known().map(|(_, seconds)| seconds).reduce(f64::max)?)
        } else {
            None
        };
        known()
            .filter(|(window, seconds)| match longest {
                Some(longest) => *seconds == longest,
                None => {
                    reserve_kind(&window.id).is_some_and(|kind| windows.iter().any(|w| w == kind))
                }
            })
            .filter(|(window, _)| {
                window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_none_or(|reset| reset > now)
            })
            .filter_map(|(window, _)| window.remaining_percent)
            .reduce(f64::min)
    }

    /// The provider reports a known share for a window the reserve watches:
    /// any account-wide window by default, or one of the chosen kinds.
    pub fn reports_reserve_window(&self, windows: &[String]) -> bool {
        if windows.is_empty() {
            return self.reports_account_window();
        }
        self.windows.iter().any(|window| {
            window.remaining_percent.is_some_and(f64::is_finite)
                && reserve_kind(&window.id).is_some_and(|kind| windows.iter().any(|w| w == kind))
        })
    }

    /// The provider reports a known share for at least one account-wide
    /// window, whether or not that window has reset since the last read.
    pub fn reports_account_window(&self) -> bool {
        self.windows.iter().any(|window| {
            is_global(&window.id)
                && window
                    .remaining_percent
                    .is_some_and(|value| value.is_finite())
        })
    }
}

/// Who asked for a quota read. A read asked by hand reports its failure at
/// once; the background poller stays quiet about transient failures until
/// [`FAILURES_SHOWN`] of them in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Manual,
    Background,
}

/// Consecutive failed reads before a transient error reaches the snapshot.
pub const FAILURES_SHOWN: u32 = 5;
const RETRY_FIRST_SECONDS: i64 = 30;
const RETRY_MAX_SECONDS: i64 = 30 * 60;

/// Wait after the `count`-th consecutive failure: 30 s, doubling, capped at
/// 30 minutes. The fifth failure lands about 7.5 minutes after the first.
pub fn retry_delay(count: u32) -> time::Duration {
    let doublings = count.saturating_sub(1).min(16);
    time::Duration::seconds((RETRY_FIRST_SECONDS << doublings).min(RETRY_MAX_SECONDS))
}

#[derive(Clone, Copy, Debug)]
struct Failures {
    count: u32,
    retry_at: time::OffsetDateTime,
}

pub struct QuotaService {
    client: reqwest::Client,
    store: Arc<AccountStore>,
    oauth: Arc<OAuthManager>,
    cache: RwLock<BTreeMap<String, QuotaSnapshot>>,
    save_lock: tokio::sync::Mutex<()>,
    verification_urls: RwLock<BTreeMap<String, String>>,
    failures: RwLock<BTreeMap<String, Failures>>,
    forecast: RwLock<super::forecast::History>,
    /// GitHub's Copilot entitlement endpoint; tests point it at a loopback.
    pub(crate) copilot_user_url: String,
}

impl QuotaService {
    pub fn new(client: reqwest::Client, store: Arc<AccountStore>) -> Self {
        let oauth = Arc::new(OAuthManager::new(client.clone(), store.clone()));
        Self::with_oauth(client, store, oauth)
    }

    pub fn with_oauth(
        client: reqwest::Client,
        store: Arc<AccountStore>,
        oauth: Arc<OAuthManager>,
    ) -> Self {
        let cache = load_snapshots(&store.quota_snapshot_path());
        let forecast = std::fs::read(
            store
                .quota_snapshot_path()
                .with_file_name(".quota-forecast"),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
        Self {
            client,
            store,
            oauth,
            cache: RwLock::new(cache),
            save_lock: tokio::sync::Mutex::new(()),
            verification_urls: RwLock::new(BTreeMap::new()),
            failures: RwLock::new(BTreeMap::new()),
            forecast: RwLock::new(forecast),
            copilot_user_url: "https://api.github.com/copilot_internal/user".into(),
        }
    }

    /// Writes the snapshots next to the accounts, so the last numbers outlive
    /// a restart: a provider refusing quota reads still has something to show.
    pub async fn save(&self) {
        let _guard = self.save_lock.lock().await;
        let snapshots = self.list().await;
        let mut forecast = self.forecast.write().await;
        forecast.retain(&snapshots);
        let forecast_bytes = serde_json::to_vec(&*forecast).ok();
        drop(forecast);
        if let Some(bytes) = forecast_bytes {
            let path = self
                .store
                .quota_snapshot_path()
                .with_file_name(".quota-forecast");
            if !matches!(
                tokio::task::spawn_blocking(move || write_private(&path, &bytes)).await,
                Ok(Ok(()))
            ) {
                tracing::warn!("quota forecast history was not saved");
            }
        }
        let path = self.store.quota_snapshot_path();
        let Ok(bytes) = serde_json::to_vec(&*self.cache.read().await) else {
            return;
        };
        if let Err(error) = tokio::task::spawn_blocking(move || write_private(&path, &bytes))
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("save task failed")))
        {
            tracing::warn!(%error, "quota snapshots were not saved");
        }
    }

    pub async fn list(&self) -> Vec<QuotaSnapshot> {
        let accounts: std::collections::BTreeSet<_> = self
            .store
            .list()
            .await
            .into_iter()
            .map(|account| account.id)
            .collect();
        self.cache
            .read()
            .await
            .values()
            .filter(|snapshot| accounts.contains(&snapshot.account_id))
            .cloned()
            .collect()
    }

    pub async fn forecasts(
        &self,
        snapshots: &[QuotaSnapshot],
    ) -> Vec<super::forecast::WindowForecast> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut forecast = self.forecast.write().await;
        forecast.retain(snapshots);
        forecast.report(snapshots, now)
    }

    #[cfg(test)]
    pub(crate) async fn insert_for_test(&self, snapshot: QuotaSnapshot) {
        self.cache
            .write()
            .await
            .insert(snapshot.account_id.clone(), snapshot);
    }

    pub async fn get(&self, id: &str) -> Option<QuotaSnapshot> {
        self.cache.read().await.get(id).cloned()
    }

    pub async fn verification_url(&self, id: &str) -> Option<String> {
        if self.store.get(id).await.is_none() {
            self.verification_urls.write().await.remove(id);
            return None;
        }
        self.verification_urls.read().await.get(id).cloned()
    }

    /// A read asked by hand: it always reaches the provider and a failure
    /// shows at once.
    pub async fn refresh(&self, id: &str) -> Result<QuotaSnapshot, SubscriptionError> {
        self.refresh_with(id, Trigger::Manual).await
    }

    /// A read nobody asked for: skipped while the account waits for its next
    /// retry, `None` then.
    pub async fn refresh_background(
        &self,
        id: &str,
    ) -> Option<Result<QuotaSnapshot, SubscriptionError>> {
        let now = time::OffsetDateTime::now_utc();
        if self
            .failures
            .read()
            .await
            .get(id)
            .is_some_and(|failures| failures.retry_at > now)
        {
            return None;
        }
        Some(self.refresh_with(id, Trigger::Background).await)
    }

    /// Claude answers every inference call with its five-hour and weekly
    /// usage. Those headers keep both windows current between usage reads,
    /// and the poller waits a full interval after them, so an account in use
    /// rarely needs the usage endpoint Anthropic can refuse with 429.
    pub async fn observe_claude_headers(
        &self,
        account: &AccountCredential,
        headers: &reqwest::header::HeaderMap,
    ) {
        if account.provider != SubscriptionProvider::Claude {
            return;
        }
        let windows = claude_header_windows(headers);
        if windows.is_empty() {
            return;
        }
        {
            let mut cache = self.cache.write().await;
            let snapshot = cache
                .entry(account.id.clone())
                .or_insert_with(|| QuotaSnapshot {
                    account_id: account.id.clone(),
                    provider: account.provider,
                    windows: Vec::new(),
                    updated_at: now(),
                    error: None,
                    subscription: account.summary().subscription,
                    credits: Vec::new(),
                    verification_required: false,
                    verification_available: false,
                });
            for (position, window) in windows.iter().cloned().enumerate() {
                match snapshot.windows.iter_mut().find(|w| w.id == window.id) {
                    Some(existing) => {
                        existing.remaining_percent = window.remaining_percent;
                        existing.used = window.used;
                        existing.reset_at = window.reset_at;
                    }
                    None => snapshot
                        .windows
                        .insert(position.min(snapshot.windows.len()), window),
                }
            }
            snapshot.updated_at = now();
            snapshot.error = None;
            self.forecast.write().await.record_claude_headers(
                snapshot,
                &windows,
                time::OffsetDateTime::now_utc().unix_timestamp(),
            );
        }
        self.failures.write().await.remove(&account.id);
    }

    /// Muse reports subscription use in inference SSE and in key mint replies.
    /// Never send a synthetic inference to poll it.
    pub(crate) async fn observe_meta_event(&self, id: &str, event: &Value) {
        self.observe_meta_event_at(id, event, time::OffsetDateTime::now_utc())
            .await;
    }

    async fn observe_meta_event_at(&self, id: &str, event: &Value, observed: time::OffsetDateTime) {
        if !super::meta::subscription_event(event) {
            return;
        }
        let Some(account) = self
            .store
            .get(id)
            .await
            .filter(|a| a.provider == SubscriptionProvider::Meta)
        else {
            return;
        };
        let mut windows = Vec::new();
        for (key, default_id, label) in [
            ("weekly", "weekly", "Weekly"),
            ("window", "session", "Session"),
        ] {
            let source = &event["subscription"][key];
            let Some(used) = source["used_percent"]
                .as_f64()
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
            else {
                continue;
            };
            let reset = source["resets_at"]
                .as_i64()
                .and_then(|n| time::OffsetDateTime::from_unix_timestamp(n).ok())
                .map(timestamp);
            let id = if key == "window" {
                source["window_duration_mins"]
                    .as_u64()
                    .filter(|n| *n > 0 && *n < 525600)
                    .map(|n| match n {
                        300 => "five-hour".into(),
                        1440 => "daily".into(),
                        10080 => "weekly".into(),
                        38880..=46080 => "monthly".into(),
                        n => format!("window-{}s", n * 60),
                    })
                    .unwrap_or_else(|| default_id.into())
            } else {
                default_id.into()
            };
            windows.push(QuotaWindow {
                id,
                label: label.into(),
                remaining_percent: Some(100.0 - used),
                reset_at: reset,
                used: None,
                limit: None,
            });
        }
        if windows.is_empty() {
            return;
        }
        let snapshot = QuotaSnapshot {
            account_id: id.into(),
            provider: account.provider,
            windows,
            updated_at: timestamp(observed),
            error: None,
            subscription: account.subscription.clone(),
            credits: vec![],
            verification_required: false,
            verification_available: false,
        };
        let mut cache = self.cache.write().await;
        if cache
            .get(id)
            .and_then(|q| parse_timestamp(&q.updated_at))
            .is_some_and(|last| last > observed)
        {
            return;
        }
        if self
            .apply_observation(id, &snapshot, None, &account.updated_at)
            .await
            .is_ok()
        {
            self.forecast
                .write()
                .await
                .record(&snapshot, observed.unix_timestamp());
            cache.insert(id.into(), snapshot);
            drop(cache);
            self.failures.write().await.remove(id);
            self.save().await;
        }
    }

    /// Accounts the poller should read now: a failing one once its retry
    /// delay has passed, a healthy one once its snapshot is `interval` old.
    /// Retry state of deleted accounts goes.
    pub async fn due(&self, interval: time::Duration, now: time::OffsetDateTime) -> Vec<String> {
        let ids: Vec<String> = self
            .store
            .list()
            .await
            .into_iter()
            .filter(|account| {
                account.provider.reports_quota() && account.provider != SubscriptionProvider::Meta
            })
            .map(|account| account.id)
            .collect();
        let mut failures = self.failures.write().await;
        failures.retain(|id, _| ids.contains(id));
        let cache = self.cache.read().await;
        ids.into_iter()
            .filter(|id| match failures.get(id) {
                Some(failures) => failures.retry_at <= now,
                None => cache
                    .get(id)
                    .and_then(|snapshot| parse_timestamp(&snapshot.updated_at))
                    .is_none_or(|at| now - at >= interval),
            })
            .collect()
    }

    async fn refresh_with(
        &self,
        id: &str,
        trigger: Trigger,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        let account = self
            .store
            .get(id)
            .await
            .ok_or(SubscriptionError::NotFound)?;
        if account.provider == SubscriptionProvider::Meta {
            let mut usage = self.oauth.take_meta_usage(id).await;
            if usage.is_none() && account.dca_token.is_some() {
                // The native key response includes subs_usage. Renew through
                // the shared credential lock and persist the key before use.
                if let Err(error) = self.oauth.force_refresh(id).await {
                    return self
                        .complete_refresh(id, &account, Err(error), trigger)
                        .await;
                }
                usage = self.oauth.take_meta_usage(id).await;
            }
            if let Some((observed, event)) = usage {
                self.observe_meta_event_at(id, &event, observed).await;
            }
            return Ok(self.get(id).await.unwrap_or(QuotaSnapshot {
                account_id: id.into(),
                provider: account.provider,
                windows: vec![],
                updated_at: now(),
                error: None,
                subscription: account.subscription,
                credits: vec![],
                verification_required: false,
                verification_available: false,
            }));
        }
        // Nothing to read: no request, and no error snapshot to show.
        if !account.provider.reports_quota() {
            return Err(SubscriptionError::Unsupported);
        }
        let result = match self.oauth.credential(id).await {
            Ok(credential) => self.fetch(&credential).await,
            Err(error) => Err(error),
        };
        self.complete_refresh(id, &account, result, trigger).await
    }

    async fn complete_refresh(
        &self,
        id: &str,
        account: &AccountCredential,
        result: Result<Observation, SubscriptionError>,
        trigger: Trigger,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        let verification_url = result
            .as_ref()
            .err()
            .and_then(SubscriptionError::verification_url)
            .map(str::to_owned);
        let verification_required = result
            .as_ref()
            .err()
            .is_some_and(SubscriptionError::is_verification_required);
        let verification_available = verification_url.is_some();
        let newly_observed = result.is_ok();
        let snapshot = match result {
            Ok(observation) => {
                let snapshot = QuotaSnapshot {
                    account_id: id.into(),
                    provider: account.provider,
                    windows: observation.windows,
                    updated_at: now(),
                    error: None,
                    subscription: observation
                        .subscription
                        .or_else(|| account.summary().subscription),
                    credits: observation.credits,
                    verification_required: false,
                    verification_available: false,
                };
                self.apply_observation(id, &snapshot, observation.plan, &account.updated_at)
                    .await?;
                if let Some(organization_id) = observation.organization_id {
                    self.store
                        .modify(id, |account| {
                            account.organization_id = Some(organization_id)
                        })
                        .await?;
                }
                // One good read ends the streak and its retry schedule.
                self.failures.write().await.remove(id);
                snapshot
            }
            Err(error) => {
                // Keep the last real observation on a transient error. Its
                // timestamp still states when those numbers were observed.
                let previous = self.cache.read().await.get(id).cloned();
                let observed = time::OffsetDateTime::now_utc();
                let count = {
                    let mut failures = self.failures.write().await;
                    let entry = failures.entry(id.into()).or_insert(Failures {
                        count: 0,
                        retry_at: observed,
                    });
                    entry.count = entry.count.saturating_add(1);
                    entry.retry_at = observed + retry_delay(entry.count);
                    entry.count
                };
                // Anthropic can answer usage reads with 429 for days. The last
                // numbers stand, dated by their observation: inference headers
                // renew them while the account serves, and an idle account
                // spends nothing. Only an account with no numbers shows it.
                let quiet = matches!(error, SubscriptionError::Upstream(429))
                    && previous.as_ref().is_some_and(|v| !v.windows.is_empty());
                // A shown error stays until a read succeeds.
                let shown = previous.as_ref().is_some_and(|v| v.error.is_some())
                    || (!quiet
                        && (trigger == Trigger::Manual
                            || !error.is_transient()
                            || count >= FAILURES_SHOWN));
                if !shown {
                    tracing::debug!(account = id, count, "quota read failed, retrying quietly");
                }
                QuotaSnapshot {
                    account_id: id.into(),
                    provider: account.provider,
                    windows: previous
                        .as_ref()
                        .map(|v| v.windows.clone())
                        .unwrap_or_default(),
                    updated_at: previous
                        .as_ref()
                        .map(|v| v.updated_at.clone())
                        .unwrap_or_else(now),
                    error: shown.then(|| error.to_string()),
                    subscription: previous
                        .as_ref()
                        .and_then(|v| v.subscription.clone())
                        .or_else(|| account.summary().subscription),
                    credits: previous.map(|v| v.credits).unwrap_or_default(),
                    verification_required,
                    verification_available,
                }
            }
        };
        let mut cache = self.cache.write().await;
        let mut verification_urls = self.verification_urls.write().await;
        if let Some(url) = verification_url {
            verification_urls.insert(id.into(), url);
        } else {
            verification_urls.remove(id);
        }
        cache.insert(id.into(), snapshot.clone());
        if newly_observed {
            self.forecast
                .write()
                .await
                .record(&snapshot, time::OffsetDateTime::now_utc().unix_timestamp());
        }
        Ok(snapshot)
    }

    async fn fetch(&self, account: &AccountCredential) -> Result<Observation, SubscriptionError> {
        let observed = time::OffsetDateTime::now_utc();
        let (value, windows) = match account.provider {
            SubscriptionProvider::Codex => {
                let mut request = self
                    .client
                    .get("https://chatgpt.com/backend-api/wham/usage")
                    .bearer_auth(&account.access_token)
                    .header("User-Agent", "codex-tui/0.149.1");
                if let Some(id) = &account.account_id {
                    request = request.header("ChatGPT-Account-Id", id);
                }
                let value = send_json(request).await?;
                let windows = parse_codex(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::Claude => {
                let value = send_json(
                    self.client
                        .get("https://api.anthropic.com/api/oauth/usage")
                        .bearer_auth(&account.access_token)
                        .header("anthropic-beta", "oauth-2025-04-20")
                        .header("User-Agent", "claude-cli/2.1.280 (external, cli)"),
                )
                .await?;
                let windows = parse_claude(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::Xai => {
                let mut request = self
                    .client
                    .get("https://cli-chat-proxy.grok.com/v1/billing?format=credits")
                    .bearer_auth(&account.access_token)
                    .header("x-xai-token-auth", "xai-grok-cli")
                    .header("x-grok-client-version", "1.0.44")
                    .header(
                        "User-Agent",
                        "grok-pager/1.0.44 grok-shell/1.0.44 (macos; aarch64)",
                    );
                if let Some(id) = &account.account_id {
                    request = request.header("x-userid", id);
                }
                let value = send_json(request).await?;
                let windows = parse_xai(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::OpencodeGo => {
                let value = send_json(
                    self.client
                        .get("https://opencode.ai/zen/go/v1/usage")
                        .bearer_auth(&account.access_token)
                        .header("User-Agent", concat!("douane/", env!("CARGO_PKG_VERSION"))),
                )
                .await?;
                let windows = parse_opencode_go(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::Antigravity => {
                let project =
                    account
                        .project_id
                        .as_deref()
                        .ok_or(SubscriptionError::InvalidImport(
                            "missing Google project ID",
                        ))?;
                let value = send_json(self.client.post("https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary")
                    .bearer_auth(&account.access_token).header("User-Agent", "antigravity/hub/2.9.1")
                    .json(&json!({"project":project}))).await?;
                let windows = parse_antigravity(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::Gemini => {
                let project =
                    account
                        .project_id
                        .as_deref()
                        .ok_or(SubscriptionError::InvalidImport(
                            "missing Google project ID",
                        ))?;
                let value = send_json(
                    self.client
                        .post("https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota")
                        .bearer_auth(&account.access_token)
                        .header("User-Agent", "GeminiCLI/0.34.0 (windows; amd64)")
                        .json(&json!({"project":project})),
                )
                .await?;
                let windows = parse_gemini(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::Kimi => {
                let value = send_json(
                    self.client
                        .get("https://api.kimi.com/coding/v1/usages")
                        .bearer_auth(&account.access_token),
                )
                .await?;
                let windows = parse_kimi(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::Copilot => {
                let value = send_json(
                    self.client
                        .get(&self.copilot_user_url)
                        .header("Authorization", format!("token {}", account.access_token))
                        .header("User-Agent", crate::relay::COPILOT_USER_AGENT)
                        .header("X-GitHub-Api-Version", crate::relay::COPILOT_API_VERSION),
                )
                .await?;
                let windows = parse_copilot(&value, observed);
                (value, windows)
            }
            SubscriptionProvider::OpenaiCompatible
            | SubscriptionProvider::Qwen
            | SubscriptionProvider::Meta => {
                return Err(SubscriptionError::Unsupported);
            }
        };
        let credits = parse_credits(account.provider, &value, observed);
        if windows.is_empty() && credits.is_empty() {
            return Err(SubscriptionError::InvalidResponse);
        }
        let plan = text(&value, &["plan_type", "planType", "subscription_type"]);
        let mut subscription = if account.provider == SubscriptionProvider::Copilot {
            // The SKU names education and free plans; the plan alone does not.
            text(&value, &["access_type_sku"])
                .and_then(|sku| {
                    SubscriptionTier::detect(account.provider, Some(&sku), None, "provider_usage")
                })
                .or_else(|| {
                    SubscriptionTier::detect(
                        account.provider,
                        text(&value, &["copilot_plan"]).as_deref(),
                        None,
                        "provider_usage",
                    )
                })
        } else {
            subscription_from_value(account.provider, &value, "provider_usage")
        };
        let mut organization_id = None;
        // Usage and account metadata are different provider resources. A
        // metadata outage must not discard a successful quota observation.
        match account.provider {
            SubscriptionProvider::Claude => {
                if let Ok(profile) = send_json(
                    self.client
                        .get("https://api.anthropic.com/api/oauth/profile")
                        .bearer_auth(&account.access_token)
                        .header("User-Agent", "claude-cli/2.1.280 (external, cli)")
                        .header("anthropic-beta", "oauth-2025-04-20")
                        .timeout(std::time::Duration::from_secs(10)),
                )
                .await
                {
                    subscription =
                        subscription_from_value(account.provider, &profile, "provider_profile")
                            .or(subscription);
                    organization_id = profile
                        .get("organization")
                        .and_then(|organization| text(organization, &["uuid", "id"]));
                }
            }
            SubscriptionProvider::Xai => {
                let mut request = self
                    .client
                    .get("https://cli-chat-proxy.grok.com/v1/settings")
                    .bearer_auth(&account.access_token)
                    .header("x-xai-token-auth", "xai-grok-cli")
                    .header("x-grok-client-version", "1.0.44")
                    .timeout(std::time::Duration::from_secs(10));
                if let Some(id) = &account.account_id {
                    request = request.header("x-userid", id);
                }
                if let Ok(settings) = send_json(request).await {
                    subscription =
                        subscription_from_value(account.provider, &settings, "provider_settings")
                            .or(subscription);
                }
            }
            SubscriptionProvider::OpencodeGo if !windows.is_empty() => {
                // This endpoint rejects accounts without the Go entitlement.
                subscription =
                    SubscriptionTier::detect(account.provider, Some("go"), None, "provider_usage");
            }
            _ => {}
        }
        Ok(Observation {
            windows,
            plan,
            subscription,
            credits,
            organization_id,
        })
    }

    async fn apply_observation(
        &self,
        id: &str,
        snapshot: &QuotaSnapshot,
        plan: Option<String>,
        account_version: &str,
    ) -> Result<(), SubscriptionError> {
        self.store
            .modify(id, |account| {
                let current = time::OffsetDateTime::now_utc();
                let cooling = account
                    .cooldown_until
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_some_and(|until| until > current);
                // A successful authenticated quota read supersedes an older
                // failure, the reason an expired cooldown left behind included,
                // but cannot dismiss an error recorded while the request was in
                // flight or an active inference cooldown.
                if account.updated_at == account_version && !cooling {
                    account.last_error = None;
                }
                if let Some(plan) = plan {
                    account.plan = Some(plan);
                }
                if let Some(subscription) = &snapshot.subscription {
                    account.subscription = Some(subscription.clone());
                    account.plan = Some(subscription.id.clone());
                }
                account.exhausted_until = None;
                account.exhausted_without_reset = false;
                account.model_exhaustion.clear();
                // Empty subscription windows do not stop an account whose
                // provider bills the overflow to usage credits. The scheduler
                // still keeps it behind accounts with subscription quota left.
                let credits = snapshot.usable_credits();
                for window in &snapshot.windows {
                    if !window
                        .remaining_percent
                        .is_some_and(|percent| percent <= 0.0)
                    {
                        continue;
                    }
                    let reset = window.reset_at.as_deref().and_then(parse_timestamp);
                    if reset.is_some_and(|reset| reset <= current) {
                        continue;
                    }
                    if is_global(&window.id) {
                        if credits {
                            continue;
                        }
                        if let Some(reset) = reset {
                            let previous =
                                account.exhausted_until.as_deref().and_then(parse_timestamp);
                            if previous.is_none_or(|previous| previous < reset) {
                                account.exhausted_until = Some(timestamp(reset));
                            }
                        } else {
                            account.exhausted_without_reset = true;
                        }
                    } else {
                        if let Some(model) = window
                            .id
                            .strip_prefix("model:")
                            .and_then(|id| id.split(':').next())
                        {
                            if !model.is_empty() {
                                account
                                    .model_exhaustion
                                    .insert(model.into(), window.reset_at.clone());
                            }
                        }
                        for family in ["opus", "sonnet", "fable"] {
                            if window.id.contains(family) {
                                account
                                    .model_exhaustion
                                    .insert(family.into(), window.reset_at.clone());
                            }
                        }
                    }
                }
            })
            .await?;
        Ok(())
    }
}

/// Window kinds a reserve can watch, shortest first.
pub const RESERVE_WINDOWS: [&str; 4] = ["five-hour", "daily", "weekly", "monthly"];

/// The reserve kind of an account-wide window id; `None` for model-scoped
/// windows and for lengths no provider names.
pub fn reserve_kind(id: &str) -> Option<&'static str> {
    match id {
        "five-hour" => Some("five-hour"),
        "daily" => Some("daily"),
        "weekly" | "seven-day" | "seven_day" => Some("weekly"),
        "monthly" => Some("monthly"),
        _ => None,
    }
}

fn is_weekly(id: &str) -> bool {
    matches!(id, "weekly" | "seven-day" | "seven_day")
}
/// Length of an account-wide window in seconds, used to find the longest
/// one. A `window-` id without a parsable length ranks below every known one.
pub(crate) fn window_seconds(id: &str) -> Option<f64> {
    match id {
        "five-hour" => Some(18_000.0),
        "daily" => Some(86_400.0),
        "weekly" | "seven-day" | "seven_day" => Some(604_800.0),
        "monthly" => Some(30.0 * 86_400.0),
        _ => id.strip_prefix("window-").map(|rest| {
            rest.strip_suffix('s')
                .and_then(|n| n.parse::<f64>().ok())
                .filter(|n| n.is_finite())
                .unwrap_or(0.0)
        }),
    }
}
fn is_global(id: &str) -> bool {
    id.starts_with("window-")
        || matches!(
            id,
            "five-hour" | "weekly" | "seven-day" | "seven_day" | "monthly" | "daily"
        )
}

fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|v| v.is_finite())
}

fn field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key))
}
fn remaining(used_percent: Option<f64>) -> Option<f64> {
    used_percent.map(|used| (100.0 - used).clamp(0.0, 100.0))
}

fn reset(
    value: Option<&Value>,
    relative: Option<&Value>,
    observed: time::OffsetDateTime,
) -> Option<String> {
    value
        .and_then(|value| {
            if let Some(raw) = value.as_str() {
                if let Some(timestamp) = parse_timestamp(raw) {
                    return Some(timestamp);
                }
            }
            let seconds = number(Some(value))?;
            if seconds <= 0.0 {
                return None;
            }
            // Contemporary millisecond epochs cannot be confused with seconds.
            let seconds = if seconds > 100_000_000_000.0 {
                seconds / 1000.0
            } else {
                seconds
            };
            time::OffsetDateTime::from_unix_timestamp(seconds as i64).ok()
        })
        .or_else(|| {
            number(relative)
                .filter(|seconds| *seconds >= 0.0 && *seconds <= 31_536_000.0)
                .map(|seconds| observed + time::Duration::seconds(seconds as i64))
        })
        .map(timestamp)
}

fn cent(value: Option<&Value>) -> Option<f64> {
    let value = value?.as_object()?;
    // The provider's proto3 Cent message serializes zero as an empty object.
    value
        .get("val")
        .map(|value| number(Some(value)))
        .unwrap_or(Some(0.0))
        .filter(|amount| *amount >= 0.0)
        .map(|amount| amount / 100.0)
}

fn credit_remaining(used: Option<f64>, limit: Option<f64>) -> Option<f64> {
    Some((limit? - used?).max(0.0))
}

fn parse_credits(
    provider: SubscriptionProvider,
    value: &Value,
    observed: time::OffsetDateTime,
) -> Vec<QuotaCredits> {
    match provider {
        SubscriptionProvider::Codex => value
            .get("credits")
            .filter(|credits| credits.is_object())
            .map(|credits| {
                vec![QuotaCredits {
                    id: "codex-credits".into(),
                    label: "Credits".into(),
                    used: None,
                    limit: None,
                    remaining: number(credits.get("balance")).filter(|balance| *balance >= 0.0),
                    unit: "credits".into(),
                    reset_at: None,
                    unlimited: credits.get("unlimited").and_then(Value::as_bool),
                    enabled: field(credits, &["has_credits", "hasCredits"])
                        .and_then(Value::as_bool),
                }]
            })
            .unwrap_or_default(),
        SubscriptionProvider::Claude => {
            let Some(extra) = value.get("extra_usage").filter(|extra| extra.is_object()) else {
                return Vec::new();
            };
            let currency = text(extra, &["currency"])
                .filter(|currency| {
                    currency.len() == 3 && currency.bytes().all(|byte| byte.is_ascii_alphabetic())
                })
                .unwrap_or_else(|| "USD".into())
                .to_ascii_uppercase();
            // USD values are cents. Other currencies retain explicit minor
            // units rather than guessing a currency's decimal exponent.
            let (unit, divisor) = if currency == "USD" {
                (currency, 100.0)
            } else {
                (format!("{currency}_minor"), 1.0)
            };
            let used = number(extra.get("used_credits"))
                .filter(|value| *value >= 0.0)
                .map(|value| value / divisor);
            let limit = number(extra.get("monthly_limit"))
                .filter(|value| *value >= 0.0)
                .map(|value| value / divisor);
            vec![QuotaCredits {
                id: "claude-extra-usage".into(),
                label: "Extra usage".into(),
                used,
                limit,
                remaining: credit_remaining(used, limit),
                unit,
                reset_at: reset(field(extra, &["resets_at", "reset_at"]), None, observed),
                unlimited: None,
                enabled: extra.get("is_enabled").and_then(Value::as_bool),
            }]
        }
        SubscriptionProvider::Xai => {
            let Some(config) = value.get("config") else {
                return Vec::new();
            };
            let period = field(config, &["currentPeriod", "current_period"]);
            let reset_at = reset(
                period
                    .and_then(|period| period.get("end"))
                    .or_else(|| field(config, &["billingPeriodEnd", "billing_period_end"])),
                None,
                observed,
            );
            let mut credits = Vec::new();
            for (used_fields, limit_fields, id, label) in [
                (
                    &["used"][..],
                    &["monthlyLimit", "monthly_limit"][..],
                    "grok-included",
                    "Included credits",
                ),
                (
                    &["onDemandUsed", "on_demand_used"][..],
                    &["onDemandCap", "on_demand_cap"][..],
                    "grok-on-demand",
                    "On-demand credits",
                ),
            ] {
                let used = cent(field(config, used_fields));
                let limit = cent(field(config, limit_fields));
                if used.is_some() || limit.is_some() {
                    credits.push(QuotaCredits {
                        id: id.into(),
                        label: label.into(),
                        used,
                        limit,
                        remaining: credit_remaining(used, limit),
                        unit: "USD".into(),
                        reset_at: reset_at.clone(),
                        unlimited: None,
                        enabled: None,
                    });
                }
            }
            if let Some(balance) = cent(field(config, &["prepaidBalance", "prepaid_balance"])) {
                credits.push(QuotaCredits {
                    id: "grok-prepaid".into(),
                    label: "Prepaid credits".into(),
                    used: None,
                    limit: None,
                    remaining: Some(balance),
                    unit: "USD".into(),
                    reset_at: None,
                    unlimited: None,
                    enabled: None,
                });
            }
            credits
        }
        _ => Vec::new(),
    }
}

fn parse_codex(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    for (keys, prefix, label) in [
        (&["rate_limit", "rateLimit"][..], "", "Codex"),
        (
            &["code_review_rate_limit", "codeReviewRateLimit"][..],
            "code-review-",
            "Code review",
        ),
    ] {
        let Some(limits) = field(value, keys) else {
            continue;
        };
        for (keys, id, name) in [
            (
                &["primary_window", "primaryWindow"][..],
                "five-hour",
                "5 hours",
            ),
            (
                &["secondary_window", "secondaryWindow"][..],
                "weekly",
                "Weekly",
            ),
        ] {
            let Some(window) = field(limits, keys).filter(|v| v.is_object()) else {
                continue;
            };
            let duration = number(field(
                window,
                &["limit_window_seconds", "limitWindowSeconds"],
            ));
            // A plan can report its weekly allowance as the primary window.
            // Position in the payload is not the window's duration.
            let (id, name) = match duration.filter(|seconds| *seconds > 0.0) {
                Some(seconds) if seconds == 604_800.0 => ("weekly".into(), "Weekly".into()),
                Some(seconds) if seconds == 18_000.0 => ("five-hour".into(), "5 hours".into()),
                Some(seconds) if seconds == 86_400.0 => ("daily".into(), "Daily".into()),
                Some(seconds) if (27.0 * 86_400.0..=32.0 * 86_400.0).contains(&seconds) => {
                    ("monthly".into(), "Monthly".into())
                }
                Some(seconds) => (
                    format!("window-{}s", seconds as u64),
                    format!("{} seconds", seconds as u64),
                ),
                None => (id.to_owned(), name.to_owned()),
            };
            let used = number(field(window, &["used_percent", "usedPercent"]));
            windows.push(QuotaWindow {
                id: format!("{prefix}{id}"),
                label: format!("{label} {name}"),
                remaining_percent: remaining(used),
                reset_at: reset(
                    field(window, &["reset_at", "resetAt"]),
                    field(window, &["reset_after_seconds", "resetAfterSeconds"]),
                    observed,
                ),
                used,
                limit: Some(100.0),
            });
        }
    }
    windows
}

fn parse_claude(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    for (key, id, label) in [
        ("five_hour", "five-hour", "5 hours"),
        ("seven_day", "seven-day", "Weekly"),
        (
            "seven_day_oauth_apps",
            "seven-day-oauth-apps",
            "OAuth apps weekly",
        ),
        ("seven_day_opus", "seven-day-opus", "Opus weekly"),
        ("seven_day_sonnet", "seven-day-sonnet", "Sonnet weekly"),
        ("seven_day_cowork", "seven-day-cowork", "Cowork weekly"),
        ("iguana_necktie", "seven-day-fable", "Fable weekly"),
    ] {
        let Some(window) = value.get(key).filter(|v| v.is_object()) else {
            continue;
        };
        let used = number(window.get("utilization"));
        windows.push(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent: remaining(used),
            reset_at: reset(window.get("resets_at"), None, observed),
            used,
            limit: Some(100.0),
        });
    }
    if let Some(limits) = value.get("limits").and_then(Value::as_array) {
        for (index, window) in limits.iter().enumerate() {
            if window.get("is_active").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let used = number(window.get("percent"));
            let model = window.pointer("/scope/model/id").and_then(Value::as_str);
            let kind = text(window, &["kind", "group"]).unwrap_or_else(|| format!("limit-{index}"));
            // Claude names its five-hour window "session" in this list.
            let id = if kind.contains("5") || kind.contains("five") || kind.contains("session") {
                "five-hour".into()
            } else if kind.contains("7") || kind.contains("week") {
                model
                    .map(|m| format!("seven-day-{m}"))
                    .unwrap_or_else(|| "seven-day".into())
            } else {
                format!("{kind}-{}", model.unwrap_or("all"))
            };
            if windows.iter().any(|existing| existing.id == id) {
                continue;
            }
            windows.push(QuotaWindow {
                id,
                label: model.map(|m| format!("{m} {kind}")).unwrap_or(kind),
                remaining_percent: remaining(used),
                reset_at: reset(window.get("resets_at"), None, observed),
                used,
                limit: Some(100.0),
            });
        }
    }
    windows
}

/// Snapshots a previous run saved; none when the file is missing or unreadable.
fn load_snapshots(path: &std::path::Path) -> BTreeMap<String, QuotaSnapshot> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return BTreeMap::new();
    }
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Replaces `path` atomically with an owner-only file.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("refusing a symlinked snapshot file"));
    }
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// The five-hour and weekly windows of Claude's unified rate-limit headers:
/// utilization is a fraction, resets are Unix seconds.
fn claude_header_windows(headers: &reqwest::header::HeaderMap) -> Vec<QuotaWindow> {
    let number = |name: String| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    [
        ("5h", "five-hour", "5 hours"),
        ("7d", "seven-day", "Weekly"),
    ]
    .into_iter()
    .filter_map(|(key, id, label)| {
        let fraction = number(format!("anthropic-ratelimit-unified-{key}-utilization"))?;
        let used = (fraction * 10_000.0).round() / 100.0;
        let reset_at = number(format!("anthropic-ratelimit-unified-{key}-reset"))
            .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds as i64).ok())
            .map(timestamp);
        Some(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent: remaining(Some(used)),
            reset_at,
            used: Some(used),
            limit: Some(100.0),
        })
    })
    .collect()
}

fn parse_xai(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let Some(config) = value.get("config") else {
        return Vec::new();
    };
    let explicit_used = number(field(
        config,
        &["creditUsagePercent", "credit_usage_percent"],
    ));
    let period = field(config, &["currentPeriod", "current_period"]);
    let period_reset = reset(period.and_then(|p| p.get("end")), None, observed);
    let legacy_limit = cent(field(config, &["monthlyLimit", "monthly_limit"]));
    let legacy_used = cent(config.get("used"));
    let used = explicit_used
        .or_else(|| Some(legacy_used? / legacy_limit.filter(|limit| *limit > 0.0)? * 100.0))
        .or_else(|| {
            (period_reset.is_some() && legacy_used.is_none() && legacy_limit.is_none())
                .then_some(0.0)
        });
    let kind = period
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let monthly = kind.contains("month")
        || (period.is_none()
            && (legacy_limit.is_some()
                || field(config, &["billingPeriodEnd", "billing_period_end"]).is_some()));
    let weekly = kind.contains("week");
    let reset_at = period_reset.or_else(|| {
        monthly
            .then(|| {
                reset(
                    field(config, &["billingPeriodEnd", "billing_period_end"]),
                    None,
                    observed,
                )
            })
            .flatten()
    });
    let mut windows = Vec::new();
    // Monthly billing periodEnd must never masquerade as a weekly capacity reset.
    if used.is_some() || reset_at.is_some() {
        windows.push(QuotaWindow {
            id: if monthly {
                "monthly"
            } else if weekly {
                "weekly"
            } else {
                "window-period"
            }
            .into(),
            label: if monthly {
                "Grok monthly"
            } else if weekly {
                "Grok weekly"
            } else {
                "Grok usage"
            }
            .into(),
            remaining_percent: remaining(used),
            reset_at: reset_at.clone(),
            used,
            limit: Some(100.0),
        });
    }
    if let Some(products) =
        field(config, &["productUsage", "product_usage"]).and_then(Value::as_array)
    {
        for (index, product) in products.iter().enumerate() {
            let used = number(field(product, &["usagePercent", "usage_percent"]));
            windows.push(QuotaWindow {
                id: format!("product-{index}"),
                label: text(product, &["product"])
                    .unwrap_or_else(|| format!("Product {}", index + 1)),
                remaining_percent: remaining(used),
                reset_at: reset_at.clone(),
                used,
                limit: Some(100.0),
            });
        }
    }
    windows
}

fn parse_opencode_go(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    for (key, id, label) in [
        ("rolling", "five-hour", "5 hours"),
        ("weekly", "weekly", "Weekly"),
        ("monthly", "monthly", "Monthly"),
    ] {
        let Some(window) = value
            .get("usage")
            .and_then(|usage| usage.get(key))
            .filter(|v| v.is_object())
        else {
            continue;
        };
        let used = number(window.get("percent"));
        windows.push(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent: remaining(used),
            reset_at: reset(
                field(window, &["resetsAt", "resetAt"]),
                field(window, &["resetInSec", "reset_in_sec"]),
                observed,
            ),
            used,
            limit: Some(100.0),
        });
    }
    windows
}

/// Copilot's monthly premium requests. Chat and completions are unlimited
/// on paid plans and need no bar; a limited one still gets its window.
/// None is account-wide: included models keep answering once premium
/// requests run out, so an empty bar must not take the account out.
fn parse_copilot(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let reset_at = reset(
        field(value, &["quota_reset_date_utc", "quota_reset_date"]),
        None,
        observed,
    )
    .or_else(|| {
        // A bare date resets at midnight UTC.
        value["quota_reset_date"]
            .as_str()
            .and_then(|date| parse_timestamp(&format!("{date}T00:00:00Z")))
            .map(timestamp)
    });
    let mut windows = Vec::new();
    for (key, id, label) in [
        ("premium_interactions", "premium", "Premium requests"),
        ("chat", "chat", "Chat"),
        ("completions", "completions", "Completions"),
    ] {
        let Some(quota) = value
            .get("quota_snapshots")
            .and_then(|snapshots| snapshots.get(key))
            .filter(|quota| quota.is_object())
        else {
            continue;
        };
        if quota["unlimited"].as_bool() == Some(true) {
            continue;
        }
        let limit = number(quota.get("entitlement")).filter(|limit| *limit > 0.0);
        let left = number(field(quota, &["remaining", "quota_remaining"]));
        let remaining_percent = number(quota.get("percent_remaining"))
            .or_else(|| Some(left? / limit? * 100.0))
            .map(|percent| percent.clamp(0.0, 100.0));
        windows.push(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent,
            reset_at: reset_at.clone(),
            used: limit.zip(left).map(|(limit, left)| (limit - left).max(0.0)),
            limit,
        });
    }
    windows
}

fn parse_antigravity(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    if let Some(groups) = value.get("groups").and_then(Value::as_array) {
        for (index, group) in groups.iter().enumerate() {
            let label = text(group, &["displayName", "display_name"])
                .unwrap_or_else(|| format!("Group {}", index + 1));
            if let Some(buckets) = group.get("buckets").and_then(Value::as_array) {
                for (bucket_index, bucket) in buckets.iter().enumerate() {
                    let fraction =
                        number(field(bucket, &["remainingFraction", "remaining_fraction"]));
                    windows.push(QuotaWindow {
                        id: text(bucket, &["bucketId", "bucket_id"])
                            .unwrap_or_else(|| format!("group-{index}-bucket-{bucket_index}")),
                        label: format!(
                            "{label} {}",
                            text(bucket, &["displayName", "display_name", "window"])
                                .unwrap_or_default()
                        ),
                        remaining_percent: fraction.map(|v| (v * 100.0).clamp(0.0, 100.0)),
                        reset_at: reset(
                            field(bucket, &["resetTime", "reset_time"]),
                            None,
                            observed,
                        ),
                        used: None,
                        limit: None,
                    });
                }
            }
        }
    }
    windows
}

fn parse_gemini(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    value
        .get("buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, bucket)| {
            let model = text(bucket, &["modelId", "model_id"]);
            let kind = text(bucket, &["tokenType", "token_type"])
                .unwrap_or_else(|| format!("bucket-{index}"));
            QuotaWindow {
                id: model
                    .as_ref()
                    .map(|model| format!("model:{model}:{kind}"))
                    .unwrap_or_else(|| format!("bucket-{index}")),
                label: model.map(|model| format!("{model} {kind}")).unwrap_or(kind),
                remaining_percent: number(field(
                    bucket,
                    &["remainingFraction", "remaining_fraction"],
                ))
                .map(|fraction| (fraction * 100.0).clamp(0.0, 100.0)),
                reset_at: reset(field(bucket, &["resetTime", "reset_time"]), None, observed),
                used: None,
                limit: None,
            }
        })
        .collect()
}

fn parse_kimi(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    if let Some(limits) = value.get("limits").and_then(Value::as_array) {
        for (index, limit) in limits.iter().enumerate() {
            let detail = limit.get("detail").unwrap_or(limit);
            let used = number(detail.get("used"));
            let max = number(detail.get("limit"));
            let percent = used
                .zip(max)
                .filter(|(_, max)| *max > 0.0)
                .map(|(used, max)| (100.0 - used / max * 100.0).clamp(0.0, 100.0));
            windows.push(QuotaWindow {
                id: format!("limit-{index}"),
                label: text(limit, &["name", "title", "scope"])
                    .unwrap_or_else(|| format!("Limit {}", index + 1)),
                remaining_percent: percent,
                reset_at: reset(
                    field(detail, &["reset_at", "resetAt", "reset_time", "resetTime"]),
                    field(detail, &["reset_in", "resetIn", "ttl"]),
                    observed,
                ),
                used,
                limit: max,
            });
        }
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn meta_session_uses_the_known_reserve_window_and_unknown_duration_stays_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let account = store
            .import(json!({"type":"meta","access_token":"fixture"}), None)
            .await
            .unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store);
        service.observe_meta_event(&account.id,&json!({"type":"response.subscription_usage","subscription":{"weekly":{"used_percent":30,"resets_at":1792368000},"window":{"used_percent":80,"resets_at":1792272519,"window_duration_mins":300}}})).await;
        let snapshot = service.get(&account.id).await.unwrap();
        let session = snapshot
            .windows
            .iter()
            .find(|w| w.label == "Session")
            .unwrap();
        assert_eq!(reserve_kind(&session.id), Some("five-hour"));
        assert_eq!(session.remaining_percent, Some(20.0));
        let older = parse_timestamp(&snapshot.updated_at).unwrap() - time::Duration::seconds(1);
        service.observe_meta_event_at(&account.id, &json!({"type":"response.subscription_usage","subscription":{"weekly":{"used_percent":0}}}), older).await;
        assert_eq!(service.get(&account.id).await.unwrap().windows.len(), 2);
        assert_eq!(
            service.get(&account.id).await.unwrap().updated_at,
            snapshot.updated_at
        );
        service.observe_meta_event(&account.id,&json!({"type":"response.subscription_usage","subscription":{"window":{"used_percent":10}}})).await;
        let snapshot = service.get(&account.id).await.unwrap();
        assert_eq!(reserve_kind(&snapshot.windows[0].id), None);
    }

    #[test]
    fn copilot_premium_requests_get_a_bar_and_unlimited_ones_none() {
        let student = json!({
            "copilot_plan":"individual","access_type_sku":"free_educational_quota",
            "quota_reset_date":"2026-11-01","quota_reset_date_utc":"2026-11-01T00:00:00.000Z",
            "quota_snapshots":{
                "chat":{"unlimited":true,"entitlement":0,"remaining":0,"percent_remaining":100.0},
                "completions":{"unlimited":true,"entitlement":0,"remaining":0,"percent_remaining":100.0},
                "premium_interactions":{"unlimited":false,"entitlement":200,"remaining":150,"percent_remaining":75.0}
            }
        });
        let windows = parse_copilot(&student, observed());
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "premium");
        assert_eq!(windows[0].remaining_percent, Some(75.0));
        assert_eq!(windows[0].used, Some(50.0));
        assert_eq!(windows[0].limit, Some(200.0));
        assert_eq!(windows[0].reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));
        // An empty premium bar must not take the account out: included models
        // keep answering.
        assert!(!is_global(&windows[0].id));
        assert_eq!(
            SubscriptionTier::detect(
                SubscriptionProvider::Copilot,
                Some("free_educational_quota"),
                None,
                "provider_usage"
            )
            .map(|tier| tier.label),
            Some("Copilot Student".into())
        );
        // A free plan's limited chat gets its own bar, read from counts alone.
        let free = json!({"quota_reset_date":"2026-11-01","quota_snapshots":{
            "chat":{"unlimited":false,"entitlement":50,"remaining":10}}});
        let windows = parse_copilot(&free, observed());
        assert_eq!(windows[0].id, "chat");
        assert_eq!(windows[0].remaining_percent, Some(20.0));
        assert_eq!(windows[0].reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));
    }

    fn observed() -> time::OffsetDateTime {
        parse_timestamp("2026-10-03T12:00:00Z").unwrap()
    }

    #[test]
    fn reserve_counts_only_known_account_windows_before_their_reset() {
        let window = |id: &str, remaining: Option<f64>, reset: &str| QuotaWindow {
            id: id.into(),
            label: id.into(),
            remaining_percent: remaining,
            reset_at: Some(reset.into()),
            used: None,
            limit: Some(100.0),
        };
        let snapshot = |windows| QuotaSnapshot {
            account_id: "fixture".into(),
            provider: SubscriptionProvider::Claude,
            windows,
            updated_at: now(),
            error: None,
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        };
        let later = "2026-10-05T12:00:00Z";
        let earlier = "2026-10-03T11:00:00Z";
        // Only the longest window decides: a nearly empty five-hour window
        // beside a weekly one never trips the reserve.
        let both = snapshot(vec![
            window("seven-day", Some(40.0), later),
            window("five-hour", Some(2.0), later),
        ]);
        assert_eq!(both.reserve_window_remaining(&[], observed()), Some(40.0));
        assert!(!both.reserve_reached(5.0, &[], observed()));
        assert!(both.reserve_reached(40.0, &[], observed()));
        assert!(!both.reserve_reached(39.0, &[], observed()));
        // OpenCode Go shape: monthly outranks weekly and five-hour.
        let go = snapshot(vec![
            window("five-hour", Some(100.0), later),
            window("weekly", Some(88.0), later),
            window("monthly", Some(3.0), later),
        ]);
        assert_eq!(go.reserve_window_remaining(&[], observed()), Some(3.0));
        assert!(go.reserve_reached(5.0, &[], observed()));
        let go_weekly_low = snapshot(vec![
            window("weekly", Some(1.0), later),
            window("monthly", Some(60.0), later),
        ]);
        assert!(!go_weekly_low.reserve_reached(5.0, &[], observed()));
        // Limits lift only near the reset of the longest window, a week or
        // more, whatever its id.
        let at = observed().unix_timestamp();
        let in_hours = |hours: i64| {
            (observed() + time::Duration::hours(hours))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let fortnight = snapshot(vec![window("window-1209600s", Some(3.0), &in_hours(5))]);
        assert!(fortnight.longest_window_ends_within(6 * 3600, at));
        assert!(!fortnight.longest_window_ends_within(4 * 3600, at));
        let fortnight_later = snapshot(vec![
            window("weekly", Some(3.0), &in_hours(5)),
            window("window-1209600s", Some(50.0), &in_hours(72)),
        ]);
        assert!(!fortnight_later.longest_window_ends_within(6 * 3600, at));
        let short = snapshot(vec![window("daily", Some(3.0), &in_hours(5))]);
        assert!(!short.longest_window_ends_within(6 * 3600, at));
        // The longest window past its reset no longer holds the account
        // back, and a shorter one does not take over.
        let reset = snapshot(vec![
            window("seven-day", Some(1.0), earlier),
            window("five-hour", Some(1.0), later),
        ]);
        assert_eq!(reset.reserve_window_remaining(&[], observed()), None);
        assert!(!reset.reserve_reached(5.0, &[], observed()));
        assert!(reset.reports_account_window());
        // Each alias counts when it is the only account-wide window.
        for id in [
            "five-hour",
            "daily",
            "seven-day",
            "seven_day",
            "weekly",
            "monthly",
            "window-1",
        ] {
            let single = snapshot(vec![window(id, Some(3.0), later)]);
            assert_eq!(
                single.reserve_window_remaining(&[], observed()),
                Some(3.0),
                "{id}"
            );
            assert!(single.reserve_reached(5.0, &[], observed()), "{id}");
        }
        // A parsed window length ranks by its seconds; an unparsed one last.
        let parsed = snapshot(vec![
            window("window-2592000s", Some(50.0), later),
            window("weekly", Some(2.0), later),
            window("window-period", Some(0.0), later),
        ]);
        assert_eq!(parsed.reserve_window_remaining(&[], observed()), Some(50.0));
        // Model-scoped and unknown windows never invent a limit.
        let scoped = snapshot(vec![
            window("seven-day-opus", Some(0.0), later),
            window("seven-day", None, later),
        ]);
        assert_eq!(scoped.reserve_window_remaining(&[], observed()), None);
        assert!(!scoped.reserve_reached(5.0, &[], observed()));
        assert!(!scoped.reports_account_window());
        // A non-finite share is unknown, not a measurement.
        let broken = snapshot(vec![window("seven-day", Some(f64::NAN), later)]);
        assert_eq!(broken.reserve_window_remaining(&[], observed()), None);
        assert!(!broken.reports_account_window());
        // Chosen kinds replace the longest-window default: five-hour alone,
        // both, or a kind the account does not report.
        let pick = |kinds: &[&str]| kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        let five = pick(&["five-hour"]);
        assert_eq!(both.reserve_window_remaining(&five, observed()), Some(2.0));
        assert!(both.reserve_reached(5.0, &five, observed()));
        let pair = pick(&["five-hour", "weekly"]);
        let weekly_low = snapshot(vec![
            window("seven-day", Some(3.0), later),
            window("five-hour", Some(90.0), later),
        ]);
        assert!(weekly_low.reserve_reached(5.0, &pair, observed()));
        assert!(both.reserve_reached(5.0, &pair, observed()));
        assert!(!weekly_low.reserve_reached(5.0, &five, observed()));
        let monthly = pick(&["monthly"]);
        assert_eq!(both.reserve_window_remaining(&monthly, observed()), None);
        assert!(!both.reports_reserve_window(&monthly));
        assert!(both.reports_reserve_window(&five));
        // A chosen window past its reset stops counting until the next read.
        assert_eq!(reset.reserve_window_remaining(&five, observed()), Some(1.0));
        assert_eq!(
            reset.reserve_window_remaining(&pick(&["weekly"]), observed()),
            None
        );
    }

    #[test]
    fn claude_session_limit_is_the_five_hour_window_not_a_third_one() {
        let reset = "2026-10-03T16:00:00Z";
        let windows = parse_claude(
            &json!({
                "five_hour": {"utilization": 53, "resets_at": reset},
                "seven_day": {"utilization": 32, "resets_at": "2026-10-07T23:00:00Z"},
                "limits": [{"kind": "session", "percent": 53, "resets_at": reset}]
            }),
            observed(),
        );
        let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["five-hour", "seven-day"]);
        // Without the legacy field, the session limit still reads as five hours.
        let only = parse_claude(
            &json!({"limits": [{"kind": "session", "percent": 10, "resets_at": reset}]}),
            observed(),
        );
        assert_eq!(only[0].id, "five-hour");
        assert_eq!(only[0].remaining_percent, Some(90.0));
    }

    #[tokio::test]
    async fn verification_action_lifecycle_preserves_observations_and_clears_on_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store.import(json!({"type":"antigravity","access_token":"fixture","project_id":"fixture-project"}), None).await.unwrap();
        let account = store.get(&summary.id).await.unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        let observation = || Observation {
            windows: parse_antigravity(
                &json!({"groups":[{"displayName":"Models","buckets":[{"remainingFraction":0.3,"resetTime":"2030-10-04T12:00:00Z"}]}]}),
                observed(),
            ),
            plan: None,
            subscription: None,
            credits: Vec::new(),
            organization_id: None,
        };
        let first = service
            .complete_refresh(&summary.id, &account, Ok(observation()), Trigger::Manual)
            .await
            .unwrap();
        assert!(!first.verification_required);
        assert!(!first.verification_available);
        assert!(service.verification_url(&summary.id).await.is_none());

        let challenge = "https://accounts.google.com/signin/continue?challenge=private-fixture";
        let blocked = service
            .complete_refresh(
                &summary.id,
                &account,
                Err(SubscriptionError::verification_required(Some(challenge))),
                Trigger::Manual,
            )
            .await
            .unwrap();
        assert!(blocked.verification_required);
        assert!(blocked.verification_available);
        assert_eq!(blocked.updated_at, first.updated_at);
        assert_eq!(blocked.windows[0].remaining_percent, Some(30.0));
        assert_eq!(
            service.verification_url(&summary.id).await.as_deref(),
            Some(challenge)
        );
        let public = serde_json::to_string(&service.list().await).unwrap();
        assert!(!public.contains("private-fixture"));
        assert!(!public.contains("accounts.google.com"));
        assert!(!format!("{blocked:?}").contains("private-fixture"));

        let without_action = service
            .complete_refresh(
                &summary.id,
                &account,
                Err(SubscriptionError::verification_required(Some(
                    "https://evil.test/challenge",
                ))),
                Trigger::Manual,
            )
            .await
            .unwrap();
        assert!(without_action.verification_required);
        assert!(!without_action.verification_available);
        assert!(service.verification_url(&summary.id).await.is_none());

        service
            .complete_refresh(
                &summary.id,
                &account,
                Err(SubscriptionError::verification_required(Some(challenge))),
                Trigger::Manual,
            )
            .await
            .unwrap();
        let recovered = service
            .complete_refresh(&summary.id, &account, Ok(observation()), Trigger::Manual)
            .await
            .unwrap();
        assert!(!recovered.verification_required);
        assert!(!recovered.verification_available);
        assert!(recovered.error.is_none());
        assert!(service.verification_url(&summary.id).await.is_none());

        service
            .complete_refresh(
                &summary.id,
                &account,
                Err(SubscriptionError::verification_required(Some(challenge))),
                Trigger::Manual,
            )
            .await
            .unwrap();
        store.delete(&summary.id).await.unwrap();
        assert!(service.verification_url(&summary.id).await.is_none());
        assert!(service.list().await.is_empty());
        assert!(service.verification_urls.read().await.is_empty());
    }

    #[test]
    fn retry_delay_widens_up_to_its_cap() {
        let seconds: Vec<_> = (1..=8).map(|n| retry_delay(n).whole_seconds()).collect();
        assert_eq!(seconds, [30, 60, 120, 240, 480, 960, 1800, 1800]);
        assert_eq!(retry_delay(u32::MAX).whole_seconds(), 1800);
    }

    #[tokio::test]
    async fn background_failures_stay_quiet_back_off_and_clear_on_success() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store.import(json!({"type":"antigravity","access_token":"fixture","project_id":"fixture-project"}), None).await.unwrap();
        let account = store.get(&summary.id).await.unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        let id = summary.id.as_str();
        let observation = || Observation {
            windows: parse_antigravity(
                &json!({"groups":[{"displayName":"Models","buckets":[{"remainingFraction":0.3,"resetTime":"2030-10-04T12:00:00Z"}]}]}),
                observed(),
            ),
            plan: None,
            subscription: None,
            credits: Vec::new(),
            organization_id: None,
        };
        let limited = || Err(SubscriptionError::Upstream(429));
        let message =
            "provider is rate-limiting this account's quota reads (HTTP 429); retry later";
        let outage = || Err(SubscriptionError::Upstream(503));
        let shown_outage = "upstream authentication or quota request failed (HTTP 503)";
        let interval = time::Duration::minutes(5);
        let second = time::Duration::seconds(1);
        // An account with no numbers at all.
        let forget = || async {
            let mut empty = service.get(id).await.unwrap();
            empty.windows.clear();
            service.insert_for_test(empty).await;
        };

        service
            .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
            .await
            .unwrap();
        let start = time::OffsetDateTime::now_utc();
        assert!(service.due(interval, start).await.is_empty());
        assert_eq!(service.due(interval, start + interval).await, [id]);

        for count in 1..FAILURES_SHOWN {
            let quiet = service
                .complete_refresh(id, &account, outage(), Trigger::Background)
                .await
                .unwrap();
            assert!(quiet.error.is_none(), "failure {count}");
            assert_eq!(quiet.windows[0].remaining_percent, Some(30.0));
            // The next read waits for a delay that widens with each failure.
            let now = time::OffsetDateTime::now_utc();
            let delay = retry_delay(count);
            assert!(
                service
                    .due(interval, now + delay - 2 * second)
                    .await
                    .is_empty()
            );
            assert_eq!(service.due(interval, now + delay + second).await, [id]);
            assert!(service.refresh_background(id).await.is_none());
        }
        let shown = service
            .complete_refresh(id, &account, outage(), Trigger::Background)
            .await
            .unwrap();
        assert_eq!(shown.error.as_deref(), Some(shown_outage));
        let sticky = service
            .complete_refresh(id, &account, outage(), Trigger::Background)
            .await
            .unwrap();
        assert_eq!(sticky.error.as_deref(), Some(shown_outage));

        // One good read removes the error and the retry schedule at once.
        let recovered = service
            .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
            .await
            .unwrap();
        assert!(recovered.error.is_none());
        assert!(service.get(id).await.unwrap().error.is_none());
        let now = time::OffsetDateTime::now_utc();
        assert!(service.due(interval, now).await.is_empty());

        // A 429 never hides real numbers, by hand or past the fifth failure,
        // yet it still backs off.
        for trigger in [Trigger::Manual; 3]
            .into_iter()
            .chain([Trigger::Background; 4])
        {
            let quiet = service
                .complete_refresh(id, &account, limited(), trigger)
                .await
                .unwrap();
            assert!(quiet.error.is_none());
            assert_eq!(quiet.windows[0].remaining_percent, Some(30.0));
        }
        assert!(service.refresh_background(id).await.is_none());
        // A read asked by hand reports any other first failure.
        let manual_outage = service
            .complete_refresh(id, &account, outage(), Trigger::Manual)
            .await
            .unwrap();
        assert_eq!(manual_outage.error.as_deref(), Some(shown_outage));
        service
            .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
            .await
            .unwrap();
        // With no numbers to show, the 429 is the answer.
        forget().await;
        let manual = service
            .complete_refresh(id, &account, limited(), Trigger::Manual)
            .await
            .unwrap();
        assert_eq!(manual.error.as_deref(), Some(message));
        service
            .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
            .await
            .unwrap();

        // A failure that cannot pass on its own shows at once.
        let revoked = service
            .complete_refresh(
                id,
                &account,
                Err(SubscriptionError::SessionRevoked),
                Trigger::Background,
            )
            .await
            .unwrap();
        assert!(revoked.error.is_some());

        // A deleted account leaves no retry state behind.
        store.delete(id).await.unwrap();
        assert!(service.due(interval, now + interval).await.is_empty());
        assert!(service.failures.read().await.is_empty());
    }

    #[tokio::test]
    async fn snapshots_outlive_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        let account = store.get(&summary.id).await.unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            "0.97".parse().unwrap(),
        );
        service.observe_claude_headers(&account, &headers).await;
        service.save().await;
        // The account file scan skips the snapshot file.
        let reopened = Arc::new(AccountStore::open(directory.path()).unwrap());
        assert_eq!(reopened.list().await.len(), 1);
        let restarted = QuotaService::new(reqwest::Client::new(), reopened);
        let snapshot = restarted.get(&summary.id).await.unwrap();
        assert_eq!(snapshot.windows[0].used, Some(97.0));
        // A 429 after the restart keeps those numbers and shows nothing.
        let limited = restarted
            .complete_refresh(
                &summary.id,
                &account,
                Err(SubscriptionError::Upstream(429)),
                Trigger::Manual,
            )
            .await
            .unwrap();
        assert!(limited.error.is_none());
        assert_eq!(limited.windows[0].used, Some(97.0));
    }

    #[tokio::test]
    async fn claude_inference_headers_refresh_windows_and_end_a_failure_streak() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        let account = store.get(&summary.id).await.unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        let id = summary.id.as_str();
        let header = |pairs: &[(&'static str, &str)]| {
            let mut map = reqwest::header::HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, value.parse().unwrap());
            }
            map
        };
        // No unified headers, as on count_tokens: nothing is recorded.
        service.observe_claude_headers(&account, &header(&[])).await;
        assert!(service.get(id).await.is_none());

        let usage = Observation {
            windows: parse_claude(
                &json!({
                    "five_hour": {"utilization": 10, "resets_at": "2030-10-03T16:00:00Z"},
                    "seven_day": {"utilization": 50, "resets_at": "2030-10-07T23:00:00Z"},
                    "seven_day_opus": {"utilization": 20, "resets_at": "2030-10-07T23:00:00Z"}
                }),
                observed(),
            ),
            plan: None,
            subscription: None,
            credits: Vec::new(),
            organization_id: None,
        };
        service
            .complete_refresh(id, &account, Ok(usage), Trigger::Background)
            .await
            .unwrap();
        let mut aged = service.get(id).await.unwrap();
        aged.updated_at = timestamp(time::OffsetDateTime::now_utc() - time::Duration::hours(1));
        aged.error = Some("old".into());
        service.insert_for_test(aged).await;
        service
            .complete_refresh(
                id,
                &account,
                Err(SubscriptionError::Upstream(429)),
                Trigger::Background,
            )
            .await
            .unwrap();
        assert!(!service.failures.read().await.is_empty());

        service
            .observe_claude_headers(
                &account,
                &header(&[
                    ("anthropic-ratelimit-unified-5h-utilization", "0.0"),
                    ("anthropic-ratelimit-unified-5h-reset", "1792263000"),
                    ("anthropic-ratelimit-unified-7d-utilization", "0.97"),
                    ("anthropic-ratelimit-unified-7d-reset", "1792537200"),
                ]),
            )
            .await;
        let snapshot = service.get(id).await.unwrap();
        assert!(snapshot.error.is_none());
        let window = |id: &str| snapshot.windows.iter().find(|w| w.id == id).unwrap();
        assert_eq!(window("five-hour").remaining_percent, Some(100.0));
        assert_eq!(window("seven-day").used, Some(97.0));
        assert_eq!(window("seven-day").remaining_percent, Some(3.0));
        assert_eq!(
            window("seven-day").reset_at.as_deref(),
            Some("2026-10-20T23:00:00Z")
        );
        // Windows the headers do not carry keep the usage read's numbers.
        assert_eq!(window("seven-day-opus").used, Some(20.0));
        // The streak ends and the poller waits a full interval again.
        assert!(service.failures.read().await.is_empty());
        let now = time::OffsetDateTime::now_utc();
        assert!(
            service
                .due(time::Duration::minutes(5), now)
                .await
                .is_empty()
        );
        // Other providers are left alone.
        let codex = store
            .import(json!({"type":"codex","access_token":"fixture"}), None)
            .await
            .unwrap();
        let codex = store.get(&codex.id).await.unwrap();
        service
            .observe_claude_headers(
                &codex,
                &header(&[("anthropic-ratelimit-unified-7d-utilization", "0.5")]),
            )
            .await;
        assert!(service.get(&codex.id).await.is_none());
    }

    #[tokio::test]
    async fn mixed_precision_claude_readings_preserve_forecasts() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        let account = store.get(&summary.id).await.unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store);
        let observed = time::OffsetDateTime::now_utc();
        let reset = observed + time::Duration::hours(2);
        let baseline = QuotaSnapshot {
            account_id: summary.id.clone(),
            provider: SubscriptionProvider::Claude,
            windows: vec![QuotaWindow {
                id: "five-hour".into(),
                label: "5 hours".into(),
                remaining_percent: Some(80.0),
                reset_at: Some(timestamp(reset)),
                used: Some(20.0),
                limit: Some(100.0),
            }],
            updated_at: timestamp(observed - time::Duration::minutes(30)),
            error: None,
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        };
        service
            .forecast
            .write()
            .await
            .record(&baseline, observed.unix_timestamp() - 1800);
        let recent = QuotaSnapshot {
            updated_at: timestamp(observed - time::Duration::minutes(5)),
            ..baseline.clone()
        };
        service
            .forecast
            .write()
            .await
            .record(&recent, observed.unix_timestamp() - 300);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.534".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            reset.unix_timestamp().to_string().parse().unwrap(),
        );
        service.observe_claude_headers(&account, &headers).await;
        let quiet = service
            .complete_refresh(
                &summary.id,
                &account,
                Err(SubscriptionError::Upstream(429)),
                Trigger::Background,
            )
            .await
            .unwrap();
        assert!(quiet.error.is_none());
        service.save().await;
        let _ = service.forecasts(&[quiet]).await;
        let usage = service
            .complete_refresh(
                &summary.id,
                &account,
                Ok(Observation {
                    windows: parse_claude(
                        &json!({"five_hour": {"utilization": 53, "resets_at": timestamp(reset)}}),
                        observed,
                    ),
                    credits: vec![],
                    plan: None,
                    subscription: None,
                    organization_id: None,
                }),
                Trigger::Manual,
            )
            .await
            .unwrap();
        let usage_at = parse_timestamp(&usage.updated_at).unwrap().unix_timestamp();
        let report = service.forecasts(&[usage]).await;
        assert_eq!(
            report.len(),
            1,
            "rounded usage must not look like a refill after precise headers"
        );
        let span = usage_at - observed.unix_timestamp() + 1800;
        assert_eq!(report[0].rate_per_hour, 33.0 / span as f64 * 3600.0);
    }

    #[tokio::test]
    async fn inference_refills_invalidate_forecasts_without_a_dashboard_read() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        let account = store.get(&summary.id).await.unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store);
        let observed = time::OffsetDateTime::now_utc();
        let reset = observed + time::Duration::hours(2);
        let baseline = QuotaSnapshot {
            account_id: summary.id.clone(),
            provider: SubscriptionProvider::Claude,
            windows: vec![QuotaWindow {
                id: "five-hour".into(),
                label: "5 hours".into(),
                remaining_percent: Some(80.0),
                reset_at: Some(timestamp(reset)),
                used: None,
                limit: None,
            }],
            updated_at: timestamp(observed - time::Duration::minutes(30)),
            error: None,
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        };
        service
            .forecast
            .write()
            .await
            .record(&baseline, observed.unix_timestamp() - 1800);
        for used in ["0.4", "0.6", "0.5"] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "anthropic-ratelimit-unified-5h-utilization",
                used.parse().unwrap(),
            );
            headers.insert(
                "anthropic-ratelimit-unified-5h-reset",
                reset.unix_timestamp().to_string().parse().unwrap(),
            );
            service.observe_claude_headers(&account, &headers).await;
        }
        let snapshots = service.list().await;
        assert!(
            service
                .forecast
                .read()
                .await
                .report(&snapshots, time::OffsetDateTime::now_utc().unix_timestamp())
                .is_empty()
        );
    }

    #[test]
    fn credits_preserve_provider_units_balances_and_unknown_limits() {
        let codex = parse_credits(
            SubscriptionProvider::Codex,
            &json!({"credits":{"has_credits":true,"unlimited":false,"balance":"42.5"}}),
            observed(),
        );
        assert_eq!(codex[0].remaining, Some(42.5));
        assert_eq!(codex[0].unit, "credits");
        assert_eq!(codex[0].used, None);
        assert_eq!(codex[0].limit, None);
        let claude = parse_credits(
            SubscriptionProvider::Claude,
            &json!({"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1250,"utilization":25}}),
            observed(),
        );
        assert_eq!(claude[0].used, Some(12.5));
        assert_eq!(claude[0].limit, Some(50.0));
        assert_eq!(claude[0].remaining, Some(37.5));
        assert_eq!(claude[0].unit, "USD");
        assert_eq!(claude[0].reset_at, None);
        let grok = parse_credits(
            SubscriptionProvider::Xai,
            &json!({"config":{"monthlyLimit":{"val":1000},"used":{},"prepaidBalance":{"val":350},"onDemandCap":{"val":500},"currentPeriod":{"end":"2026-10-10T00:00:00Z"}}}),
            observed(),
        );
        assert_eq!(grok[0].used, Some(0.0));
        assert_eq!(grok[0].remaining, Some(10.0));
        assert_eq!(grok[1].used, None);
        assert_eq!(grok[1].remaining, None);
        assert_eq!(grok[2].remaining, Some(3.5));
        assert_eq!(grok[2].reset_at, None);
    }

    #[test]
    fn credit_only_snapshots_and_old_snapshot_shapes_remain_valid() {
        let old: QuotaSnapshot = serde_json::from_value(json!({"account_id":"fixture","provider":"codex","windows":[],"updated_at":"2026-10-03T12:00:00Z","error":null})).unwrap();
        assert!(old.subscription.is_none());
        assert!(old.credits.is_empty());
        assert!(!old.verification_required);
        assert!(!old.verification_available);
        assert!(
            parse_credits(
                SubscriptionProvider::OpencodeGo,
                &json!({"usage":{"weekly":{"percent":10}}}),
                observed()
            )
            .is_empty()
        );
        let missing = parse_credits(
            SubscriptionProvider::Codex,
            &json!({"credits":{"unlimited":true}}),
            observed(),
        );
        assert_eq!(missing[0].remaining, None);
        assert_eq!(missing[0].unlimited, Some(true));
    }

    #[test]
    fn opencode_go_accepts_relative_resets_from_the_native_usage_reader() {
        let windows = parse_opencode_go(
            &json!({"usage":{"weekly":{"percent":97,"resetInSec":7200}}}),
            observed(),
        );
        assert_eq!(windows[0].reset_at.as_deref(), Some("2026-10-03T14:00:00Z"));
    }

    #[test]
    fn grok_proto3_zero_usage_is_available_when_current_period_is_reported() {
        let windows = parse_xai(
            &json!({"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-10-08T00:00:00Z"}}}),
            observed(),
        );
        assert_eq!(windows[0].remaining_percent, Some(100.0));
    }

    #[test]
    fn grok_missing_period_type_does_not_invent_a_weekly_window() {
        let windows = parse_xai(
            &json!({"config":{"creditUsagePercent":12,"currentPeriod":{"end":"2026-10-08T00:00:00Z"}}}),
            observed(),
        );
        assert_eq!(windows[0].id, "window-period");
        assert_eq!(windows[0].remaining_percent, Some(88.0));
    }

    #[test]
    fn gemini_model_buckets_keep_real_resets_and_unknown_amounts() {
        let windows = parse_gemini(
            &json!({"buckets":[
                {"modelId":"gemini-2.5-pro","tokenType":"REQUESTS","remainingFraction":0.25,"resetTime":"2026-10-04T12:00:00Z"},
                {"modelId":"gemini-2.5-flash","tokenType":"REQUESTS","resetTime":"2026-10-04T13:00:00Z"}
            ]}),
            observed(),
        );
        assert_eq!(windows[0].id, "model:gemini-2.5-pro:REQUESTS");
        assert_eq!(windows[0].remaining_percent, Some(25.0));
        assert_eq!(windows[0].reset_at.as_deref(), Some("2026-10-04T12:00:00Z"));
        assert_eq!(windows[1].remaining_percent, None);
        assert!(
            windows
                .iter()
                .all(|window| window.used.is_none() && window.limit.is_none())
        );
        let snapshot = QuotaSnapshot {
            account_id: "fixture".into(),
            provider: SubscriptionProvider::Gemini,
            windows,
            updated_at: now(),
            error: None,
            subscription: None,
            credits: Vec::new(),
            verification_required: false,
            verification_available: false,
        };
        assert_eq!(snapshot.weekly_reset(), None);
        assert_eq!(snapshot.weekly_remaining_percent(), None);
    }

    #[test]
    fn codex_percentages_and_reset_instants_come_from_the_same_windows() {
        let windows = parse_codex(
            &json!({"rate_limit":{"primary_window":{"used_percent":21,"reset_after_seconds":3600},
            "secondary_window":{"used_percent":"76.5","reset_at":1791633600,"limit_window_seconds":604800}},
            "code_review_rate_limit":{"primary_window":{"used_percent":100,"reset_at":1791633600000i64}}}),
            observed(),
        );
        assert_eq!(windows[0].remaining_percent, Some(79.0));
        assert_eq!(windows[0].reset_at.as_deref(), Some("2026-10-03T13:00:00Z"));
        assert_eq!(windows[1].remaining_percent, Some(23.5));
        assert_eq!(windows[1].reset_at, windows[2].reset_at);
        assert_eq!(windows[2].id, "code-review-five-hour");
    }

    #[test]
    fn codex_primary_weekly_window_drives_weekly_routing() {
        let windows = parse_codex(
            &json!({"rate_limit":{
                "primary_window":{"used_percent":6,"reset_at":1791580593,"reset_after_seconds":560546,"limit_window_seconds":604800},
                "secondary_window":null
            }}),
            observed(),
        );
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "weekly");
        assert_eq!(windows[0].label, "Codex Weekly");
        assert_eq!(windows[0].remaining_percent, Some(94.0));
        let snapshot = QuotaSnapshot {
            account_id: "fixture".into(),
            provider: SubscriptionProvider::Codex,
            windows,
            updated_at: now(),
            error: None,
            subscription: None,
            credits: Vec::new(),
            verification_required: false,
            verification_available: false,
        };
        assert_eq!(snapshot.weekly_remaining_percent(), Some(94.0));
        assert_eq!(snapshot.weekly_reset(), Some("2026-10-09T21:16:33Z"));
    }

    #[test]
    fn claude_missing_utilization_stays_unknown_instead_of_looking_available() {
        let windows = parse_claude(
            &json!({"five_hour":{"utilization":100,"resets_at":"2026-10-03T17:00:00Z"},
            "seven_day":{"resets_at":null},"seven_day_opus":{"utilization":42,"resets_at":"2026-10-07T12:00:00Z"}}),
            observed(),
        );
        assert_eq!(windows[0].remaining_percent, Some(0.0));
        assert_eq!(windows[1].remaining_percent, None);
        assert_eq!(windows[1].reset_at, None);
        assert_eq!(windows[2].remaining_percent, Some(58.0));
    }

    #[test]
    fn grok_weekly_reset_does_not_borrow_a_monthly_billing_rollover() {
        let weekly = parse_xai(
            &json!({"config":{"creditUsagePercent":39.0,"currentPeriod":{"type":"weekly","start":"2026-10-01T00:00:00Z","end":"2026-10-08T00:00:00Z"}}}),
            observed(),
        );
        assert_eq!(weekly[0].remaining_percent, Some(61.0));
        assert_eq!(weekly[0].reset_at.as_deref(), Some("2026-10-08T00:00:00Z"));
        let monthly = parse_xai(
            &json!({"config":{"used":{"val":350},"monthlyLimit":{"val":1000},"billingPeriodEnd":"2026-11-01T00:00:00Z"}}),
            observed(),
        );
        assert_eq!(monthly[0].id, "monthly");
        assert_eq!(monthly[0].remaining_percent, Some(65.0));
        assert_eq!(monthly[0].reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));
    }

    #[test]
    fn opencode_go_reads_all_reported_subscription_windows() {
        let windows = parse_opencode_go(
            &json!({"usage":{"rolling":{"status":"ok","percent":18.5,"resetsAt":"2026-10-03T17:00:00Z"},
            "weekly":{"status":"rate-limited","percent":100,"resetsAt":"2026-10-09T00:00:00Z"},
            "monthly":{"status":"ok","percent":37,"resetsAt":"2026-11-03T00:00:00Z"}}}),
            observed(),
        );
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].remaining_percent, Some(81.5));
        assert_eq!(windows[1].remaining_percent, Some(0.0));
        assert_eq!(windows[1].reset_at.as_deref(), Some("2026-10-09T00:00:00Z"));
    }

    #[test]
    fn antigravity_fraction_and_kimi_counts_remain_real_observations() {
        let antigravity = parse_antigravity(
            &json!({"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"gemini-5h","remainingFraction":"0.3","resetTime":"2026-10-03T15:00:00Z"}]}]}),
            observed(),
        );
        assert_eq!(antigravity[0].remaining_percent, Some(30.0));
        let kimi = parse_kimi(
            &json!({"limits":[{"name":"Requests","detail":{"used":6,"limit":10,"reset_in":120}}]}),
            observed(),
        );
        assert_eq!(kimi[0].remaining_percent, Some(40.0));
        assert_eq!(kimi[0].reset_at.as_deref(), Some("2026-10-03T12:02:00Z"));
    }

    #[tokio::test]
    async fn successful_quota_clears_an_older_error_without_active_cooldown() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let account = store
            .import(json!({"type":"xai","access_token":"fixture-access","refresh_token":"fixture-refresh"}), None)
            .await
            .unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        let snapshot = QuotaSnapshot {
            account_id: account.id.clone(),
            provider: SubscriptionProvider::Xai,
            windows: parse_xai(
                &json!({"config":{"creditUsagePercent":57.0,"currentPeriod":{"type":"weekly","end":"2030-10-05T18:34:26Z"}}}),
                observed(),
            ),
            updated_at: now(),
            error: None,
            subscription: None,
            credits: Vec::new(),
            verification_required: false,
            verification_available: false,
        };
        store
            .mark_error(&account.id, Some("fixture transient failure".into()))
            .await
            .unwrap();
        let before = store.get(&account.id).await.unwrap();
        assert_eq!(before.summary().status, "error");
        service
            .apply_observation(&account.id, &snapshot, None, &before.updated_at)
            .await
            .unwrap();
        let recovered = store.get(&account.id).await.unwrap();
        assert_eq!(recovered.summary().status, "ready");
        assert!(recovered.last_error.is_none());
        assert_eq!(recovered.access_token, before.access_token);
        assert_eq!(recovered.refresh_token, before.refresh_token);
        assert!(
            AccountStore::open(directory.path())
                .unwrap()
                .get(&account.id)
                .await
                .unwrap()
                .last_error
                .is_none()
        );

        store
            .mark_cooldown(&account.id, 60, "provider request failed")
            .await
            .unwrap();
        let cooling = store.get(&account.id).await.unwrap();
        service
            .apply_observation(&account.id, &snapshot, None, &cooling.updated_at)
            .await
            .unwrap();
        let after = store.get(&account.id).await.unwrap();
        assert_eq!(after.summary().status, "cooldown");
        assert_eq!(after.cooldown_until, cooling.cooldown_until);
        assert_eq!(after.last_error, cooling.last_error);

        // An inference cooldown keeps its reason once it expires; the next
        // successful read is what returns the account to ready.
        store
            .mark_cooldown(&account.id, 10, "upstream connection failed")
            .await
            .unwrap();
        store
            .modify(&account.id, |account| {
                account.cooldown_until = Some("2020-01-01T00:00:00Z".into())
            })
            .await
            .unwrap();
        let expired = store.get(&account.id).await.unwrap();
        assert_eq!(expired.summary().status, "error");
        service
            .apply_observation(&account.id, &snapshot, None, &expired.updated_at)
            .await
            .unwrap();
        let recovered = store.get(&account.id).await.unwrap();
        assert_eq!(recovered.summary().status, "ready");
        assert!(recovered.last_error.is_none());

        store
            .modify(&account.id, |account| account.cooldown_until = None)
            .await
            .unwrap();
        let before = store.get(&account.id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        store
            .mark_error(&account.id, Some("fixture newer failure".into()))
            .await
            .unwrap();
        let newer = store.get(&account.id).await.unwrap();
        assert_ne!(newer.updated_at, before.updated_at);
        service
            .apply_observation(&account.id, &snapshot, None, &before.updated_at)
            .await
            .unwrap();
        assert_eq!(
            store.get(&account.id).await.unwrap().last_error,
            newer.last_error
        );
    }

    #[tokio::test]
    async fn exhausted_global_window_blocks_selection_but_opus_window_only_blocks_opus() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let account = store
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        let mut snapshot = QuotaSnapshot {
            account_id: account.id.clone(),
            provider: SubscriptionProvider::Claude,
            windows: vec![QuotaWindow {
                id: "seven-day-opus".into(),
                label: "Opus".into(),
                remaining_percent: Some(0.0),
                reset_at: Some("2030-01-01T00:00:00Z".into()),
                used: Some(100.0),
                limit: Some(100.0),
            }],
            updated_at: now(),
            error: None,
            subscription: None,
            credits: Vec::new(),
            verification_required: false,
            verification_available: false,
        };
        service
            .apply_observation(&account.id, &snapshot, None, &account.updated_at)
            .await
            .unwrap();
        assert!(
            store
                .eligible(SubscriptionProvider::Claude, Some("claude-opus-4-6"))
                .await
                .is_empty()
        );
        assert_eq!(
            store
                .eligible(SubscriptionProvider::Claude, Some("claude-sonnet-4-6"))
                .await
                .len(),
            1
        );
        snapshot.windows[0].id = "five-hour".into();
        service
            .apply_observation(&account.id, &snapshot, None, &account.updated_at)
            .await
            .unwrap();
        assert!(
            store
                .eligible(SubscriptionProvider::Claude, Some("claude-sonnet-4-6"))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn codex_usage_credits_keep_an_exhausted_account_selectable() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let account = store
            .import(json!({"type":"codex","access_token":"fixture"}), None)
            .await
            .unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        let usage = |credits: Value| {
            json!({
                "rate_limit": {
                    "primary_window": {"used_percent": 100, "limit_window_seconds": 18000, "reset_at": 1_900_000_000},
                    "secondary_window": {"used_percent": 100, "limit_window_seconds": 604800, "reset_at": 1_900_000_000}
                },
                "credits": credits
            })
        };
        let observe = |value: Value| QuotaSnapshot {
            account_id: account.id.clone(),
            provider: SubscriptionProvider::Codex,
            windows: parse_codex(&value, observed()),
            updated_at: now(),
            error: None,
            subscription: None,
            credits: parse_credits(SubscriptionProvider::Codex, &value, observed()),
            verification_required: false,
            verification_available: false,
        };
        let later = parse_timestamp("2031-01-01T00:00:00Z").unwrap();
        let eligible = || async {
            store
                .eligible(SubscriptionProvider::Codex, Some("gpt-5.5"))
                .await
                .len()
        };
        for (credits, selectable) in [
            (
                json!({"has_credits":true,"unlimited":false,"balance":"42.5"}),
                true,
            ),
            (
                json!({"has_credits":true,"unlimited":true,"balance":null}),
                true,
            ),
            (
                json!({"has_credits":true,"unlimited":false,"balance":"0"}),
                false,
            ),
            (
                json!({"has_credits":false,"unlimited":false,"balance":"10"}),
                false,
            ),
            (Value::Null, false),
        ] {
            let snapshot = observe(usage(credits.clone()));
            assert!(snapshot.window_exhausted(observed()), "{credits}");
            assert_eq!(snapshot.credits_only(observed()), selectable, "{credits}");
            assert!(!snapshot.window_exhausted(later), "{credits}");
            service
                .apply_observation(&account.id, &snapshot, None, &account.updated_at)
                .await
                .unwrap();
            assert_eq!(eligible().await, usize::from(selectable), "{credits}");
            let status = store.get(&account.id).await.unwrap().summary().status;
            assert_eq!(status == "exhausted", !selectable, "{credits}");
        }
    }
}
