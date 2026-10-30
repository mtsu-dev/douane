//! Runtime configuration. Subscription credentials live in the auth directory.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub settings: Settings,
    pub disabled_models: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub listen: String,
    /// Public client API address when the dashboard uses a different origin.
    pub client_api_base_url: Option<String>,
    pub auth_dir: PathBuf,
    pub database: PathBuf,
    /// Optional management token, loaded from this environment variable.
    pub management_token_env: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub strategy: crate::scheduler::Strategy,
    pub provider_strategies: BTreeMap<String, crate::scheduler::Strategy>,
    pub quota_refresh_seconds: u64,
    pub request_timeout_seconds: u64,
    pub default_provider: Option<String>,
    /// How `/v1/quotas` presents each provider's accounts. Absent means per account.
    pub quota_display: BTreeMap<String, QuotaDisplay>,
    /// Which windows `/v1/quotas` shows for each provider, and the one shown first.
    pub quota_windows: BTreeMap<String, QuotaWindows>,
    /// Show the optional provider consumption pace on the dashboard.
    pub show_provider_pace: bool,
    /// Hours before an account's weekly reset (monthly when its longest
    /// window is a month) when its reserve and client shares stop applying,
    /// so what is left gets used. 0 keeps them to the end.
    pub free_before_reset_hours: u32,
    /// Memory for Antigravity's signed Gemini history, every conversation
    /// together. A conversation squeezed out replays without its native parts.
    pub antigravity_replay_mib: u32,
    /// Minutes an idle conversation keeps its signed Gemini history.
    pub antigravity_replay_idle_minutes: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaDisplay {
    #[default]
    Accounts,
    Average,
}

/// A provider's window choices for `/v1/quotas`, by window id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaWindows {
    /// Window ids left out of `/v1/quotas`.
    pub hidden: BTreeSet<String>,
    /// The window listed first and flagged `primary`, shown even when hidden.
    pub primary: Option<String>,
}

const MAX_WINDOW_ID: usize = 80;
const MAX_HIDDEN_WINDOWS: usize = 64;

fn window_id(id: &str) -> bool {
    !id.trim().is_empty()
        && id.len() <= MAX_WINDOW_ID
        && !id.chars().any(|character| character.is_control())
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".into(),
            client_api_base_url: None,
            auth_dir: "auth".into(),
            database: "douane.sqlite".into(),
            management_token_env: None,
        }
    }
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            strategy: crate::scheduler::Strategy::Smart,
            provider_strategies: BTreeMap::new(),
            quota_refresh_seconds: 300,
            request_timeout_seconds: 180,
            default_provider: None,
            quota_display: BTreeMap::new(),
            quota_windows: BTreeMap::new(),
            show_provider_pace: false,
            free_before_reset_hours: 6,
            antigravity_replay_mib: 64,
            antigravity_replay_idle_minutes: 60,
        }
    }
}
impl Server {
    pub fn normalize(&mut self) -> Result<()> {
        if let Some(base) = &self.client_api_base_url {
            self.client_api_base_url = Some(normalize_client_api_base_url(base)?);
        }
        Ok(())
    }
}

fn normalize_client_api_base_url(value: &str) -> Result<String> {
    let invalid = || {
        anyhow::anyhow!(
            "client_api_base_url must be an absolute HTTP(S) URL without credentials, query, fragment, whitespace or control characters"
        )
    };
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid());
    }
    let (scheme, authority) = value.split_once("://").ok_or_else(invalid)?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(invalid());
    }
    if authority
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|authority| authority.contains('@'))
    {
        return Err(invalid());
    }
    let mut url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/v1") {
        path.to_owned()
    } else {
        format!("{path}/v1")
    };
    url.set_path(&path);
    Ok(url.into())
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !(30..=86400).contains(&self.quota_refresh_seconds) {
            bail!("quota_refresh_seconds must be between 30 and 86400");
        }
        if !(10..=1800).contains(&self.request_timeout_seconds) {
            bail!("request_timeout_seconds must be between 10 and 1800");
        }
        if self.free_before_reset_hours > 168 {
            bail!("free_before_reset_hours must be between 0 and 168");
        }
        if !(1..=4096).contains(&self.antigravity_replay_mib) {
            bail!("antigravity_replay_mib must be between 1 and 4096");
        }
        if !(1..=1440).contains(&self.antigravity_replay_idle_minutes) {
            bail!("antigravity_replay_idle_minutes must be between 1 and 1440");
        }
        if let Some(p) = &self.default_provider {
            if crate::subscriptions::SubscriptionProvider::parse(p).is_none() {
                bail!("unknown default provider");
            }
        }
        if self.quota_display.keys().any(|p| {
            crate::subscriptions::SubscriptionProvider::parse(p).map(|p| p.as_str()) != Some(p)
        }) {
            bail!("unknown quota display provider");
        }
        if self.quota_windows.keys().any(|p| {
            crate::subscriptions::SubscriptionProvider::parse(p).map(|p| p.as_str()) != Some(p)
        }) {
            bail!("unknown quota windows provider");
        }
        if self.quota_windows.values().any(|choice| {
            choice.hidden.len() > MAX_HIDDEN_WINDOWS
                || !choice.hidden.iter().all(|id| window_id(id))
                || choice.primary.as_deref().is_some_and(|id| !window_id(id))
        }) {
            bail!(
                "quota windows take at most {MAX_HIDDEN_WINDOWS} hidden ids of 1 to {MAX_WINDOW_ID} characters"
            );
        }
        Ok(())
    }

    pub fn antigravity_replay_bytes(&self) -> usize {
        self.antigravity_replay_mib as usize * 1024 * 1024
    }

    pub fn antigravity_replay_idle(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.antigravity_replay_idle_minutes) * 60)
    }

    pub fn free_before_reset_seconds(&self) -> i64 {
        i64::from(self.free_before_reset_hours) * 3600
    }

    pub fn quota_windows(
        &self,
        provider: crate::subscriptions::SubscriptionProvider,
    ) -> Option<&QuotaWindows> {
        self.quota_windows.get(provider.as_str())
    }

    pub fn quota_display(
        &self,
        provider: crate::subscriptions::SubscriptionProvider,
    ) -> QuotaDisplay {
        self.quota_display
            .get(provider.as_str())
            .copied()
            .unwrap_or_default()
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self = toml::from_str(&content).context("invalid configuration; douane 0.2 uses subscription accounts instead of vault/provider/client sections")?;
        config.settings.validate()?;
        config.server.normalize()?;
        let parent = path.parent().unwrap_or(Path::new("."));
        if config.server.auth_dir.is_relative() {
            config.server.auth_dir = parent.join(&config.server.auth_dir);
        }
        if config.server.database.is_relative() {
            config.server.database = parent.join(&config.server.database);
        }
        Ok(config)
    }
    pub async fn save(&self, path: &Path) -> Result<()> {
        let data = toml::to_string_pretty(self)?;
        let temporary = path.with_extension("toml.tmp");
        tokio::fs::write(&temporary, data).await?;
        tokio::fs::rename(&temporary, path).await?;
        Ok(())
    }
}
