use crate::{
    ledger::Ledger,
    scheduler::{Candidate, Scheduler},
    settings::Config,
    subscriptions::{
        AccountCredential, AccountStore, OAuthManager, QuotaService, ResetManager,
        SubscriptionProvider, antigravity_user_agent,
    },
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::RwLock;

#[derive(Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: SubscriptionProvider,
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub upstream_id: String,
    #[serde(default)]
    pub thinking: Option<Value>,
    /// The custom endpoint route serving this model, absent for providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Upstream paths the model accepts, when its provider says so
    /// (`/v1/messages`, `/responses`, `/chat/completions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<String>,
    /// Native provider catalog fields required by its own client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}
fn yes() -> bool {
    true
}

/// The last catalog read of one pool.
#[derive(Clone, Serialize)]
pub struct ModelListing {
    pub refreshed_at: String,
    /// The live listing failed, so the pool shows the previous catalog.
    pub stale: bool,
}

/// Whether `model` belongs to `pool`, whose name prefixes its id.
fn in_pool(model: &Model, pool: &str) -> bool {
    model
        .id
        .strip_prefix(pool)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// A blank enabled row; constructors name the fields they know.
impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider: SubscriptionProvider::OpenaiCompatible,
            name: String::new(),
            context_length: None,
            max_output_tokens: None,
            enabled: true,
            upstream_id: String::new(),
            thinking: None,
            route: None,
            endpoints: vec![],
            metadata: None,
        }
    }
}

/// The bundled catalog, each provider's models in that provider's own order.
pub(crate) fn catalog() -> &'static [Model] {
    static CATALOG: std::sync::OnceLock<Vec<Model>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../data/models.json")).unwrap_or_default()
    })
}

/// A model's place in its provider's own list, the provider's default first.
/// A model the catalog does not name comes after the ones it does.
pub(crate) fn catalog_rank(provider: SubscriptionProvider, upstream_id: &str) -> usize {
    catalog()
        .iter()
        .filter(|model| model.provider == provider)
        .position(|model| model.id == upstream_id)
        .unwrap_or(usize::MAX)
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: Option<PathBuf>,
    pub accounts: Arc<AccountStore>,
    pub oauth: Arc<OAuthManager>,
    pub quotas: Arc<QuotaService>,
    pub resets: Arc<ResetManager>,
    pub ledger: Arc<Ledger>,
    pub clients: Arc<crate::clients::Clients>,
    pub client: reqwest::Client,
    pub models: RwLock<Vec<Model>>,
    /// When each pool's catalog was last read, and whether its live listing
    /// failed; the background loop rereads a stale pool until it answers.
    pub model_listings: Mutex<BTreeMap<String, ModelListing>>,
    /// One catalog refresh at a time: each one starts from the rows the
    /// last one published.
    model_refresh: tokio::sync::Mutex<()>,
    /// Milliseconds between native WebSocket pings; a peer silent for 3.6
    /// of them (90 s at the default 25 s) is dropped. Tests shorten it.
    pub socket_heartbeat_ms: std::sync::atomic::AtomicU64,
    pub scheduler: Mutex<Scheduler>,
    pub affinity: Mutex<crate::affinity::Affinity>,
    pub(crate) antigravity_replay: Mutex<crate::antigravity_replay::ReplayCache>,
    pub management_token: Option<String>,
    pub config_write: tokio::sync::Mutex<()>,
    /// Last background quota refresh started for an account near its reserve.
    reserve_refresh: Mutex<BTreeMap<String, std::time::Instant>>,
    /// Models an account refused although a catalog lists them, by account
    /// and model, with the time of the refusal.
    refused: Mutex<BTreeMap<(String, String), std::time::Instant>>,
    /// The upstream ids each Codex, Copilot or Antigravity account's own live
    /// listing returned last: one Google or GitHub plan can offer models
    /// another one of the same pool lacks.
    listed: Mutex<BTreeMap<String, BTreeSet<String>>>,
}

/// How long a refusal hides a model from an account: a plan change shows up
/// the next day, or at once after a restart.
const REFUSAL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

impl AppState {
    pub fn new(mut config: Config, config_path: Option<PathBuf>) -> Result<Arc<Self>> {
        config.settings.validate()?;
        config.server.normalize()?;
        let management_token = config
            .server
            .management_token_env
            .as_ref()
            .map(|name| std::env::var(name))
            .transpose()?;
        let accounts = Arc::new(AccountStore::open(config.server.auth_dir.clone())?);
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(120))
            .pool_max_idle_per_host(32)
            .tcp_nodelay(true)
            .user_agent(concat!("douane/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let oauth = Arc::new(OAuthManager::new(client.clone(), accounts.clone()));
        let quotas = Arc::new(QuotaService::with_oauth(
            client.clone(),
            accounts.clone(),
            oauth.clone(),
        ));
        let resets = Arc::new(ResetManager::new(accounts.clone(), oauth.clone())?);
        let ledger = Ledger::open(&config.server.database)?;
        let clients = Arc::new(crate::clients::Clients::open(&config.server.auth_dir)?);
        let antigravity_replay =
            crate::antigravity_replay::ReplayCache::for_settings(&config.settings);
        Ok(Arc::new(Self {
            config: RwLock::new(config),
            config_path,
            accounts,
            oauth,
            quotas,
            resets,
            ledger,
            clients,
            client,
            models: RwLock::new(vec![]),
            model_listings: Mutex::new(BTreeMap::new()),
            model_refresh: tokio::sync::Mutex::new(()),
            socket_heartbeat_ms: std::sync::atomic::AtomicU64::new(25_000),
            scheduler: Mutex::new(Scheduler::default()),
            affinity: Mutex::new(crate::affinity::Affinity::default()),
            antigravity_replay: Mutex::new(antigravity_replay),
            management_token,
            config_write: tokio::sync::Mutex::new(()),
            reserve_refresh: Mutex::new(BTreeMap::new()),
            refused: Mutex::new(BTreeMap::new()),
            listed: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Apply the replay cache's limits from changed settings.
    pub fn configure_replay(&self, settings: &crate::settings::Settings) {
        self.antigravity_replay
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .configure(settings);
    }

    fn refuses(&self, account: &str, model: &str) -> bool {
        self.refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(account.to_owned(), model.to_owned()))
            .is_some_and(|at| at.elapsed() < REFUSAL)
    }

    /// Whether `account` can be sent `model`: it did not refuse it lately,
    /// and its own last live listing, when it has one, names it.
    fn serves(&self, account: &str, model: &str, provider: SubscriptionProvider) -> bool {
        !self.refuses(account, model)
            && self
                .listed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(account)
                .map_or(provider != SubscriptionProvider::Meta, |ids| {
                    ids.contains(model)
                })
    }

    /// A catalog can list models an account's plan cannot use (Copilot), or
    /// that only another account of the pool offers (Antigravity); the first
    /// refusal skips the account for that model, and the model leaves the
    /// list once no account of the provider serves it.
    pub(crate) async fn model_refused(
        &self,
        provider: SubscriptionProvider,
        account: &str,
        model: &str,
    ) {
        self.refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (account.to_owned(), model.to_owned()),
                std::time::Instant::now(),
            );
        let served = self
            .accounts
            .eligible_in(provider, None, Some(model))
            .await
            .iter()
            .any(|other| self.serves(&other.id, model, other.provider));
        if !served {
            self.models
                .write()
                .await
                .retain(|m| !(m.provider == provider && m.upstream_id == model));
        }
    }

    pub async fn refresh_models(&self) {
        self.refresh_models_in(None).await;
    }

    /// Pools whose last live listing failed.
    pub fn stale_pools(&self) -> Vec<String> {
        self.model_listings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, listing)| listing.stale)
            .map(|(pool, _)| pool.clone())
            .collect()
    }

    /// Rereads the catalog of one pool, or of every pool; the other pools
    /// keep their rows untouched.
    pub async fn refresh_models_in(&self, only: Option<&str>) {
        let _refresh = self.model_refresh.lock().await;
        let bundled = catalog();
        let previous = self.models.read().await.clone();
        let mut read: BTreeSet<String> = BTreeSet::new();
        let mut failed: BTreeSet<String> = BTreeSet::new();
        let mut models: BTreeMap<String, Model> = BTreeMap::new();
        if let Some(pool) = only {
            for model in previous.iter().filter(|m| !in_pool(m, pool)) {
                models.insert(model.id.clone(), model.clone());
            }
        }
        for summary in self.accounts.list().await {
            let Some(account) = self.accounts.get(&summary.id).await else {
                continue;
            };
            if only.is_some_and(|pool| pool != account.pool()) {
                continue;
            }
            let pool = account.pool();
            read.insert(pool.to_owned());
            // What the last refresh read for this account, within its own
            // model list, for when a live listing fails.
            let kept: Vec<Model> = previous
                .iter()
                .filter(|m| {
                    m.provider == account.provider
                        && in_pool(m, pool)
                        && (account.models.is_empty() || account.models.contains(&m.upstream_id))
                })
                .map(|m| Model {
                    id: m.upstream_id.clone(),
                    ..m.clone()
                })
                .collect();
            let mut available: Vec<Model> = if account.models.is_empty() {
                bundled
                    .iter()
                    .filter(|m| m.provider == account.provider)
                    .cloned()
                    .collect()
            } else {
                account
                    .models
                    .iter()
                    .map(|id| Model {
                        id: id.clone(),
                        provider: account.provider,
                        name: id.clone(),
                        enabled: true,
                        upstream_id: id.clone(),
                        ..Model::default()
                    })
                    .collect()
            };
            if matches!(
                account.provider,
                SubscriptionProvider::Meta
                    | SubscriptionProvider::OpencodeGo
                    | SubscriptionProvider::OpenaiCompatible
                    | SubscriptionProvider::Qwen
            ) {
                let base = account
                    .base_url
                    .as_deref()
                    .unwrap_or(match account.provider {
                        SubscriptionProvider::Meta => crate::subscriptions::meta::API,
                        SubscriptionProvider::OpencodeGo => "https://opencode.ai/zen/go/v1",
                        SubscriptionProvider::Qwen => "https://portal.qwen.ai/v1",
                        _ => "",
                    });
                if !base.is_empty() {
                    let root = base.trim_end_matches('/');
                    let url = if account.provider == SubscriptionProvider::Meta {
                        format!(
                            "{}/muse-code/models",
                            root.strip_suffix("/v1").unwrap_or(root)
                        )
                    } else if root.ends_with("/v1") {
                        format!("{root}/models")
                    } else {
                        format!("{root}/v1/models")
                    };
                    let credential = if account.provider == SubscriptionProvider::Meta {
                        self.oauth.credential(&account.id).await.ok()
                    } else {
                        Some(account.clone())
                    };
                    let listing = self
                        .client
                        .get(url)
                        .headers(
                            credential
                                .as_ref()
                                .map(|a| {
                                    crate::relay::upstream_headers(a, &axum::http::HeaderMap::new())
                                })
                                .unwrap_or_default(),
                        )
                        .header("Accept", "application/json")
                        .timeout(std::time::Duration::from_secs(15))
                        .send()
                        .await
                        .ok()
                        .filter(|response| response.status().is_success());
                    let data = match listing {
                        Some(response) => response.json::<Value>().await.ok(),
                        None => None,
                    };
                    if let Some(data) = data.as_ref().and_then(|v| v["data"].as_array()) {
                        if account.provider == SubscriptionProvider::Meta {
                            let ids = data
                                .iter()
                                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                                .collect();
                            self.listed
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(account.id.clone(), ids);
                        }
                        available = data
                            .iter()
                            .filter_map(|m| {
                                let native = (account.provider == SubscriptionProvider::Meta)
                                    .then(|| &m["metadata"]["muse-code"])
                                    .filter(|v| v.is_object());
                                Some(Model {
                                    id: m["id"].as_str()?.into(),
                                    name: m["name"].as_str().or_else(|| m["id"].as_str())?.into(),
                                    provider: account.provider,
                                    context_length: m["context_length"]
                                        .as_u64()
                                        .or_else(|| m["context_window"].as_u64())
                                        .or_else(|| {
                                            native.and_then(|v| v["limit"]["context"].as_u64())
                                        }),
                                    max_output_tokens: native
                                        .and_then(|v| v["limit"]["output"].as_u64()),
                                    enabled: true,
                                    upstream_id: String::new(),
                                    thinking: m.get("thinking").cloned(),
                                    metadata: native.map(|v| json!({"muse-code":v})),
                                    ..Model::default()
                                })
                            })
                            .filter(|m| account.models.is_empty() || account.models.contains(&m.id))
                            .collect();
                    } else {
                        // A listing that times out or errors once must not
                        // empty the pool until the next restart: keep what
                        // the last refresh read, within the account's own
                        // model list. An account that names its models keeps
                        // them without a listing, so only one that names
                        // none is retried in the background.
                        tracing::warn!(pool = %pool, "model listing unavailable, keeping the previous catalog");
                        if account.models.is_empty() {
                            available = kept.clone();
                        } else {
                            // Every named model stays, with what the last
                            // listing said about it when it said something.
                            for model in &mut available {
                                if let Some(known) =
                                    kept.iter().find(|k| k.upstream_id == model.upstream_id)
                                {
                                    *model = known.clone();
                                }
                            }
                        }
                        if account.models.is_empty() {
                            failed.insert(pool.to_owned());
                        }
                    }
                }
            }
            // A failed live read keeps the previous rows, or the bundled
            // ones on a first read, and is retried like a failed listing.
            if account.provider == SubscriptionProvider::Antigravity && account.models.is_empty() {
                match self.antigravity_models(&account).await {
                    Some(live) => {
                        self.remember_listing(&account.id, &live);
                        available = live
                            .into_iter()
                            .filter(|m| !self.refuses(&account.id, &m.id))
                            .collect();
                    }
                    None => {
                        if !kept.is_empty() {
                            available = kept.clone();
                        }
                        failed.insert(pool.to_owned());
                    }
                }
            }
            if matches!(
                account.provider,
                SubscriptionProvider::Codex | SubscriptionProvider::Copilot
            ) {
                let live = if account.provider == SubscriptionProvider::Codex {
                    self.codex_models(&account).await
                } else {
                    self.copilot_models(&account).await
                };
                match live {
                    Some(live) => {
                        self.remember_listing(&account.id, &live);
                        available = live
                            .into_iter()
                            .filter(|m| account.models.is_empty() || account.models.contains(&m.id))
                            .filter(|m| !self.refuses(&account.id, &m.id))
                            .collect();
                    }
                    None => {
                        if !kept.is_empty() {
                            available = kept
                                .iter()
                                .filter(|m| !self.refuses(&account.id, &m.id))
                                .cloned()
                                .collect();
                        }
                        failed.insert(pool.to_owned());
                    }
                }
            }
            for mut model in available {
                model.upstream_id = model.id.clone();
                model.route = account.route.clone();
                model.id = format!("{}/{}", account.pool(), model.upstream_id);
                models.insert(model.id.clone(), model);
            }
        }
        // A model switched while the listings were read must keep its new
        // state: read the disabled list once the catalog is locked.
        let mut published = self.models.write().await;
        let disabled = self.config.read().await.disabled_models.clone();
        *published = models
            .into_values()
            .map(|mut model| {
                model.enabled =
                    !disabled.contains(&model.id) && !disabled.contains(&model.upstream_id);
                model
            })
            .collect();
        drop(published);
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let mut listings = self
            .model_listings
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match only {
            Some(pool) => {
                listings.remove(pool);
            }
            None => listings.clear(),
        }
        for pool in read {
            let stale = failed.contains(&pool);
            listings.insert(
                pool,
                ModelListing {
                    refreshed_at: now.clone(),
                    stale,
                },
            );
        }
    }

    /// Copilot's own catalog: the models this account's plan enables, with
    /// the upstream paths each one accepts.
    async fn copilot_models(&self, account: &AccountCredential) -> Option<Vec<Model>> {
        let base = account
            .base_url
            .as_deref()
            .unwrap_or(crate::relay::COPILOT_API);
        let response = self
            .client
            .get(crate::relay::endpoint(base, "/models"))
            .bearer_auth(&account.access_token)
            .header("User-Agent", crate::relay::COPILOT_USER_AGENT)
            .header("X-GitHub-Api-Version", crate::relay::COPILOT_API_VERSION)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value = response.json::<Value>().await.ok()?;
        let models = copilot_catalog(&value);
        (!models.is_empty()).then_some(models)
    }

    /// Codex's native list carries newly released models and their current
    /// context/reasoning metadata, independently of the bundled fallback.
    async fn codex_models(&self, stored: &AccountCredential) -> Option<Vec<Model>> {
        let account = self.oauth.credential(&stored.id).await.ok()?;
        let base = account
            .base_url
            .as_deref()
            .unwrap_or("https://chatgpt.com/backend-api/codex");
        let mut request = self
            .client
            .get(crate::relay::endpoint(base, "/models"))
            .query(&[("client_version", crate::relay::CODEX_VERSION)])
            .bearer_auth(&account.access_token)
            .header("originator", "codex_cli_rs")
            .header("user-agent", crate::relay::CODEX_USER_AGENT)
            .timeout(std::time::Duration::from_secs(15));
        if let Some(id) = &account.account_id {
            request = request.header("chatgpt-account-id", id);
        }
        let response = request.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value: Value = response.json().await.ok()?;
        let rows = value.get("models")?.as_array()?;
        let mut models = Vec::new();
        for row in rows {
            let Some(id) = row
                .get("slug")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            if row.get("supported_in_api").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let known = catalog()
                .iter()
                .find(|m| m.provider == SubscriptionProvider::Codex && m.id == id);
            let levels: Vec<&str> = row
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|level| level.get("effort").and_then(Value::as_str))
                .collect();
            models.push(Model {
                id: id.to_owned(),
                upstream_id: id.to_owned(),
                provider: SubscriptionProvider::Codex,
                name: row
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
                context_length: row
                    .get("context_window")
                    .and_then(Value::as_u64)
                    .or_else(|| known.and_then(|m| m.context_length)),
                max_output_tokens: row
                    .get("max_output_tokens")
                    .and_then(Value::as_u64)
                    .or_else(|| known.and_then(|m| m.max_output_tokens)),
                thinking: if levels.is_empty() {
                    known.and_then(|m| m.thinking.clone())
                } else {
                    Some(json!({"levels":levels}))
                },
                ..Model::default()
            });
        }
        Some(models)
    }

    /// What an account's own live listing named, for routing its requests.
    fn remember_listing(&self, account: &str, live: &[Model]) {
        self.listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                account.to_owned(),
                live.iter().map(|m| m.id.clone()).collect(),
            );
    }

    /// Google renames Antigravity models without notice, so the bundled ids
    /// go stale; the account's own `fetchAvailableModels` list is the truth.
    async fn antigravity_models(&self, stored: &AccountCredential) -> Option<Vec<Model>> {
        let account = self.oauth.credential(&stored.id).await.ok()?;
        let project = account.project_id.as_deref()?;
        let base = account
            .base_url
            .as_deref()
            .unwrap_or("https://daily-cloudcode-pa.googleapis.com");
        let response = self
            .client
            .post(crate::relay::endpoint(
                base,
                "/v1internal:fetchAvailableModels",
            ))
            .bearer_auth(&account.access_token)
            .header("User-Agent", antigravity_user_agent())
            .json(&json!({ "project": project }))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value = response.json::<Value>().await.ok()?;
        let models = antigravity_catalog(&value);
        (!models.is_empty()).then_some(models)
    }

    pub async fn candidates(
        &self,
        requested: &str,
        provider_hint: Option<&str>,
        session: Option<&str>,
    ) -> Result<(String, Vec<AccountCredential>), String> {
        self.candidates_for(requested, provider_hint, session, None)
            .await
    }

    /// The accounts `client` may use for `requested`, best first. A client
    /// sees only the accounts its access allows, and an account where the
    /// other holders' shares leave it nothing drops out like a reserve.
    pub async fn candidates_for(
        &self,
        requested: &str,
        provider_hint: Option<&str>,
        session: Option<&str>,
        client: Option<&crate::clients::Client>,
    ) -> Result<(String, Vec<AccountCredential>), String> {
        self.candidates_for_replay(requested, provider_hint, session, client, None)
            .await
    }

    pub(crate) async fn candidates_for_replay(
        &self,
        requested: &str,
        provider_hint: Option<&str>,
        session: Option<&str>,
        client: Option<&crate::clients::Client>,
        owner: Option<&str>,
    ) -> Result<(String, Vec<AccountCredential>), String> {
        let config = self.config.read().await.clone();
        if config.disabled_models.contains(requested) {
            return Err("model is disabled".into());
        }
        // A prefix names a provider or a custom endpoint route; anything else
        // is part of the model id (OpenRouter ids contain slashes).
        let routes = self.accounts.routes().await;
        let pool_of = |name: &str| -> Option<(SubscriptionProvider, Option<String>)> {
            SubscriptionProvider::parse(name)
                .map(|provider| (provider, None))
                .or_else(|| {
                    routes.contains(name).then(|| {
                        (
                            SubscriptionProvider::OpenaiCompatible,
                            Some(name.to_owned()),
                        )
                    })
                })
        };
        let (prefix, model) = requested
            .split_once('/')
            .and_then(|(p, m)| pool_of(p).map(|pool| (Some(pool), m)))
            .unwrap_or((None, requested));
        let explicit = match prefix {
            Some(pool) => Some(pool),
            None => match provider_hint.or(config.settings.default_provider.as_deref()) {
                Some(name) => Some(pool_of(name).ok_or("unknown provider")?),
                None => None,
            },
        };
        let all_models = self.models.read().await;
        let pools: Vec<(SubscriptionProvider, Option<String>)> = if let Some(pool) = explicit {
            vec![pool]
        } else {
            let mut pools = vec![];
            for m in all_models
                .iter()
                .filter(|m| m.upstream_id == model && m.enabled)
            {
                let pool = (m.provider, m.route.clone());
                if !pools.contains(&pool) {
                    pools.push(pool);
                }
            }
            // Native names select the matching native provider ahead of aggregators.
            pools.sort_by_key(|(p, _)| match (model, *p) {
                (m, SubscriptionProvider::Codex) if m.starts_with("gpt-") => 0,
                (m, SubscriptionProvider::Claude) if m.starts_with("claude-") => 0,
                (m, SubscriptionProvider::Xai) if m.starts_with("grok-") => 0,
                _ => 1,
            });
            pools
        };
        drop(all_models);
        let mut ordered = vec![];
        // The earliest reset among accounts only shares held back.
        let mut held_back: Option<(String, i64)> = None;
        for (provider, route) in pools {
            let pool = route.as_deref().unwrap_or(provider.as_str());
            if owner.is_some() && provider != SubscriptionProvider::Antigravity {
                continue;
            }
            if config.disabled_models.contains(&format!("{pool}/{model}")) {
                continue;
            }
            let now = time::OffsetDateTime::now_utc();
            let free_before = config.settings.free_before_reset_seconds();
            let mut eligible = vec![];
            let mut candidates = vec![];
            let mut pool_accounts = self
                .accounts
                .eligible_in_for_replay(provider, route.as_deref(), Some(model), owner)
                .await;
            let mut resting = false;
            loop {
                for a in pool_accounts {
                    if owner.is_some_and(|owner| a.id != owner) {
                        continue;
                    }
                    if client.is_some_and(|c| !c.access.allows(&a)) {
                        continue;
                    }
                    if !self.serves(&a.id, model, a.provider) {
                        continue;
                    }
                    let quota = self.quotas.get(&a.id).await;
                    if self.reserve_excludes_with(&a, quota.as_ref(), now, free_before) {
                        continue;
                    }
                    if let Some(reset) = self.clients.share_blocks(
                        client,
                        &a,
                        quota.as_ref(),
                        now.unix_timestamp(),
                        free_before,
                    ) {
                        if held_back.as_ref().is_none_or(|(_, r)| reset < *r) {
                            held_back = Some((pool.to_owned(), reset));
                        }
                        continue;
                    }
                    candidates.push(Candidate {
                        id: a.id.clone(),
                        remaining_percent: quota.as_ref().and_then(|q| {
                            q.weekly_remaining_percent()
                                .or_else(|| q.remaining_percent())
                        }),
                        weekly_reset: quota
                            .as_ref()
                            .and_then(|q| q.weekly_reset())
                            .and_then(crate::subscriptions::parse_timestamp)
                            .map(|t| t.unix_timestamp()),
                        credits_only: quota.as_ref().is_some_and(|q| q.credits_only(now)),
                    });
                    eligible.push(a);
                }
                // A rate limit rests one account so the pool can rotate. With no
                // usable account to rotate to, refusing every client for the whole
                // rest blames a missing subscription; the upstream decides instead.
                if !eligible.is_empty() || owner.is_some() || resting {
                    break;
                }
                resting = true;
                pool_accounts = self
                    .accounts
                    .resting_in(provider, route.as_deref(), Some(model))
                    .await;
            }
            let strategy = config
                .settings
                .provider_strategies
                .get(pool)
                .or_else(|| config.settings.provider_strategies.get(provider.as_str()))
                .copied()
                .unwrap_or(config.settings.strategy);
            let now = now.unix_timestamp();
            if let Some(key) = session {
                let pinned = self
                    .affinity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(pool, key);
                // A pin holds unless it would spend usage credits while another
                // account still has subscription quota, or reset-first has an
                // eligible account with an earlier weekly reset.
                let any_quota = candidates.iter().any(|c| !c.credits_only);
                if let Some(index) = pinned.and_then(|id| {
                    candidates.iter().position(|c| {
                        c.id == id
                            && (!c.credits_only || !any_quota)
                            && Scheduler::pin_holds(strategy, &candidates, &id, now)
                    })
                }) {
                    let mut pinned = eligible;
                    pinned.swap(0, index);
                    return Ok((model.into(), pinned));
                }
            }
            let order = self
                .scheduler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .order(pool, strategy, &candidates, now);
            for id in order {
                if let Some(a) = eligible.iter().find(|a| a.id == id) {
                    ordered.push(a.clone());
                }
            }
            // Automatic fallback stays inside one provider's subscription pool.
            if !ordered.is_empty() {
                break;
            }
        }
        if let (true, Some((pool, reset))) = (ordered.is_empty(), held_back) {
            let until = time::OffsetDateTime::from_unix_timestamp(reset)
                .ok()
                .and_then(|t| {
                    t.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .unwrap_or_default();
            Err(format!(
                "{} {pool} is used up until {until}; its overbooked share cap is reached or quota is reserved for other clients",
                crate::clients::SHARE_USED_UP
            ))
        } else if ordered.is_empty() {
            Err("no available subscription for this model; connect an account or wait for its quota reset".into())
        } else {
            Ok((model.into(), ordered))
        }
    }

    /// The account would pay with usage credits although another eligible
    /// account of its provider still has subscription quota for `model`.
    pub async fn credits_avoidable(&self, account: &AccountCredential, model: &str) -> bool {
        let now = time::OffsetDateTime::now_utc();
        if !self
            .quotas
            .get(&account.id)
            .await
            .is_some_and(|q| q.credits_only(now))
        {
            return false;
        }
        for other in self
            .accounts
            .eligible_in(account.provider, account.route.as_deref(), Some(model))
            .await
        {
            if other.id != account.id
                && !self
                    .quotas
                    .get(&other.id)
                    .await
                    .is_some_and(|q| q.credits_only(now))
                && !self.reserve_reached(&other).await
            {
                return true;
            }
        }
        false
    }

    /// Douane leaves the account's reserve unused: an account-wide quota
    /// window at or below it takes the account out of rotation until that
    /// window resets. Unknown quotas never trigger it. This check only reads;
    /// request routing uses [`Self::reserve_excludes`].
    pub async fn reserve_reached(&self, account: &AccountCredential) -> bool {
        let free_before = self.free_before_reset_seconds().await;
        let snapshot = self.quotas.get(&account.id).await;
        reserve_reached_in(
            account.reserve(),
            &account.reserve_windows(),
            snapshot.as_ref(),
            time::OffsetDateTime::now_utc(),
            free_before,
        )
    }

    /// Routing check for the reserve. Close to or at the reserve, a snapshot
    /// older than a minute also starts a background quota refresh, so the
    /// account leaves rotation in time and returns soon after its reset.
    /// The client may not use this account: its access refuses it, or the
    /// other holders' shares leave it nothing there.
    pub async fn client_excludes(
        &self,
        client: Option<&crate::clients::Client>,
        account: &AccountCredential,
    ) -> bool {
        if client.is_some_and(|c| !c.access.allows(account)) {
            return true;
        }
        let free_before = self.free_before_reset_seconds().await;
        let snapshot = self.quotas.get(&account.id).await;
        self.clients
            .share_blocks(
                client,
                account,
                snapshot.as_ref(),
                time::OffsetDateTime::now_utc().unix_timestamp(),
                free_before,
            )
            .is_some()
    }

    pub async fn reserve_excludes(&self, account: &AccountCredential) -> bool {
        let free_before = self.free_before_reset_seconds().await;
        let snapshot = self.quotas.get(&account.id).await;
        self.reserve_excludes_with(
            account,
            snapshot.as_ref(),
            time::OffsetDateTime::now_utc(),
            free_before,
        )
    }

    /// Seconds before an account's weekly reset when its reserve and client
    /// shares stop applying.
    pub async fn free_before_reset_seconds(&self) -> i64 {
        self.config
            .read()
            .await
            .settings
            .free_before_reset_seconds()
    }

    fn reserve_excludes_with(
        &self,
        account: &AccountCredential,
        snapshot: Option<&crate::subscriptions::QuotaSnapshot>,
        now: time::OffsetDateTime,
        free_before: i64,
    ) -> bool {
        let (Some(reserve), Some(snapshot)) = (account.reserve(), snapshot) else {
            return false;
        };
        if snapshot.longest_window_ends_within(free_before, now.unix_timestamp()) {
            return false;
        }
        let windows = account.reserve_windows();
        let near = snapshot
            .reserve_window_remaining(&windows, now)
            .is_some_and(|lowest| lowest <= reserve + 10.0);
        let stale = crate::subscriptions::parse_timestamp(&snapshot.updated_at)
            .is_none_or(|at| now - at > time::Duration::minutes(1));
        if near && stale {
            self.refresh_soon(&account.id);
        }
        reserve_reached_in(Some(reserve), &windows, Some(snapshot), now, 0)
    }

    fn refresh_soon(&self, id: &str) {
        let started = std::time::Instant::now();
        let throttle = std::time::Duration::from_secs(60);
        {
            let mut last = self
                .reserve_refresh
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Expired entries go, so deleted accounts do not accumulate.
            last.retain(|_, at| started.duration_since(*at) < throttle);
            if last.contains_key(id) {
                return;
            }
            last.insert(id.to_owned(), started);
        }
        let quotas = self.quotas.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _ = quotas.refresh_background(&id).await;
        });
    }

    /// A read asked by hand, for one account or all of them: a failure shows
    /// at once.
    pub async fn refresh_quotas(&self, id: Option<&str>) {
        use futures_util::{StreamExt, stream};
        let ids: Vec<String> = self
            .accounts
            .list()
            .await
            .into_iter()
            .filter(|a| id.is_none_or(|id| a.id == id))
            .map(|a| a.id)
            .collect();
        stream::iter(ids)
            .for_each_concurrent(4, |id| async move {
                let _ = self.quotas.refresh(&id).await;
                self.observe_usage(&id).await;
            })
            .await;
    }

    /// Attributes the latest rise of an account's windows to its clients.
    pub async fn observe_usage(&self, id: &str) {
        if let Some(snapshot) = self.quotas.get(id).await {
            self.clients
                .observe(&snapshot, time::OffsetDateTime::now_utc().unix_timestamp());
        }
    }

    /// The poller's pass: reads only the accounts whose snapshot is
    /// `quota_refresh_seconds` old or whose retry delay has passed.
    pub async fn refresh_due_quotas(&self) {
        use futures_util::{StreamExt, stream};
        let interval = self.config.read().await.settings.quota_refresh_seconds;
        let interval = time::Duration::seconds(interval as i64);
        let ids = self
            .quotas
            .due(interval, time::OffsetDateTime::now_utc())
            .await;
        stream::iter(ids)
            .for_each_concurrent(4, |id| async move {
                if self.quotas.refresh_background(&id).await.is_some() {
                    self.observe_usage(&id).await;
                }
            })
            .await;
        let existing = self
            .accounts
            .list()
            .await
            .into_iter()
            .map(|a| a.id)
            .collect();
        let clients = self.clients.clone();
        let _ = tokio::task::spawn_blocking(move || {
            clients.forget_accounts(&existing);
            clients.save_usage();
        })
        .await;
    }
}

/// A valid reserve and a snapshot whose account-wide window sits at or below
/// it, unless the weekly reset is within `free_before` seconds. Shared by
/// routing and the management listing.
pub fn reserve_reached_in(
    reserve: Option<f64>,
    windows: &[String],
    snapshot: Option<&crate::subscriptions::QuotaSnapshot>,
    now: time::OffsetDateTime,
    free_before: i64,
) -> bool {
    reserve
        .filter(|percent| crate::subscriptions::valid_reserve(*percent))
        .zip(snapshot)
        .filter(|(_, snapshot)| {
            !snapshot.longest_window_ends_within(free_before, now.unix_timestamp())
        })
        .is_some_and(|(reserve, snapshot)| snapshot.reserve_reached(reserve, windows, now))
}

/// Agent models from a `fetchAvailableModels` answer, in Antigravity's own
/// order. Tab-completion and internal models are skipped, and an alias that
/// repeats a display name already listed is dropped.
fn antigravity_catalog(value: &Value) -> Vec<Model> {
    let Some(available) = value["models"].as_object() else {
        return vec![];
    };
    let hidden: BTreeSet<&str> = value["tabModelIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut order: Vec<&str> = value["agentModelSorts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sort| sort["groups"].as_array())
        .flatten()
        .filter_map(|group| group["modelIds"].as_array())
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut rest: Vec<&str> = available.keys().map(String::as_str).collect();
    rest.sort_by_key(|id| (catalog_rank(SubscriptionProvider::Antigravity, id), *id));
    order.extend(rest);
    let mut seen_ids = BTreeSet::new();
    let mut seen_names = BTreeSet::new();
    let mut models = vec![];
    for id in order {
        let Some(entry) = available.get(id) else {
            continue;
        };
        let Some(name) = entry["displayName"].as_str() else {
            continue;
        };
        if hidden.contains(id)
            || entry["isInternal"].as_bool() == Some(true)
            || !seen_ids.insert(id)
            || !seen_names.insert(name)
        {
            continue;
        }
        let bundled = catalog()
            .iter()
            .find(|m| m.provider == SubscriptionProvider::Antigravity && m.id == id);
        let thinking = bundled.and_then(|m| m.thinking.clone()).or_else(|| {
            (entry["supportsThinking"].as_bool() == Some(true)).then(|| {
                json!({
                    "min": entry["minThinkingBudget"].as_u64().unwrap_or(1),
                    "dynamic_allowed": entry["thinkingBudget"].as_i64() == Some(-1),
                })
            })
        });
        models.push(Model {
            id: id.into(),
            provider: SubscriptionProvider::Antigravity,
            name: name.into(),
            context_length: entry["maxTokens"]
                .as_u64()
                .or_else(|| bundled.and_then(|m| m.context_length)),
            max_output_tokens: entry["maxOutputTokens"]
                .as_u64()
                .or_else(|| bundled.and_then(|m| m.max_output_tokens)),
            enabled: true,
            upstream_id: String::new(),
            thinking,
            ..Model::default()
        });
    }
    models
}

/// Paths of Copilot's API that douane can forward to.
const COPILOT_PATHS: [&str; 3] = ["/v1/messages", "/responses", "/chat/completions"];

/// The chat models a Copilot account's plan enables, from its `/models`
/// answer. A model whose policy is not `enabled` is refused upstream, and
/// one without `supported_endpoints` speaks Chat Completions only.
pub(crate) fn copilot_catalog(value: &Value) -> Vec<Model> {
    let Some(rows) = value["data"].as_array() else {
        return vec![];
    };
    let mut models: Vec<Model> = vec![];
    for row in rows {
        let Some(id) = row["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        if row.pointer("/policy/state").and_then(Value::as_str) != Some("enabled")
            || row.pointer("/capabilities/type").and_then(Value::as_str) != Some("chat")
            || models.iter().any(|m| m.id == id)
        {
            continue;
        }
        let endpoints: Vec<String> = match row["supported_endpoints"].as_array() {
            Some(paths) => paths
                .iter()
                .filter_map(Value::as_str)
                .filter(|path| COPILOT_PATHS.contains(path))
                .map(str::to_owned)
                .collect(),
            None => vec!["/chat/completions".into()],
        };
        if endpoints.is_empty() {
            continue;
        }
        let limits = row.pointer("/capabilities/limits");
        let levels: Vec<Value> = row
            .pointer("/capabilities/supports/reasoning_effort")
            .and_then(Value::as_array)
            .map(|levels| levels.iter().filter(|l| l.is_string()).cloned().collect())
            .unwrap_or_default();
        models.push(Model {
            id: id.into(),
            provider: SubscriptionProvider::Copilot,
            name: row["name"].as_str().unwrap_or(id).into(),
            context_length: limits.and_then(|l| {
                l["max_context_window_tokens"]
                    .as_u64()
                    .or_else(|| l["max_prompt_tokens"].as_u64())
            }),
            max_output_tokens: limits.and_then(|l| l["max_output_tokens"].as_u64()),
            enabled: true,
            thinking: (!levels.is_empty()).then(|| json!({ "levels": levels })),
            endpoints,
            ..Model::default()
        });
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Strategy;
    use crate::subscriptions::{QuotaCredits, QuotaSnapshot, QuotaWindow};
    use serde_json::json;

    #[test]
    fn antigravity_catalog_keeps_agent_models_in_upstream_order() {
        let value = serde_json::json!({
            "models": {
                "chat_20706": {"isInternal": true, "maxTokens": 16384},
                "tab_flash_lite_preview": {"maxTokens": 16384},
                "gemini-3.8-flash-tiered": {"maxTokens": 1048576},
                "gemini-2.5-flash": {"displayName": "Gemini 3.5 Flash Lite", "maxTokens": 1048576},
                "gemini-3.5-flash-lite": {"displayName": "Gemini 3.5 Flash Lite", "maxTokens": 1048576},
                "claude-opus-4-6-thinking": {"displayName": "Claude Opus 4.6 (Thinking)", "maxTokens": 250000, "maxOutputTokens": 64000},
                "gemini-3.8-flash-high": {"displayName": "Gemini 3.8 Flash (High)", "maxTokens": 1048576, "maxOutputTokens": 65536},
                "gemini-new": {"displayName": "Gemini New", "supportsThinking": true, "minThinkingBudget": 32, "thinkingBudget": -1}
            },
            "agentModelSorts": [{"groups": [{"modelIds": ["gemini-3.8-flash-high", "claude-opus-4-6-thinking"]}]}],
            "tabModelIds": ["chat_20706", "tab_flash_lite_preview"]
        });
        let models = super::antigravity_catalog(&value);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini-3.8-flash-high",
                "claude-opus-4-6-thinking",
                "gemini-3.5-flash-lite",
                "gemini-new"
            ]
        );
        assert_eq!(models[1].context_length, Some(250000));
        assert_eq!(models[1].max_output_tokens, Some(64000));
        assert_eq!(
            models[3].thinking,
            Some(serde_json::json!({"min": 32, "dynamic_allowed": true}))
        );
    }

    #[test]
    fn bundled_antigravity_ids_are_the_live_agent_models() {
        let ids: Vec<&str> = super::catalog()
            .iter()
            .filter(|m| m.provider == SubscriptionProvider::Antigravity)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids[0], "gemini-3.8-flash-high");
        assert!(ids.contains(&"claude-opus-4-6-thinking"));
        assert!(!ids.contains(&"gemini-pro-agent"));
    }

    fn credits_only(id: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: id.into(),
            provider: SubscriptionProvider::Codex,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                label: "Codex Weekly".into(),
                remaining_percent: Some(0.0),
                reset_at: Some("2099-01-01T00:00:00Z".into()),
                used: Some(100.0),
                limit: Some(100.0),
            }],
            updated_at: crate::subscriptions::now(),
            error: None,
            subscription: None,
            credits: vec![QuotaCredits {
                id: "codex-credits".into(),
                label: "Credits".into(),
                used: None,
                limit: None,
                remaining: Some(10.0),
                unit: "credits".into(),
                reset_at: None,
                unlimited: Some(false),
                enabled: Some(true),
            }],
            verification_required: false,
            verification_available: false,
        }
    }

    fn weekly(id: &str, reset: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                label: "Codex Weekly".into(),
                remaining_percent: Some(60.0),
                reset_at: Some(reset.into()),
                used: Some(40.0),
                limit: Some(100.0),
            }],
            credits: vec![],
            ..credits_only(id)
        }
    }

    #[tokio::test]
    async fn overbooked_account_caps_fail_over_and_match_public_quotas() {
        use axum::{body::Body, http::Request};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["a", "b"] {
            ids.push(
                state
                    .accounts
                    .import(
                        json!({"type":"codex","access_token":"fixture",
                "account_id":name,"name":name}),
                        None,
                    )
                    .await
                    .unwrap()
                    .id,
            );
        }
        let (small, key) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("small".into()),
                shares: Some([("codex".into(), 20.0)].into()),
                account_shares: Some([(ids[0].clone(), 10.0)].into()),
                ..Default::default()
            })
            .unwrap();
        state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("large".into()),
                shares: Some([("codex".into(), 95.0)].into()),
                ..Default::default()
            })
            .unwrap();
        let at = |id: &str, used: f64| {
            let mut snapshot = weekly(id, "2099-01-08T00:00:00Z");
            snapshot.windows[0].remaining_percent = Some(100.0 - used);
            snapshot
        };
        for id in &ids {
            state.quotas.insert_for_test(at(id, 0.0)).await;
            state.observe_usage(id).await;
        }
        state.clients.call_ended(&ids[0], Some(&small.id), 1.0);
        state.quotas.insert_for_test(at(&ids[0], 10.0)).await;
        state.observe_usage(&ids[0]).await;
        let (_, candidates) = state
            .candidates_for("gpt-5.5", Some("codex"), None, Some(&small))
            .await
            .unwrap();
        assert_eq!(
            candidates.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            vec![ids[1].clone()]
        );

        let response = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/quotas")
                    .header("authorization", format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let view: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let entries = view["providers"][0]["entries"].as_array().unwrap();
        for (id, expected) in [(&ids[0], 0.0), (&ids[1], 100.0)] {
            let entry = entries.iter().find(|e| e["id"] == id.as_str()).unwrap();
            assert_eq!(entry["windows"][0]["remaining_percent"], expected, "{view}");
        }
        // With access restricted to the spent account, the HTTP router returns 429.
        state
            .clients
            .update(
                &small.id,
                crate::clients::ClientPatch {
                    access: Some(crate::clients::Access {
                        all: false,
                        pools: Default::default(),
                        accounts: [ids[0].clone()].into(),
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        let response = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("authorization", format!("Bearer {key}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"codex/gpt-5.5","input":"test"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&bytes).contains("overbooked share cap"));
    }

    #[tokio::test]
    async fn a_client_past_what_other_shares_leave_it_is_held_back_with_429() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"a","name":"a"}),
                None,
            )
            .await
            .unwrap();
        let patch = |name: &str, share: Option<f64>| crate::clients::ClientPatch {
            name: Some(name.into()),
            shares: share.map(|s| [("codex".to_owned(), s)].into()),
            ..Default::default()
        };
        let (famille, _) = state.clients.create(patch("famille", Some(30.0))).unwrap();
        let (perso, _) = state.clients.create(patch("perso", None)).unwrap();
        let (outsider, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                access: Some(crate::clients::Access {
                    all: false,
                    pools: ["claude".to_owned()].into(),
                    accounts: Default::default(),
                }),
                ..patch("claude only", None)
            })
            .unwrap();
        let at = |used: f64| QuotaSnapshot {
            windows: vec![QuotaWindow {
                remaining_percent: Some(100.0 - used),
                ..weekly(&account.id, "2099-01-08T00:00:00Z").windows[0].clone()
            }],
            ..weekly(&account.id, "2099-01-08T00:00:00Z")
        };
        state.quotas.insert_for_test(at(0.0)).await;
        state.observe_usage(&account.id).await;
        state.clients.call_ended(&account.id, Some(&perso.id), 10.0);
        state.quotas.insert_for_test(at(70.0)).await;
        state.observe_usage(&account.id).await;
        let first = |client: &crate::clients::Client| {
            let state = state.clone();
            let client = client.clone();
            async move {
                state
                    .candidates_for("gpt-5.5", Some("codex"), None, Some(&client))
                    .await
                    .map(|(_, accounts)| accounts[0].id.clone())
            }
        };
        let refused = first(&perso).await.unwrap_err();
        assert!(
            refused.starts_with(crate::clients::SHARE_USED_UP),
            "{refused}"
        );
        assert_eq!(
            crate::relay::refusal_status(&refused),
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(first(&famille).await.unwrap(), account.id);
        assert!(
            first(&outsider)
                .await
                .unwrap_err()
                .starts_with("no available subscription")
        );
    }

    #[tokio::test]
    async fn a_rate_limit_rests_an_account_without_closing_its_pool() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["main", "spare"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        let (main, spare) = (ids[0].clone(), ids[1].clone());
        let first = || async {
            state
                .candidates("gpt-5.5", Some("codex"), None)
                .await
                .map(|(_, accounts)| accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>())
        };
        // With another account ready, the rested one stays out of rotation.
        state
            .accounts
            .cooldown(&main, 400, "upstream temporarily unavailable")
            .await
            .unwrap();
        assert_eq!(first().await.unwrap(), vec![spare.clone()]);
        // A ready account outside the client's access must not hide the
        // rested one it may use.
        let (main_only, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("main only".into()),
                access: Some(crate::clients::Access {
                    all: false,
                    pools: Default::default(),
                    accounts: [main.clone()].into(),
                }),
                ..Default::default()
            })
            .unwrap();
        let (_, allowed) = state
            .candidates_for("gpt-5.5", Some("codex"), None, Some(&main_only))
            .await
            .unwrap();
        assert_eq!(allowed[0].id, main);
        // With none ready, the pool still serves: the upstream answers the
        // client itself rather than a 503 naming a missing subscription.
        state
            .accounts
            .cooldown(&spare, 60, "websocket upstream rate limited")
            .await
            .unwrap();
        let mut resting = first().await.unwrap();
        resting.sort();
        let mut both = vec![main.clone(), spare.clone()];
        both.sort();
        assert_eq!(resting, both);
        // An empty window or a refused login still closes the pool: retrying
        // either cannot succeed before its reset or a new login.
        state
            .accounts
            .cooldown(&main, 300, "authentication needs reconnecting")
            .await
            .unwrap();
        state
            .accounts
            .cooldown(&spare, 3600, "websocket upstream quota exceeded")
            .await
            .unwrap();
        assert!(
            first()
                .await
                .unwrap_err()
                .starts_with("no available subscription")
        );
    }

    #[tokio::test]
    async fn reset_first_moves_a_pinned_session_to_the_nearest_reset() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["later", "soon"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        let (later, soon) = (ids[0].clone(), ids[1].clone());
        state
            .quotas
            .insert_for_test(weekly(&later, "2099-01-08T00:00:00Z"))
            .await;
        state
            .quotas
            .insert_for_test(weekly(&soon, "2099-01-01T00:00:00Z"))
            .await;
        // A failover or an earlier strategy left the conversation on "later".
        state
            .affinity
            .lock()
            .unwrap()
            .bind("codex", "session", &later);
        async fn first(state: &AppState, strategy: Strategy) -> String {
            state.config.write().await.settings.strategy = strategy;
            let (_, accounts) = state
                .candidates("gpt-5.5", Some("codex"), Some("session"))
                .await
                .unwrap();
            accounts[0].id.clone()
        }
        for strategy in [Strategy::Smart, Strategy::RoundRobin, Strategy::FillFirst] {
            assert_eq!(first(&state, strategy).await, later, "{strategy:?}");
        }
        assert_eq!(first(&state, Strategy::ResetFirst).await, soon);

        // While the nearest reset is unavailable, the pin stays on "later".
        state.accounts.set_enabled(&soon, false).await.unwrap();
        assert_eq!(first(&state, Strategy::ResetFirst).await, later);
    }

    #[tokio::test]
    async fn a_session_leaves_usage_credits_only_for_subscription_quota() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["paid", "quota"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        state.quotas.insert_for_test(credits_only(&ids[0])).await;
        state
            .affinity
            .lock()
            .unwrap()
            .bind("codex", "session", &ids[0]);
        let paid = state.accounts.get(&ids[0]).await.unwrap();
        async fn first(state: &AppState) -> String {
            let (_, accounts) = state
                .candidates("gpt-5.5", Some("codex"), Some("session"))
                .await
                .unwrap();
            accounts[0].id.clone()
        }
        assert_eq!(first(&state).await, ids[1]);
        assert!(state.credits_avoidable(&paid, "gpt-5.5").await);

        // Once no subscription quota remains, the pinned account pays.
        state.accounts.set_enabled(&ids[1], false).await.unwrap();
        assert_eq!(first(&state).await, ids[0]);
        assert!(!state.credits_avoidable(&paid, "gpt-5.5").await);
    }

    #[tokio::test]
    async fn claude_haiku_5_5_goes_to_a_claude_account() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let claude = state
            .accounts
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        state.refresh_models().await;
        let (upstream, accounts) = state
            .candidates("claude-haiku-5-5", None, None)
            .await
            .unwrap();
        assert_eq!(upstream, "claude-haiku-5-5");
        assert_eq!(accounts[0].id, claude.id);
    }

    #[tokio::test]
    async fn an_account_at_its_reserve_leaves_rotation_and_releases_its_pin() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["kept", "spare"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        let (kept, spare) = (ids[0].clone(), ids[1].clone());
        let mut low = weekly(&kept, "2099-01-01T00:00:00Z");
        low.windows[0].remaining_percent = Some(5.0);
        state.quotas.insert_for_test(low).await;
        state
            .quotas
            .insert_for_test(weekly(&spare, "2099-01-08T00:00:00Z"))
            .await;
        state
            .affinity
            .lock()
            .unwrap()
            .bind("codex", "session", &kept);
        async fn order(state: &AppState) -> Result<Vec<String>, String> {
            state
                .candidates("gpt-5.5", Some("codex"), Some("session"))
                .await
                .map(|(_, accounts)| accounts.into_iter().map(|a| a.id).collect())
        }
        // Without a reserve, 5% left is still spent.
        assert_eq!(order(&state).await.unwrap()[0], kept);

        state
            .accounts
            .update(&kept, None, None, Some(Some(5.0)), None)
            .await
            .unwrap();
        assert_eq!(order(&state).await.unwrap(), vec![spare.clone()]);
        let reserved = state.accounts.get(&kept).await.unwrap();
        assert!(state.reserve_reached(&reserved).await);

        // The reserve is a hard floor: no other account means no request.
        state.accounts.set_enabled(&spare, false).await.unwrap();
        assert!(order(&state).await.is_err());

        // Above the reserve, the account serves again.
        state
            .accounts
            .update(&kept, None, None, Some(Some(4.0)), None)
            .await
            .unwrap();
        assert_eq!(order(&state).await.unwrap(), vec![kept]);
    }

    #[tokio::test]
    async fn a_reserve_stops_applying_hours_before_the_weekly_reset() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"kept","name":"kept"}),
                None,
            )
            .await
            .unwrap();
        state
            .accounts
            .update(&account.id, None, None, Some(Some(5.0)), None)
            .await
            .unwrap();
        let reset_in = |hours: i64| {
            (time::OffsetDateTime::now_utc() + time::Duration::hours(hours))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let low = |reset: String| {
            let mut low = weekly(&account.id, &reset);
            low.windows[0].remaining_percent = Some(5.0);
            low
        };
        async fn serves(state: &AppState) -> bool {
            state
                .candidates("gpt-5.5", Some("codex"), None)
                .await
                .is_ok()
        }
        let credential = state.accounts.get(&account.id).await.unwrap();
        // Seven hours before the reset, the reserve still holds.
        state.quotas.insert_for_test(low(reset_in(7))).await;
        assert!(!serves(&state).await);
        assert!(state.reserve_reached(&credential).await);
        // Within the default six hours, the rest of the week gets used.
        state.quotas.insert_for_test(low(reset_in(5))).await;
        assert!(serves(&state).await);
        assert!(!state.reserve_reached(&credential).await);
        assert!(!state.reserve_excludes(&credential).await);
        // At 0 the reserve holds to the end.
        state.config.write().await.settings.free_before_reset_hours = 0;
        assert!(!serves(&state).await);
    }

    #[tokio::test]
    async fn a_stale_reached_reserve_refreshes_and_a_bad_stored_value_is_ignored() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"low"}),
                None,
            )
            .await
            .unwrap();
        state
            .accounts
            .update(&account.id, None, None, Some(Some(5.0)), None)
            .await
            .unwrap();
        let mut low = weekly(&account.id, "2099-01-01T00:00:00Z");
        low.windows[0].remaining_percent = Some(2.0);
        low.updated_at = "2026-01-01T00:00:00Z".into();
        state.quotas.insert_for_test(low).await;
        let credential = state.accounts.get(&account.id).await.unwrap();
        // The listing check reads only.
        assert!(state.reserve_reached(&credential).await);
        assert!(state.reserve_refresh.lock().unwrap().is_empty());
        // Removing the account keeps the spawned refresh off the network.
        state.accounts.delete(&account.id).await.unwrap();
        assert!(state.reserve_excludes(&credential).await);
        assert!(
            state
                .reserve_refresh
                .lock()
                .unwrap()
                .contains_key(&account.id)
        );

        // A hand-edited file outside 0 < reserve < 100 never parks the account.
        let mut edited = credential.clone();
        for invalid in [150.0, f64::INFINITY, f64::NAN, -3.0] {
            edited.reserve_percent = Some(invalid);
            assert!(!state.reserve_reached(&edited).await, "{invalid}");
            assert_eq!(edited.summary().reserve_percent, None);
        }
    }
    #[tokio::test]
    async fn replay_owner_in_cooldown_still_obeys_reserves_and_disabled_models() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let owner=state.accounts.import(json!({"type":"antigravity","access_token":"synthetic-reserve","models":["synthetic"]}),None).await.unwrap();
        state
            .accounts
            .cooldown(&owner.id, 60, "synthetic cooldown")
            .await
            .unwrap();
        let pick = || {
            state.candidates_for_replay("antigravity/synthetic", None, None, None, Some(&owner.id))
        };
        assert_eq!(pick().await.unwrap().1[0].id, owner.id);
        let mut snapshot = weekly(&owner.id, "2099-01-08T00:00:00Z");
        snapshot.provider = SubscriptionProvider::Antigravity;
        state.quotas.insert_for_test(snapshot).await;
        state
            .accounts
            .update(&owner.id, None, None, Some(Some(70.0)), None)
            .await
            .unwrap();
        assert!(
            pick().await.is_err(),
            "cooldown ownership cannot spend the reserved quota"
        );
        state
            .accounts
            .update(&owner.id, None, None, Some(None), None)
            .await
            .unwrap();
        state
            .config
            .write()
            .await
            .disabled_models
            .insert("antigravity/synthetic".into());
        assert_eq!(pick().await.err().unwrap(), "model is disabled");
    }
}
