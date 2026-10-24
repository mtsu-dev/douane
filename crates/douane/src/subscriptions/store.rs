use super::{SubscriptionError, now, parse_timestamp, random_id, timestamp};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tokio::sync::{Mutex, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionProvider {
    Codex,
    Claude,
    Gemini,
    Antigravity,
    Qwen,
    Kimi,
    Xai,
    OpencodeGo,
    Copilot,
    Meta,
    OpenaiCompatible,
}

impl SubscriptionProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
            Self::Qwen => "qwen",
            Self::Kimi => "kimi",
            Self::Xai => "xai",
            Self::OpencodeGo => "opencode_go",
            Self::Copilot => "copilot",
            Self::Meta => "meta",
            Self::OpenaiCompatible => "openai_compatible",
        }
    }

    /// Whether douane can read this provider's quota. Custom endpoints and
    /// Qwen expose none, so they never reach the quota poller or overview.
    pub fn reports_quota(self) -> bool {
        !matches!(self, Self::OpenaiCompatible | Self::Qwen)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" | "chatgpt" => Some(Self::Codex),
            "claude" | "anthropic" => Some(Self::Claude),
            "gemini" | "gemini-cli" => Some(Self::Gemini),
            "antigravity" => Some(Self::Antigravity),
            "qwen" => Some(Self::Qwen),
            "kimi" | "kimi.ai" | "kimi-ai" => Some(Self::Kimi),
            "xai" | "grok" => Some(Self::Xai),
            "opencode_go" | "opencode-go" | "opencode" => Some(Self::OpencodeGo),
            "meta" | "muse" | "muse-code" => Some(Self::Meta),
            "copilot" | "github-copilot" | "github_copilot" => Some(Self::Copilot),
            "openai" | "openai_compatible" | "openai-compatible" => Some(Self::OpenaiCompatible),
            _ => None,
        }
    }
}

/// Provider-confirmed plan metadata. No credential or arbitrary provider body
/// can enter a management response through this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionTier {
    pub id: String,
    pub label: String,
    pub multiplier: Option<u32>,
    pub source: String,
}

impl SubscriptionTier {
    pub(crate) fn detect(
        provider: SubscriptionProvider,
        plan: Option<&str>,
        rate_tier: Option<&str>,
        source: &'static str,
    ) -> Option<Self> {
        let plan = plan.unwrap_or_default().trim().to_ascii_lowercase();
        let rate_tier = rate_tier.unwrap_or_default().trim().to_ascii_lowercase();
        let (id, label, multiplier) = match provider {
            SubscriptionProvider::Claude => match rate_tier.as_str() {
                "default_claude_max_20x" => ("max", "Claude Max", Some(20)),
                "default_claude_max_5x" => ("max", "Claude Max", Some(5)),
                _ => match plan.trim_start_matches("claude_") {
                    "max" => ("max", "Claude Max", None),
                    "pro" => ("pro", "Claude Pro", None),
                    "team" => ("team", "Claude Team", None),
                    "enterprise" => ("enterprise", "Claude Enterprise", None),
                    "free" => ("free", "Claude Free", None),
                    _ => return None,
                },
            },
            SubscriptionProvider::Codex => match plan.as_str() {
                "pro" => ("pro", "Codex Pro", None),
                "prolite" => ("prolite", "Codex Pro Lite", None),
                "promax" => ("promax", "Codex Pro Max", None),
                "pro_20x" | "pro-20x" => ("pro", "Codex Pro", Some(20)),
                "pro_5x" | "pro-5x" => ("pro", "Codex Pro", Some(5)),
                "plus" => ("plus", "Codex Plus", None),
                "go" => ("go", "Codex Go", None),
                "team" => ("team", "Codex Team", None),
                "business" => ("business", "Codex Business", None),
                "enterprise" => ("enterprise", "Codex Enterprise", None),
                "free" | "guest" => ("free", "Codex Free", None),
                _ => return None,
            },
            SubscriptionProvider::Xai => match plan.replace([' ', '-'], "_").as_str() {
                "supergrok" => ("supergrok", "SuperGrok", None),
                "supergrok_heavy" | "heavy" => ("supergrok_heavy", "SuperGrok Heavy", None),
                "premium" | "x_premium" => ("premium", "X Premium", None),
                "premium_plus" | "x_premium+" => ("premium_plus", "X Premium+", None),
                "free" => ("free", "Grok Free", None),
                _ => return None,
            },
            SubscriptionProvider::OpencodeGo if plan == "go" || plan == "opencode_go" => {
                ("go", "OpenCode Go", None)
            }
            // GitHub names the plan by its SKU; education plans carry their own.
            SubscriptionProvider::Copilot => match plan.as_str() {
                sku if sku.contains("educational") || sku == "student" => {
                    ("student", "Copilot Student", None)
                }
                sku if sku.contains("plus") => ("pro_plus", "Copilot Pro+", None),
                "free" | "free_limited_copilot" => ("free", "Copilot Free", None),
                "pro" | "copilot_pro" | "individual" | "monthly_subscriber"
                | "yearly_subscriber" => ("pro", "Copilot Pro", None),
                "business" | "copilot_business" => ("business", "Copilot Business", None),
                "enterprise" | "copilot_enterprise" => ("enterprise", "Copilot Enterprise", None),
                _ => return None,
            },
            _ => return None,
        };
        Some(Self {
            id: id.into(),
            label: multiplier
                .map(|multiple| format!("{label} x{multiple}"))
                .unwrap_or_else(|| label.into()),
            multiplier,
            source: source.into(),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountSummary {
    pub id: String,
    pub name: String,
    pub provider: SubscriptionProvider,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub subscription: Option<SubscriptionTier>,
    pub enabled: bool,
    pub status: String,
    pub last_error: Option<String>,
    pub reserve_percent: Option<f64>,
    pub reserve_windows: Vec<String>,
    /// A custom endpoint's model prefix, address, wire format and model
    /// filter. Absent for subscriptions; never a credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Request credentials are available only inside the broker. Do not serialize
/// this type into management responses or derive Debug for it.
#[derive(Clone, Serialize, Deserialize)]
pub struct AccountCredential {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) provider: SubscriptionProvider,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    /// Meta device credential, distinct from its minted inference key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dca_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dca_expires_at: Option<String>,
    pub(crate) id_token: Option<String>,
    pub(crate) account_id: Option<String>,
    #[serde(default)]
    pub(crate) organization_id: Option<String>,
    pub(crate) base_url: Option<String>,
    #[serde(default)]
    pub(crate) models: Vec<String>,
    pub(crate) project_id: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) plan: Option<String>,
    #[serde(default)]
    pub(crate) subscription: Option<SubscriptionTier>,
    pub(crate) expires_at: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) cooldown_until: Option<String>,
    #[serde(default)]
    pub(crate) exhausted_until: Option<String>,
    #[serde(default)]
    pub(crate) exhausted_without_reset: bool,
    #[serde(default)]
    pub(crate) client_id: Option<String>,
    #[serde(default)]
    pub(crate) client_secret: Option<String>,
    #[serde(default)]
    pub(crate) token_endpoint: Option<String>,
    #[serde(default)]
    pub(crate) model_exhaustion: BTreeMap<String, Option<String>>,
    /// Share of every account-wide quota window that douane leaves unused.
    #[serde(default)]
    pub(crate) reserve_percent: Option<f64>,
    /// Window kinds the reserve watches; empty watches the longest window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) reserve_windows: Vec<String>,
    /// Model prefix of a custom endpoint (`deepseek` serves `deepseek/...`).
    /// Keys sharing a route rotate; keys on different routes never mix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<String>,
    /// Wire format a custom endpoint speaks: `chat`, `responses` or
    /// `messages`. Absent forwards the client's own format unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dialect: Option<String>,
}

/// A reserve is a share strictly between 0 and 100.
pub fn valid_reserve(percent: f64) -> bool {
    percent.is_finite() && percent > 0.0 && percent < 100.0
}

/// A route names a custom endpoint in model ids (`deepseek/deepseek-chat`):
/// 1 to 32 lowercase letters, digits, `-` or `_`, never a provider's name,
/// which would make `<route>/<model>` ambiguous.
pub fn valid_route(route: &str) -> bool {
    route.len() <= 32
        && route
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && route
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && SubscriptionProvider::parse(route).is_none()
}

/// Wire formats a custom endpoint can speak.
pub fn valid_dialect(dialect: &str) -> bool {
    matches!(dialect, "chat" | "responses" | "messages")
}

/// An upstream address: http or https, without credentials or a query.
pub fn valid_base_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
    })
}

/// Fields a custom endpoint can change in place; `None` keeps the value.
#[derive(Default)]
pub struct EndpointPatch {
    pub route: Option<String>,
    pub base_url: Option<String>,
    /// `Some(None)` returns to forwarding the client's own format.
    pub dialect: Option<Option<String>>,
    /// An empty list serves every model the endpoint lists.
    pub models: Option<Vec<String>>,
    pub api_key: Option<String>,
}

const ROUTE_RULE: &str = "route must be 1 to 32 lowercase letters, digits, - or _, and not a provider name such as claude, codex, openai or copilot";
const DIALECT_RULE: &str = "dialect must be chat, responses or messages";
const BASE_URL_RULE: &str =
    "base_url must be an http or https address without credentials or query";

/// Known reserve window kinds, deduplicated in shortest-first order, or
/// `None` when one is unknown.
pub fn reserve_windows(kinds: &[String]) -> Option<Vec<String>> {
    if kinds
        .iter()
        .any(|kind| !super::RESERVE_WINDOWS.contains(&kind.as_str()))
    {
        return None;
    }
    Some(
        super::RESERVE_WINDOWS
            .iter()
            .filter(|known| kinds.iter().any(|kind| kind == *known))
            .map(|known| known.to_string())
            .collect(),
    )
}

impl AccountCredential {
    /// The stored reserve, ignored when a hand-edited file holds a value
    /// outside the accepted range.
    pub(crate) fn reserve(&self) -> Option<f64> {
        self.reserve_percent
            .filter(|percent| valid_reserve(*percent))
    }

    /// The watched window kinds; a hand-edited unknown kind falls back to
    /// the longest window rather than silently watching nothing.
    pub(crate) fn reserve_windows(&self) -> Vec<String> {
        reserve_windows(&self.reserve_windows).unwrap_or_default()
    }

    pub(crate) fn summary(&self) -> AccountSummary {
        let current = time::OffsetDateTime::now_utc();
        let active = |until: &Option<String>| {
            until
                .as_deref()
                .and_then(parse_timestamp)
                .is_some_and(|t| t > current)
        };
        let status = if !self.enabled {
            "disabled"
        } else if self.exhausted_without_reset || active(&self.exhausted_until) {
            "exhausted"
        } else if active(&self.cooldown_until) {
            "cooldown"
        } else if self
            .expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|t| t <= current)
            && self.refresh_token.is_none()
        {
            "expired"
        } else if self.last_error.is_some() {
            "error"
        } else {
            "ready"
        };
        AccountSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider,
            email: self.email.clone(),
            plan: self.plan.clone(),
            subscription: self.subscription.clone().or_else(|| {
                SubscriptionTier::detect(self.provider, self.plan.as_deref(), None, "credential")
            }),
            enabled: self.enabled,
            status: status.into(),
            last_error: self.last_error.clone(),
            reserve_percent: self.reserve(),
            reserve_windows: self.reserve_windows(),
            route: self.route.clone(),
            base_url: (self.provider == SubscriptionProvider::OpenaiCompatible)
                .then(|| self.base_url.clone())
                .flatten(),
            dialect: self.dialect.clone(),
            models: if self.provider == SubscriptionProvider::OpenaiCompatible {
                self.models.clone()
            } else {
                vec![]
            },
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }

    /// The pool this account serves: a custom endpoint's route, otherwise
    /// its provider. Scheduling, affinity and model prefixes use it.
    pub(crate) fn pool(&self) -> &str {
        self.route.as_deref().unwrap_or(self.provider.as_str())
    }

    pub(crate) fn needs_refresh(&self) -> bool {
        if self.provider == SubscriptionProvider::Meta {
            return self.access_token.starts_with("dca:") && self.dca_token.is_some();
        }
        self.expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|t| t <= time::OffsetDateTime::now_utc() + time::Duration::minutes(5))
    }
}

pub struct AccountStore {
    directory: PathBuf,
    accounts: RwLock<BTreeMap<String, AccountCredential>>,
    pub(crate) refresh_locks: Mutex<BTreeMap<String, std::sync::Arc<Mutex<()>>>>,
}

impl AccountStore {
    pub(crate) fn oauth_clients_path(&self) -> PathBuf {
        self.directory
            .parent()
            .unwrap_or(&self.directory)
            .join("oauth-clients.json")
    }
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SubscriptionError> {
        let directory = path.into();
        fs::create_dir_all(&directory).map_err(|_| SubscriptionError::Storage)?;
        private_directory(&directory)?;
        let mut accounts = BTreeMap::new();
        for entry in fs::read_dir(&directory).map_err(|_| SubscriptionError::Storage)? {
            let entry = entry.map_err(|_| SubscriptionError::Storage)?;
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            if entry
                .file_type()
                .map_err(|_| SubscriptionError::Storage)?
                .is_symlink()
            {
                return Err(SubscriptionError::Storage);
            }
            let bytes = fs::read(&path).map_err(|_| SubscriptionError::Storage)?;
            let account: AccountCredential =
                serde_json::from_slice(&bytes).map_err(|_| SubscriptionError::Storage)?;
            if !valid_id(&account.id)
                || path.file_stem().and_then(|v| v.to_str()) != Some(&account.id)
            {
                return Err(SubscriptionError::Storage);
            }
            private_file(&path)?;
            accounts.insert(account.id.clone(), account);
        }
        Ok(Self {
            directory,
            accounts: RwLock::new(accounts),
            refresh_locks: Mutex::new(BTreeMap::new()),
        })
    }

    pub async fn list(&self) -> Vec<AccountSummary> {
        self.accounts
            .read()
            .await
            .values()
            .map(AccountCredential::summary)
            .collect()
    }

    pub async fn get(&self, id: &str) -> Option<AccountCredential> {
        self.accounts.read().await.get(id).cloned()
    }

    pub(crate) fn reset_state_path(&self) -> PathBuf {
        self.directory.join(".quota-resets")
    }

    pub(crate) fn quota_snapshot_path(&self) -> PathBuf {
        self.directory.join(".quota-snapshots")
    }

    pub async fn import(
        &self,
        value: Value,
        name: Option<String>,
    ) -> Result<AccountSummary, SubscriptionError> {
        let explicit_name = name.as_deref().is_some_and(|name| !name.trim().is_empty())
            || text(&value, &["name", "label"]).is_some();
        let mut account = parse_import(value, name)?;
        let mut accounts = self.accounts.write().await;
        // Reimporting the same identity rotates its tokens without duplicating it
        // or silently re-enabling a credential the user disabled.
        if let Some(previous) = accounts
            .values()
            .find(|existing| same_identity(existing, &account))
        {
            account.id = previous.id.clone();
            account.created_at = previous.created_at.clone();
            account.enabled = previous.enabled;
            account.reserve_percent = previous.reserve_percent;
            account.reserve_windows = previous.reserve_windows.clone();
            if !explicit_name {
                account.name = previous.name.clone();
            }
            if account.subscription.is_none() && account.plan.is_none() {
                account.subscription = previous.subscription.clone();
                account.plan = previous.plan.clone();
            }
            account.organization_id = account
                .organization_id
                .or_else(|| previous.organization_id.clone());
        }
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(account.id.clone(), account);
        Ok(summary)
    }

    /// Every stored record, tokens included, for a configuration export.
    /// This is the one deliberate exit of credentials from the broker: the
    /// export route sends them to the operator who asked for a backup.
    pub async fn export(&self) -> Vec<AccountCredential> {
        self.accounts.read().await.values().cloned().collect()
    }

    /// Restores one record of an export. A record naming a stored account by
    /// id, or by identity as `import` matches it, replaces that account in
    /// place under its local id; any other record is added. Cooldowns and
    /// errors belong to the instance that recorded them and are cleared.
    pub async fn restore(&self, record: Value) -> Result<AccountSummary, SubscriptionError> {
        let mut account: AccountCredential = serde_json::from_value(record)
            .map_err(|_| SubscriptionError::InvalidImport("invalid account record"))?;
        check_record(&account)?;
        account.cooldown_until = None;
        account.last_error = None;
        let mut accounts = self.accounts.write().await;
        let previous = accounts
            .get(&account.id)
            .filter(|existing| existing.provider == account.provider)
            .or_else(|| {
                accounts
                    .values()
                    .find(|existing| same_identity(existing, &account))
            });
        if let Some(previous) = previous {
            account.id = previous.id.clone();
            account.created_at = previous.created_at.clone();
        } else if !valid_id(&account.id) || accounts.contains_key(&account.id) {
            account.id = random_id();
        }
        if parse_timestamp(&account.created_at).is_none() {
            account.created_at = now();
        }
        account.updated_at = now();
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(account.id.clone(), account);
        Ok(summary)
    }

    pub async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.modify(id, |account| account.enabled = enabled).await
    }

    /// `reserve` is `Some(None)` to remove the reserve, `None` to keep it;
    /// `windows` is `None` to keep the watched kinds.
    pub async fn update(
        &self,
        id: &str,
        name: Option<String>,
        enabled: Option<bool>,
        reserve: Option<Option<f64>>,
        windows: Option<Vec<String>>,
    ) -> Result<AccountSummary, SubscriptionError> {
        if reserve
            .flatten()
            .is_some_and(|percent| !valid_reserve(percent))
        {
            return Err(SubscriptionError::InvalidReserve);
        }
        let windows = match windows {
            Some(kinds) => {
                Some(reserve_windows(&kinds).ok_or(SubscriptionError::InvalidReserveWindows)?)
            }
            None => None,
        };
        self.modify(id, |account| {
            if let Some(name) = name {
                account.name = name;
            }
            if let Some(enabled) = enabled {
                account.enabled = enabled;
            }
            if let Some(reserve) = reserve {
                account.reserve_percent = reserve;
            }
            if let Some(windows) = windows {
                account.reserve_windows = windows;
            }
        })
        .await
    }

    pub async fn delete(&self, id: &str) -> Result<(), SubscriptionError> {
        let mut accounts = self.accounts.write().await;
        if !accounts.contains_key(id) {
            return Err(SubscriptionError::NotFound);
        }
        fs::remove_file(self.directory.join(format!("{id}.json")))
            .map_err(|_| SubscriptionError::Storage)?;
        accounts.remove(id);
        Ok(())
    }

    /// Routes of the custom endpoints currently configured.
    pub async fn routes(&self) -> std::collections::BTreeSet<String> {
        self.accounts
            .read()
            .await
            .values()
            .filter_map(|account| account.route.clone())
            .collect()
    }

    /// Changes a custom endpoint in place. Its key stays in the account file
    /// and never comes back in the summary.
    pub async fn update_endpoint(
        &self,
        id: &str,
        patch: EndpointPatch,
    ) -> Result<AccountSummary, SubscriptionError> {
        let route = patch.route.map(|r| r.trim().to_ascii_lowercase());
        if route.as_deref().is_some_and(|r| !valid_route(r)) {
            return Err(SubscriptionError::InvalidEndpoint(ROUTE_RULE));
        }
        let base_url = patch.base_url.map(|url| url.trim().to_owned());
        if base_url.as_deref().is_some_and(|url| !valid_base_url(url)) {
            return Err(SubscriptionError::InvalidEndpoint(BASE_URL_RULE));
        }
        let dialect = patch
            .dialect
            .map(|d| d.map(|d| d.trim().to_ascii_lowercase()));
        if dialect
            .as_ref()
            .and_then(Option::as_deref)
            .is_some_and(|d| !valid_dialect(d))
        {
            return Err(SubscriptionError::InvalidEndpoint(DIALECT_RULE));
        }
        let api_key = patch.api_key.map(|key| key.trim().to_owned());
        if api_key.as_deref().is_some_and(|key| {
            key.is_empty() || key.len() > 8192 || key.chars().any(char::is_control)
        }) {
            return Err(SubscriptionError::InvalidEndpoint(
                "api_key must be 1 to 8192 characters without control characters",
            ));
        }
        let models = patch.models.map(|models| {
            models
                .into_iter()
                .map(|m| m.trim().to_owned())
                .filter(|m| !m.is_empty())
                .collect::<Vec<_>>()
        });
        if models
            .as_ref()
            .is_some_and(|models| models.len() > 500 || models.iter().any(|m| m.len() > 200))
        {
            return Err(SubscriptionError::InvalidEndpoint(
                "models takes at most 500 ids of 200 characters",
            ));
        }
        let mut accounts = self.accounts.write().await;
        let current = accounts.get(id).ok_or(SubscriptionError::NotFound)?;
        if current.provider != SubscriptionProvider::OpenaiCompatible {
            return Err(SubscriptionError::InvalidEndpoint(
                "only openai_compatible endpoints take a route, base_url, dialect, models or api_key",
            ));
        }
        let mut account = current.clone();
        if let Some(route) = route {
            account.route = Some(route);
        }
        if let Some(url) = base_url {
            account.base_url = Some(url);
        }
        if let Some(dialect) = dialect {
            account.dialect = dialect;
        }
        if let Some(models) = models {
            account.models = models;
        }
        if let Some(key) = api_key {
            account.access_token = key;
            account.last_error = None;
            account.cooldown_until = None;
        }
        if account.route.is_some() && account.base_url.is_none() {
            return Err(SubscriptionError::InvalidEndpoint(
                "an endpoint with a route needs its base_url",
            ));
        }
        account.updated_at = now();
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(account.id.clone(), account);
        Ok(summary)
    }

    pub async fn eligible(
        &self,
        provider: SubscriptionProvider,
        model: Option<&str>,
    ) -> Vec<AccountCredential> {
        self.eligible_in(provider, None, model).await
    }

    /// Accounts of one pool: a provider's own accounts, or the keys of one
    /// custom endpoint route. Keys on different routes never serve each other.
    pub async fn eligible_in(
        &self,
        provider: SubscriptionProvider,
        route: Option<&str>,
        model: Option<&str>,
    ) -> Vec<AccountCredential> {
        self.accounts
            .read()
            .await
            .values()
            .filter(|account| {
                account.provider == provider
                    && account.route.as_deref() == route
                    && matches!(account.summary().status.as_str(), "ready" | "error")
                    && model.is_none_or(|model| {
                        (account.models.is_empty() || account.models.iter().any(|m| m == model))
                            && !account.model_exhaustion.iter().any(|(family, reset)| {
                                model.contains(family)
                                    && reset
                                        .as_deref()
                                        .and_then(parse_timestamp)
                                        .is_none_or(|t| t > time::OffsetDateTime::now_utc())
                            })
                    })
            })
            .cloned()
            .collect()
    }

    pub(crate) async fn update_credential(
        &self,
        mut account: AccountCredential,
    ) -> Result<AccountSummary, SubscriptionError> {
        let mut accounts = self.accounts.write().await;
        let previous = accounts
            .get(&account.id)
            .ok_or(SubscriptionError::NotFound)?;
        account.enabled = previous.enabled;
        account.name = previous.name.clone();
        account.reserve_percent = previous.reserve_percent;
        account.reserve_windows = previous.reserve_windows.clone();
        if account.provider != SubscriptionProvider::Meta {
            account.plan = previous.plan.clone().or(account.plan);
            account.subscription = previous.subscription.clone().or(account.subscription);
        }
        account.organization_id = previous.organization_id.clone().or(account.organization_id);
        account.updated_at = now();
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(account.id.clone(), account);
        Ok(summary)
    }

    pub async fn mark_cooldown(
        &self,
        id: &str,
        seconds: u64,
        error: &'static str,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.modify(id, |account| {
            account.cooldown_until = Some(timestamp(
                time::OffsetDateTime::now_utc()
                    + time::Duration::seconds(seconds.min(86_400) as i64),
            ));
            account.last_error = Some(error.into());
        })
        .await
    }

    pub async fn cooldown(
        &self,
        id: &str,
        seconds: u64,
        reason: &'static str,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.mark_cooldown(id, seconds, reason).await
    }

    /// A successful upstream response proves the account serves again: the
    /// reason an expired cooldown or an earlier refusal left behind goes,
    /// while an active cooldown keeps its own. Writes only when one clears.
    pub async fn clear_stale_error(&self, id: &str) {
        let mut accounts = self.accounts.write().await;
        let Some(current) = accounts.get(id) else {
            return;
        };
        let cooling = current
            .cooldown_until
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|until| until > time::OffsetDateTime::now_utc());
        if current.last_error.is_none() || cooling {
            return;
        }
        let mut account = current.clone();
        account.last_error = None;
        account.cooldown_until = None;
        account.updated_at = now();
        if self.persist(&account).is_ok() {
            accounts.insert(id.into(), account);
        }
    }

    pub async fn mark_error(
        &self,
        id: &str,
        error: Option<String>,
    ) -> Result<AccountSummary, SubscriptionError> {
        // Only the caller's sanitized category belongs in the public summary.
        self.modify(id, |account| {
            account.last_error = error.map(|_| "provider request failed".into())
        })
        .await
    }

    pub(crate) async fn modify(
        &self,
        id: &str,
        edit: impl FnOnce(&mut AccountCredential),
    ) -> Result<AccountSummary, SubscriptionError> {
        let mut accounts = self.accounts.write().await;
        let mut account = accounts
            .get(id)
            .cloned()
            .ok_or(SubscriptionError::NotFound)?;
        edit(&mut account);
        account.updated_at = now();
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(id.into(), account);
        Ok(summary)
    }

    fn persist(&self, account: &AccountCredential) -> Result<(), SubscriptionError> {
        if !valid_id(&account.id) {
            return Err(SubscriptionError::Storage);
        }
        let target = self.directory.join(format!("{}.json", account.id));
        let temporary = self.directory.join(format!(".{}.tmp", random_id()));
        let result = (|| {
            let bytes =
                serde_json::to_vec_pretty(account).map_err(|_| SubscriptionError::Storage)?;
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
            private_file(&temporary)?;
            file.write_all(&bytes)
                .map_err(|_| SubscriptionError::Storage)?;
            file.sync_all().map_err(|_| SubscriptionError::Storage)?;
            drop(file);
            fs::rename(&temporary, &target).map_err(|_| SubscriptionError::Storage)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

/// Two records name the same sign-in: same provider and same account id or
/// email. Custom endpoints carry neither and never match this way.
fn same_identity(existing: &AccountCredential, account: &AccountCredential) -> bool {
    existing.provider == account.provider
        && ((account.account_id.is_some() && existing.account_id == account.account_id)
            || (account.email.is_some() && existing.email == account.email))
}

/// The checks `parse_import` applies, for a stored record coming back from
/// an export.
fn check_record(account: &AccountCredential) -> Result<(), SubscriptionError> {
    if account.provider == SubscriptionProvider::Meta
        && account
            .base_url
            .as_deref()
            .is_some_and(|url| !super::meta::valid_api_base(url))
    {
        return Err(SubscriptionError::InvalidImport("untrusted Meta base URL"));
    }
    let token = account.access_token.trim();
    if token.is_empty() || token.len() > 65_536 || token.chars().any(char::is_control) {
        return Err(SubscriptionError::InvalidImport("missing access token"));
    }
    if account.name.trim().is_empty()
        || account.name.len() > 200
        || account.email.as_ref().is_some_and(|v| v.len() > 320)
    {
        return Err(SubscriptionError::InvalidImport(
            "account label is missing or too long",
        ));
    }
    if account
        .expires_at
        .as_deref()
        .is_some_and(|t| parse_timestamp(t).is_none())
    {
        return Err(SubscriptionError::InvalidImport("invalid expiry timestamp"));
    }
    if account
        .base_url
        .as_deref()
        .is_some_and(|url| !valid_base_url(url))
    {
        return Err(SubscriptionError::InvalidImport("invalid base URL"));
    }
    if account.route.is_some() || account.dialect.is_some() {
        if account.provider != SubscriptionProvider::OpenaiCompatible {
            return Err(SubscriptionError::InvalidEndpoint(
                "route and dialect apply only to openai_compatible endpoints",
            ));
        }
        if account.route.as_deref().is_some_and(|r| !valid_route(r)) {
            return Err(SubscriptionError::InvalidEndpoint(ROUTE_RULE));
        }
        if account
            .dialect
            .as_deref()
            .is_some_and(|d| !valid_dialect(d))
        {
            return Err(SubscriptionError::InvalidEndpoint(DIALECT_RULE));
        }
        if account.route.is_some() && account.base_url.is_none() {
            return Err(SubscriptionError::InvalidEndpoint(
                "an endpoint with a route needs its base_url",
            ));
        }
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id.len() == 48 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn private_directory(path: &Path) -> Result<(), SubscriptionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| SubscriptionError::Storage)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let user = std::env::var("USERNAME").map_err(|_| SubscriptionError::Storage)?;
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        let principal = if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        };
        let output = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{principal}:(OI)(CI)F"))
            .creation_flags(0x08000000)
            .output()
            .map_err(|_| SubscriptionError::Storage)?;
        if !output.status.success() {
            return Err(SubscriptionError::Storage);
        }
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<(), SubscriptionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| SubscriptionError::Storage)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub(crate) fn text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned)
    })
}

pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn apply_identity(account: &mut AccountCredential) {
    let claims = account
        .id_token
        .as_deref()
        .and_then(jwt_claims)
        .or_else(|| jwt_claims(&account.access_token));
    if let Some(claims) = claims {
        // JWT claims are identity hints only. They do not authorize a request.
        account.email = account.email.take().or_else(|| text(&claims, &["email"]));
        let auth = claims.get("https://api.openai.com/auth").unwrap_or(&claims);
        account.account_id = account
            .account_id
            .take()
            .or_else(|| text(auth, &["chatgpt_account_id", "account_id"]));
        if account.provider == SubscriptionProvider::Xai {
            account.account_id = account
                .account_id
                .take()
                .or_else(|| text(&claims, &["sub"]));
        }
        account.plan = account
            .plan
            .take()
            .or_else(|| text(auth, &["chatgpt_plan_type", "plan_type"]));
        if account.subscription.is_none() {
            account.subscription =
                subscription_from_value(account.provider, auth, "identity_claim");
        }
    }
    // Identity tokens and bearer tokens can have different lifetimes. Only
    // the bearer token's expiration governs when an inference call refreshes.
    if account.expires_at.is_none() {
        account.expires_at = jwt_claims(&account.access_token)
            .and_then(|claims| claims.get("exp").and_then(Value::as_i64))
            .and_then(|exp| time::OffsetDateTime::from_unix_timestamp(exp).ok())
            .map(timestamp);
    }
}

fn parse_import(
    mut value: Value,
    name: Option<String>,
) -> Result<AccountCredential, SubscriptionError> {
    if !value.is_object() {
        return Err(SubscriptionError::InvalidImport("expected a JSON object"));
    }
    // Grok CLI stores OAuth under an issuer/client compound key. Import the
    // selected record rather than treating arbitrary siblings as credentials.
    if let Some(native) = value
        .get("https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828")
        .cloned()
    {
        value = native;
        let object = value
            .as_object_mut()
            .ok_or(SubscriptionError::InvalidImport(
                "invalid Grok credential record",
            ))?;
        object.insert("type".into(), serde_json::json!("xai"));
    }
    // OpenCode keeps its GitHub Copilot login under the provider's id; the
    // GitHub OAuth token serves the Copilot API directly and does not expire.
    if let Some(native) = value
        .get("github-copilot")
        .filter(|record| record.is_object())
        .cloned()
    {
        let token = text(&native, &["refresh", "access", "access_token"]).ok_or(
            SubscriptionError::InvalidImport("missing GitHub Copilot token"),
        )?;
        let mut record = serde_json::json!({"type":"copilot","access_token":token});
        if let Some(domain) = text(&native, &["enterpriseUrl"]) {
            record["base_url"] = serde_json::json!(format!(
                "https://copilot-api.{}",
                domain
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
            ));
        }
        value = record;
    }
    let (provider, token) = if let Some(oauth) = value.get("claudeAiOauth") {
        (SubscriptionProvider::Claude, oauth)
    } else if let Some(tokens) = value.get("tokens") {
        (SubscriptionProvider::Codex, tokens)
    } else {
        let kind = text(&value, &["type", "provider"])
            .ok_or(SubscriptionError::InvalidImport("missing provider type"))?;
        let provider = SubscriptionProvider::parse(&kind).ok_or(
            SubscriptionError::InvalidImport("unsupported provider type"),
        )?;
        (
            provider,
            value
                .get("token")
                .filter(|t| t.is_object())
                .unwrap_or(&value),
        )
    };
    if provider == SubscriptionProvider::Meta
        && [&value, token].iter().any(|record| {
            record.get("is_subs_active").is_some_and(|v| v == false)
                || record["require_payment"] == true
        })
    {
        return Err(SubscriptionError::InvalidImport(
            "Muse Code requires an active subscription",
        ));
    }
    let access_token = text(
        token,
        if provider == SubscriptionProvider::Meta {
            &["api_key", "access_token", "dca_token"]
        } else {
            &["access_token", "accessToken", "api_key", "key"]
        },
    )
    .ok_or(SubscriptionError::InvalidImport("missing access token"))?;
    let expires_at = text(token, &["expired", "expiry", "expires_at", "expiration"])
        .or_else(|| {
            token
                .get("expiresAt")
                .or_else(|| token.get("expiry_date"))
                .and_then(Value::as_i64)
                .and_then(|milliseconds| {
                    time::OffsetDateTime::from_unix_timestamp(milliseconds / 1000)
                        .ok()
                        .map(timestamp)
                })
        })
        .or_else(|| {
            token
                .get("expires_at")
                .and_then(Value::as_i64)
                .and_then(|epoch| {
                    time::OffsetDateTime::from_unix_timestamp(if epoch > 100_000_000_000 {
                        epoch / 1000
                    } else {
                        epoch
                    })
                    .ok()
                    .map(timestamp)
                })
        });
    if expires_at
        .as_deref()
        .is_some_and(|t| parse_timestamp(t).is_none())
    {
        return Err(SubscriptionError::InvalidImport("invalid expiry timestamp"));
    }
    let base_url = text(&value, &["base_url", "resource_url"]).or_else(|| {
        (provider == SubscriptionProvider::Xai).then(|| "https://cli-chat-proxy.grok.com/v1".into())
    });
    if base_url.as_deref().is_some_and(|url| !valid_base_url(url)) {
        return Err(SubscriptionError::InvalidImport("invalid base URL"));
    }
    if provider == SubscriptionProvider::Meta
        && base_url
            .as_deref()
            .is_some_and(|url| !super::meta::valid_api_base(url))
    {
        return Err(SubscriptionError::InvalidImport("untrusted Meta base URL"));
    }
    let route = text(&value, &["route", "prefix"]).map(|r| r.trim().to_ascii_lowercase());
    let dialect = text(&value, &["dialect", "format"]).map(|d| d.trim().to_ascii_lowercase());
    if route.is_some() || dialect.is_some() {
        if provider != SubscriptionProvider::OpenaiCompatible {
            return Err(SubscriptionError::InvalidEndpoint(
                "route and dialect apply only to openai_compatible endpoints",
            ));
        }
        if route.as_deref().is_some_and(|r| !valid_route(r)) {
            return Err(SubscriptionError::InvalidEndpoint(ROUTE_RULE));
        }
        if dialect.as_deref().is_some_and(|d| !valid_dialect(d)) {
            return Err(SubscriptionError::InvalidEndpoint(DIALECT_RULE));
        }
        if route.is_some() && base_url.is_none() {
            return Err(SubscriptionError::InvalidEndpoint(
                "an endpoint with a route needs its base_url",
            ));
        }
    }
    let created = now();
    let subscription = subscription_from_value(provider, token, "credential")
        .or_else(|| subscription_from_value(provider, &value, "credential"));
    let mut account = AccountCredential {
        id: random_id(),
        name: String::new(),
        provider,
        access_token,
        refresh_token: text(token, &["refresh_token", "refreshToken"]),
        dca_token: if provider == SubscriptionProvider::Meta {
            text(token, &["dca_token"])
                .or_else(|| text(token, &["access_token"]).filter(|t| t.starts_with("dca:")))
        } else {
            None
        },
        dca_expires_at: text(token, &["dca_expired", "dca_expires_at"]).or_else(|| {
            token["dca_expires_at"]
                .as_i64()
                .and_then(|n| time::OffsetDateTime::from_unix_timestamp(n).ok())
                .map(timestamp)
        }),
        id_token: text(token, &["id_token", "idToken"]),
        account_id: text(token, &["account_id", "account_uuid", "sub", "user_id"])
            .or_else(|| text(&value, &["account_id", "account_uuid", "sub", "user_id"])),
        organization_id: text(&value, &["organization_id", "organizationUuid"])
            .or_else(|| {
                value
                    .get("oauthAccount")
                    .and_then(|account| text(account, &["organizationUuid"]))
            })
            .or_else(|| {
                value
                    .get("organization")
                    .and_then(|organization| text(organization, &["uuid", "id"]))
            }),
        base_url,
        models: value
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        project_id: text(&value, &["project_id", "projectId"]),
        email: text(&value, &["email", "user_email"]).or_else(|| text(token, &["email"])),
        plan: text(
            &value,
            &["plan_type", "subscription_type", "plan", "subs_tier_name"],
        )
        .or_else(|| text(token, &["subscriptionType", "plan_type"])),
        subscription,
        expires_at: if provider == SubscriptionProvider::Meta {
            None
        } else {
            expires_at
        },
        enabled: !value
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        created_at: created.clone(),
        updated_at: created,
        last_error: None,
        cooldown_until: None,
        exhausted_until: None,
        exhausted_without_reset: false,
        client_id: text(&value, &["client_id"]).or_else(|| text(token, &["client_id"])),
        client_secret: text(&value, &["client_secret"]).or_else(|| text(token, &["client_secret"])),
        token_endpoint: text(&value, &["token_endpoint"]),
        model_exhaustion: BTreeMap::new(),
        reserve_percent: None,
        reserve_windows: vec![],
        route,
        dialect,
    };
    apply_identity(&mut account);
    account.name = name
        .filter(|v| !v.trim().is_empty())
        .or_else(|| text(&value, &["name", "label"]))
        .or_else(|| account.email.clone())
        .or_else(|| account.route.clone())
        .unwrap_or_else(|| format!("{} account", provider.as_str()));
    if account.name.len() > 200 || account.email.as_ref().is_some_and(|v| v.len() > 320) {
        return Err(SubscriptionError::InvalidImport(
            "account label is too long",
        ));
    }
    Ok(account)
}

pub(crate) fn subscription_from_value(
    provider: SubscriptionProvider,
    value: &Value,
    source: &'static str,
) -> Option<SubscriptionTier> {
    if provider == SubscriptionProvider::Meta {
        return Some(SubscriptionTier {
            id: text(value, &["subs_tier_id"])?,
            label: text(value, &["subs_tier_name"])?,
            multiplier: None,
            source: source.into(),
        });
    }
    let organization = value.get("organization").unwrap_or(value);
    let plan = text(
        value,
        &[
            "plan_type",
            "planType",
            "chatgpt_plan_type",
            "subscriptionType",
            "subscription_type",
            "subscription_tier_display",
            "subscriptionTierDisplay",
            "subscription_tier",
            "subscriptionTier",
            "plan",
        ],
    )
    .or_else(|| text(organization, &["organization_type"]));
    let rate_tier = text(organization, &["rate_limit_tier", "rateLimitTier"]);
    let mut tier =
        SubscriptionTier::detect(provider, plan.as_deref(), rate_tier.as_deref(), source)?;
    // The generic Pro identifier does not prove an allowance multiplier.
    // Only an explicit provider field can add it.
    if provider == SubscriptionProvider::Codex {
        let multiple = value
            .get("usage_multiplier")
            .or_else(|| value.get("rate_limit_multiplier"))
            .and_then(Value::as_u64)
            .filter(|multiple| (1..=100).contains(multiple));
        if let Some(multiple) = multiple {
            tier.multiplier = Some(multiple as u32);
            tier.label = format!("{} x{multiple}", tier.label);
        }
    }
    Some(tier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_claude_tier_preserves_the_explicit_allowance_multiplier() {
        let account = parse_import(json!({"claudeAiOauth":{"accessToken":"fixture-private","subscriptionType":"max","rateLimitTier":"default_claude_max_20x"},"oauthAccount":{"organizationUuid":"fixture-organization"}}), None).unwrap();
        let tier = account.summary().subscription.unwrap();
        assert_eq!(tier.label, "Claude Max x20");
        assert_eq!(tier.multiplier, Some(20));
        assert_eq!(
            account.organization_id.as_deref(),
            Some("fixture-organization")
        );
        let public = serde_json::to_string(&account.summary()).unwrap();
        assert!(!public.contains("fixture-private"));
        assert!(!public.contains("fixture-organization"));
    }

    #[test]
    fn tier_detection_uses_provider_metadata_and_keeps_unknowns_unknown() {
        let claude = subscription_from_value(SubscriptionProvider::Claude, &json!({"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_5x"}}), "provider_profile").unwrap();
        assert_eq!(claude.label, "Claude Max x5");
        let codex = subscription_from_value(
            SubscriptionProvider::Codex,
            &json!({"plan_type":"pro"}),
            "provider_usage",
        )
        .unwrap();
        assert_eq!(codex.label, "Codex Pro");
        assert_eq!(codex.multiplier, None);
        assert_eq!(
            subscription_from_value(
                SubscriptionProvider::Codex,
                &json!({"plan_type":"pro","usage_multiplier":20}),
                "provider_usage"
            )
            .unwrap()
            .multiplier,
            Some(20)
        );
        assert_eq!(
            subscription_from_value(
                SubscriptionProvider::Xai,
                &json!({"subscription_tier_display":"SuperGrok Heavy"}),
                "provider_settings"
            )
            .unwrap()
            .label,
            "SuperGrok Heavy"
        );
        assert_eq!(
            subscription_from_value(
                SubscriptionProvider::Xai,
                &json!({"subscription_tier":"premium"}),
                "provider_settings"
            )
            .unwrap()
            .label,
            "X Premium"
        );
        assert_eq!(
            subscription_from_value(
                SubscriptionProvider::Xai,
                &json!({"subscription_tier_display":"X Premium+"}),
                "provider_settings"
            )
            .unwrap()
            .label,
            "X Premium+"
        );
        assert!(subscription_from_value(SubscriptionProvider::Claude, &json!({"organization":{"organization_type":"unknown-fixture","rate_limit_tier":"unknown-private-value"}}), "provider_profile").is_none());
    }

    #[tokio::test]
    async fn account_update_is_atomic_persisted_and_survives_credential_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let store = AccountStore::open(directory.path()).unwrap();
        let imported = store.import(json!({"type":"claude","access_token":"fixture-private","account_id":"fixture-account"}), None).await.unwrap();
        let stale = store.get(&imported.id).await.unwrap();
        let updated = store
            .update(
                &imported.id,
                Some("New label".into()),
                Some(false),
                Some(Some(5.0)),
                Some(vec!["weekly".into(), "five-hour".into(), "weekly".into()]),
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "New label");
        // Watched kinds are stored once each, shortest first.
        assert_eq!(updated.reserve_windows, ["five-hour", "weekly"]);
        assert!(!updated.enabled);
        assert_eq!(updated.reserve_percent, Some(5.0));
        store.update_credential(stale).await.unwrap();
        let reopened = AccountStore::open(directory.path()).unwrap();
        assert_eq!(reopened.list().await[0].name, "New label");
        assert!(!reopened.list().await[0].enabled);
        assert_eq!(reopened.list().await[0].reserve_percent, Some(5.0));
        assert_eq!(
            reopened.list().await[0].reserve_windows,
            ["five-hour", "weekly"]
        );
        // A reimport keeps the reserve; an explicit removal clears it.
        reopened.import(json!({"type":"claude","access_token":"rotated-fixture","account_id":"fixture-account"}), None).await.unwrap();
        assert_eq!(reopened.list().await[0].reserve_percent, Some(5.0));
        assert_eq!(
            reopened.list().await[0].reserve_windows,
            ["five-hour", "weekly"]
        );
        assert!(matches!(
            reopened
                .update(&imported.id, None, None, None, Some(vec!["hourly".into()]))
                .await,
            Err(SubscriptionError::InvalidReserveWindows)
        ));
        let longest = reopened
            .update(&imported.id, None, None, None, Some(vec![]))
            .await
            .unwrap();
        assert!(longest.reserve_windows.is_empty());
        let cleared = reopened
            .update(&imported.id, None, None, Some(None), None)
            .await
            .unwrap();
        assert_eq!(cleared.reserve_percent, None);
        // The store refuses a reserve outside 0 < percent < 100.
        for invalid in [-1.0, 0.0, 100.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                reopened
                    .update(&imported.id, None, None, Some(Some(invalid)), None)
                    .await,
                Err(SubscriptionError::InvalidReserve)
            ));
        }
        assert_eq!(reopened.list().await[0].reserve_percent, None);
    }

    #[test]
    fn account_storage_without_metadata_fields_remains_compatible() {
        let old: AccountCredential = serde_json::from_value(json!({"id":"fixture","name":"Old account","provider":"codex","access_token":"private-fixture","plan":"pro","enabled":true,"created_at":"2026-10-03T12:00:00Z","updated_at":"2026-10-03T12:00:00Z"})).unwrap();
        assert!(old.subscription.is_none());
        assert!(old.organization_id.is_none());
        assert_eq!(old.summary().subscription.unwrap().label, "Codex Pro");
    }

    #[tokio::test]
    async fn reimport_without_a_label_preserves_the_renamed_account() {
        let directory = tempfile::tempdir().unwrap();
        let store = AccountStore::open(directory.path()).unwrap();
        let account = store
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"fixture-account"}),
                None,
            )
            .await
            .unwrap();
        store
            .modify(&account.id, |account| account.name = "Renamed".into())
            .await
            .unwrap();
        let reimported = store.import(json!({"type":"codex","access_token":"rotated-fixture","account_id":"fixture-account"}), None).await.unwrap();
        assert_eq!(reimported.name, "Renamed");
    }

    #[tokio::test]
    async fn imports_native_credentials_without_exposing_tokens_and_persists_disabled_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = AccountStore::open(directory.path()).unwrap();
        let summary = store.import(json!({"tokens":{"access_token":"fixture-access-private","refresh_token":"fixture-refresh-private","account_id":"account-1"}}), Some("Personal".into())).await.unwrap();
        let public = serde_json::to_string(&summary).unwrap();
        assert!(!public.contains("fixture-access-private"));
        assert!(!public.contains("fixture-refresh-private"));
        store.set_enabled(&summary.id, false).await.unwrap();
        assert!(
            store
                .eligible(SubscriptionProvider::Codex, None)
                .await
                .is_empty()
        );
        let reopened = AccountStore::open(directory.path()).unwrap();
        assert_eq!(reopened.list().await[0].status, "disabled");
        let imported = reopened
            .import(
                json!({"type":"codex","access_token":"rotation","account_id":"account-1"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(imported.id, summary.id);
        assert!(!imported.enabled);
        assert_eq!(reopened.list().await.len(), 1);
    }

    #[test]
    fn imports_claude_native_and_cli_proxy_gemini_shapes() {
        let claude = parse_import(json!({"claudeAiOauth":{"accessToken":"fixture","refreshToken":"refresh","expiresAt":2_000_000_000_000i64,"subscriptionType":"max"}}), None).unwrap();
        assert_eq!(claude.provider, SubscriptionProvider::Claude);
        assert_eq!(claude.plan.as_deref(), Some("max"));
        assert!(
            claude
                .expires_at
                .as_deref()
                .and_then(parse_timestamp)
                .is_some()
        );
        let gemini = parse_import(json!({"type":"gemini","token":{"access_token":"fixture","refresh_token":"refresh","expiry":"2030-01-01T00:00:00Z"},"project_id":"project"}), None).unwrap();
        assert_eq!(gemini.project_id.as_deref(), Some("project"));
        let grok = parse_import(json!({"https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828":{
            "key":"native-fixture", "refresh_token":"refresh", "user_id":"native-subject", "email":"fixture@example.test", "expires_at":2_000_000_000i64
        }}), None).unwrap();
        assert_eq!(grok.provider, SubscriptionProvider::Xai);
        assert_eq!(grok.account_id.as_deref(), Some("native-subject"));
        assert_eq!(
            grok.base_url.as_deref(),
            Some("https://cli-chat-proxy.grok.com/v1")
        );
    }

    #[test]
    fn rejects_unrecognized_provider_without_echoing_untrusted_values() {
        let error = parse_import(
            json!({"type":"secret-in-type","access_token":"secret-in-access"}),
            None,
        )
        .err()
        .unwrap();
        assert!(!error.to_string().contains("secret-in"));
    }

    #[test]
    fn native_codex_expiration_uses_access_token_while_identity_uses_id_token() {
        let jwt = |claims: Value| {
            format!(
                "e30.{}.fixture",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            )
        };
        let expired_identity = jwt(json!({"email":"fixture@example.test","exp":1_600_000_000}));
        let valid_access = jwt(json!({"exp":2_000_000_000}));
        let account = parse_import(json!({"tokens":{
            "id_token":expired_identity,"access_token":valid_access,"refresh_token":"fixture-refresh"
        }}),None).unwrap();
        assert_eq!(account.email.as_deref(), Some("fixture@example.test"));
        assert_eq!(
            account.expires_at,
            time::OffsetDateTime::from_unix_timestamp(2_000_000_000)
                .ok()
                .map(timestamp)
        );
        assert!(!account.needs_refresh());

        let opaque_access = parse_import(json!({"type":"codex","id_token":jwt(json!({"exp":1_600_000_000})),"access_token":"opaque-fixture"}),None).unwrap();
        assert_eq!(opaque_access.expires_at, None);
        assert_eq!(opaque_access.summary().status, "ready");
    }

    #[tokio::test]
    async fn cooldown_excludes_an_account_until_it_expires() {
        let directory = tempfile::tempdir().unwrap();
        let store = AccountStore::open(directory.path()).unwrap();
        let account = store
            .import(json!({"type":"codex","access_token":"fixture"}), None)
            .await
            .unwrap();
        store
            .mark_cooldown(&account.id, 30, "rate limited")
            .await
            .unwrap();
        assert!(
            store
                .eligible(SubscriptionProvider::Codex, None)
                .await
                .is_empty()
        );
        store
            .modify(&account.id, |a| {
                a.cooldown_until = Some("2020-01-01T00:00:00Z".into())
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .eligible(SubscriptionProvider::Codex, None)
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_success_clears_the_reason_an_expired_cooldown_left() {
        let directory = tempfile::tempdir().unwrap();
        let store = AccountStore::open(directory.path()).unwrap();
        let account = store
            .import(json!({"type":"codex","access_token":"fixture"}), None)
            .await
            .unwrap();
        store
            .cooldown(&account.id, 30, "upstream temporarily unavailable")
            .await
            .unwrap();
        store.clear_stale_error(&account.id).await;
        let cooling = store.get(&account.id).await.unwrap();
        assert_eq!(cooling.summary().status, "cooldown");
        assert!(cooling.last_error.is_some());

        store
            .modify(&account.id, |a| {
                a.cooldown_until = Some("2020-01-01T00:00:00Z".into())
            })
            .await
            .unwrap();
        assert_eq!(
            store.get(&account.id).await.unwrap().summary().status,
            "error"
        );
        store.clear_stale_error(&account.id).await;
        let cleared = store.get(&account.id).await.unwrap();
        assert_eq!(cleared.summary().status, "ready");
        assert!(cleared.last_error.is_none() && cleared.cooldown_until.is_none());
        let reopened = AccountStore::open(directory.path()).unwrap();
        assert!(
            reopened
                .get(&account.id)
                .await
                .unwrap()
                .last_error
                .is_none()
        );
    }
}
