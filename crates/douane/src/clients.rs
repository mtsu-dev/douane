//! Client keys: which subscriptions each caller may use, and the share of a
//! provider's quota windows it holds.
//!
//! Every call to the client API needs a client key: with no client stored,
//! douane refuses every call until one is created. A share is a percentage of each
//! account-wide window (five hours, day, week, month) of every account in a
//! pool. Unused shares stay reserved for their holders until the last seventh
//! of longer windows, when anyone may use them; what no share covers is common.
//! Five-hour shares are always hard caps and keep their reservations until reset.
//! Account shares override pool shares. When authorized holders promise more
//! than 100%, unused quota is common and each holder has its own usage cap.
//! In the hours before an account's weekly reset (`free_before_reset_hours`),
//! only longer-window shares stop holding callers back.
//!
//! Usage is attributed, not read: each rise of a window's utilization goes to
//! the clients whose calls ended on that account since the previous reading,
//! weighted by tokens, or to the calls still running, or to "outside" when no
//! call through douane can explain it.

use crate::subscriptions::{
    AccountCredential, QuotaSnapshot, parse_timestamp, reserve_kind, window_seconds, write_private,
};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Mutex, RwLock},
};

pub const KEY_PREFIX: &str = "dk_";
/// Message start of a refusal caused by shares, answered with 429.
pub const SHARE_USED_UP: &str = "this client's share of";
/// A reset that moves by less than this is the same window read again.
const RESET_TOLERANCE: i64 = 600;
/// A fall of usage this large within one window means it was reset early.
const NEW_WINDOW_DROP: f64 = 10.0;
const MAX_CLIENTS: usize = 200;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Client {
    pub id: String,
    pub name: String,
    /// SHA-256 of the key, hex. The key itself is shown once and never stored.
    pub key_hash: String,
    /// The key's prefix and last four characters, to tell keys apart.
    pub key_hint: String,
    pub created_at: String,
    #[serde(default)]
    pub access: Access,
    /// Percent of each account-wide window, by pool (provider or route).
    #[serde(default)]
    pub shares: BTreeMap<String, f64>,
    /// Per-account percentages, overriding the pool's share without granting access.
    #[serde(default)]
    pub account_shares: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Access {
    /// Every account, accounts connected later included.
    #[serde(default = "yes")]
    pub all: bool,
    /// Pools whose every account is allowed.
    #[serde(default)]
    pub pools: BTreeSet<String>,
    /// Single accounts allowed beyond the pools.
    #[serde(default)]
    pub accounts: BTreeSet<String>,
}

fn yes() -> bool {
    true
}

impl Default for Access {
    fn default() -> Self {
        Self {
            all: true,
            pools: BTreeSet::new(),
            accounts: BTreeSet::new(),
        }
    }
}

impl Access {
    pub fn allows(&self, account: &AccountCredential) -> bool {
        self.allows_in(account.pool(), &account.id)
    }

    pub fn allows_summary(&self, account: &crate::subscriptions::AccountSummary) -> bool {
        let pool = account
            .route
            .as_deref()
            .unwrap_or(account.provider.as_str());
        self.allows_in(pool, &account.id)
    }

    fn allows_in(&self, pool: &str, account: &str) -> bool {
        self.all || self.pools.contains(pool) || self.accounts.contains(account)
    }
}

impl Client {
    pub fn share_for(&self, pool: &str, account_id: &str) -> Option<f64> {
        self.account_shares
            .get(account_id)
            .or_else(|| self.shares.get(pool))
            .copied()
    }

    /// The listing shown to the dashboard: everything but the key hash.
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "key_hint": self.key_hint,
            "created_at": self.created_at,
            "access": self.access,
            "shares": self.shares,
            "account_shares": self.account_shares,
        })
    }
}

/// Attributed usage of one account-wide window of one account.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowUsage {
    pub account_id: String,
    /// `five-hour`, `daily`, `weekly` or `monthly`.
    pub kind: String,
    /// Unix seconds.
    pub reset: i64,
    /// Percent of the window used at the last reading.
    pub used: f64,
    /// Percent points attributed to each client id; `""` is a call without key.
    #[serde(default)]
    pub clients: BTreeMap<String, f64>,
    /// Percent points no call through douane explains.
    #[serde(default)]
    pub outside: f64,
}

#[derive(Default)]
struct Usage {
    windows: Vec<WindowUsage>,
    /// Token weight of calls ended since the last rise, by account then client.
    pending: BTreeMap<String, BTreeMap<String, f64>>,
    /// Calls running, by account then client.
    running: BTreeMap<String, BTreeMap<String, u32>>,
    dirty: bool,
}

/// One account-wide window of a snapshot with known usage and reset.
struct Reading {
    kind: &'static str,
    used: f64,
    reset: i64,
}

fn readings(snapshot: &QuotaSnapshot) -> Vec<Reading> {
    let mut out: Vec<Reading> = vec![];
    for window in &snapshot.windows {
        let Some(kind) = reserve_kind(&window.id) else {
            continue;
        };
        let (Some(remaining), Some(reset)) = (
            window.remaining_percent,
            window
                .reset_at
                .as_deref()
                .and_then(parse_timestamp)
                .map(|t| t.unix_timestamp()),
        ) else {
            continue;
        };
        if !remaining.is_finite() || out.iter().any(|r| r.kind == kind) {
            continue;
        }
        out.push(Reading {
            kind,
            used: (100.0 - remaining).clamp(0.0, 100.0),
            reset,
        });
    }
    out
}

/// What a call costs relative to another on the same account: Anthropic's
/// price ratios between fresh input, cache writes, cache reads and output.
/// `input_tokens` counts cache reads and writes, as the ledger stores it.
pub fn call_weight(call: &crate::ledger::Call) -> f64 {
    let fresh = call
        .input_tokens
        .saturating_sub(call.cache_read_tokens + call.cache_write_tokens);
    fresh as f64
        + 1.25 * call.cache_write_tokens as f64
        + 0.1 * call.cache_read_tokens as f64
        + 5.0 * call.output_tokens as f64
}

pub fn key_hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// The key a caller presents, whatever its SDK calls it.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    header("authorization")
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or_else(|| header("x-api-key"))
        .or_else(|| header("x-goog-api-key"))
        .or_else(|| header("api-key"))
}

#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// No client exists yet, so no key can be right.
    NoClient,
    Missing,
    Unknown,
}

impl Refusal {
    pub fn message(&self) -> &'static str {
        match self {
            Refusal::NoClient => {
                "douane has no client yet: create one on the Clients page or with `douanectl client add`, then send its key as the API key"
            }
            Refusal::Missing => {
                "a douane client key is required: create one on the Clients page and send it as the API key"
            }
            Refusal::Unknown => "unknown douane client key",
        }
    }
}

/// Input of a client creation or change from the management API.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPatch {
    pub name: Option<String>,
    pub access: Option<Access>,
    pub shares: Option<BTreeMap<String, f64>>,
    pub account_shares: Option<BTreeMap<String, f64>>,
    /// Replace the key; the new one is returned once.
    #[serde(default)]
    pub rotate_key: bool,
}

pub struct Clients {
    path: PathBuf,
    usage_path: PathBuf,
    list: RwLock<Vec<Client>>,
    usage: Mutex<Usage>,
    /// Serializes changes so each write starts from the last one.
    write: Mutex<()>,
}

fn new_key() -> String {
    format!("{KEY_PREFIX}{}", hex::encode(rand::random::<[u8; 24]>()))
}

fn hint(key: &str) -> String {
    format!("{KEY_PREFIX}…{}", &key[key.len().saturating_sub(4)..])
}

fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err("name must be 1 to 64 printable characters".into());
    }
    Ok(name.to_owned())
}

fn check_shares(shares: &BTreeMap<String, f64>) -> Result<(), String> {
    for (pool, share) in shares {
        if pool.is_empty() || pool.len() > 64 {
            return Err("a share names a provider or route".into());
        }
        if !share.is_finite() || *share <= 0.0 || *share > 100.0 {
            return Err(format!(
                "the share of {pool} must be above 0 and at most 100"
            ));
        }
    }
    Ok(())
}

impl Clients {
    /// The files carry no `.json` extension: the account store reads every
    /// JSON file of the folder as an account.
    pub fn open(auth_dir: &std::path::Path) -> anyhow::Result<Self> {
        let path = auth_dir.join(".clients");
        let usage_path = auth_dir.join(".client-usage");
        let list: Vec<Client> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("{} is not a client list: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e.into()),
        };
        let windows: Vec<WindowUsage> = std::fs::read(&usage_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Ok(Self {
            path,
            usage_path,
            list: RwLock::new(list),
            usage: Mutex::new(Usage {
                windows,
                ..Usage::default()
            }),
            write: Mutex::new(()),
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Client>> {
        self.list.read().unwrap_or_else(|e| e.into_inner())
    }

    fn usage(&self) -> std::sync::MutexGuard<'_, Usage> {
        self.usage.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn list(&self) -> Vec<Client> {
        self.read().clone()
    }

    /// The client whose key the caller presents. There is no keyless access.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Client, Refusal> {
        let list = self.read();
        if list.is_empty() {
            return Err(Refusal::NoClient);
        }
        let key = presented_key(headers).ok_or(Refusal::Missing)?;
        let hash = key_hash(key);
        list.iter()
            .find(|c| c.key_hash == hash)
            .cloned()
            .ok_or(Refusal::Unknown)
    }

    fn save(&self, list: &[Client]) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        write_private(&self.path, &bytes).map_err(|_| "the client list could not be saved".into())
    }

    /// Applies `change` to a copy, checks and writes it, then publishes it.
    fn change<T>(
        &self,
        change: impl FnOnce(&mut Vec<Client>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _write = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut list = self.list();
        let out = change(&mut list)?;
        self.save(&list)?;
        *self.list.write().unwrap_or_else(|e| e.into_inner()) = list;
        Ok(out)
    }

    /// The new client and its key, shown once.
    pub fn create(&self, patch: ClientPatch) -> Result<(Client, String), String> {
        let name = check_name(patch.name.as_deref().unwrap_or(""))?;
        let shares = patch.shares.unwrap_or_default();
        check_shares(&shares)?;
        let account_shares = patch.account_shares.unwrap_or_default();
        check_shares(&account_shares)?;
        let key = new_key();
        let client = self.change(|list| {
            if list.len() >= MAX_CLIENTS {
                return Err(format!("at most {MAX_CLIENTS} clients"));
            }
            let id = loop {
                let id = hex::encode(rand::random::<[u8; 4]>());
                if !list.iter().any(|c| c.id == id) {
                    break id;
                }
            };
            let client = Client {
                id,
                name,
                key_hash: key_hash(&key),
                key_hint: hint(&key),
                created_at: crate::subscriptions::now(),
                access: patch.access.unwrap_or_default(),
                shares,
                account_shares,
            };
            list.push(client.clone());
            Ok(client)
        })?;
        Ok((client, key))
    }

    /// The changed client, and its new key when one was asked for.
    pub fn update(&self, id: &str, patch: ClientPatch) -> Result<(Client, Option<String>), String> {
        let name = patch.name.as_deref().map(check_name).transpose()?;
        if let Some(shares) = &patch.shares {
            check_shares(shares)?;
        }
        if let Some(shares) = &patch.account_shares {
            check_shares(shares)?;
        }
        let key = patch.rotate_key.then(new_key);
        let client = self.change(|list| {
            let client = list
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or("client not found")?;
            if let Some(name) = name {
                client.name = name;
            }
            if let Some(access) = patch.access {
                client.access = access;
            }
            if let Some(shares) = patch.shares {
                client.shares = shares;
            }
            if let Some(shares) = patch.account_shares {
                client.account_shares = shares;
            }
            if let Some(key) = &key {
                client.key_hash = key_hash(key);
                client.key_hint = hint(key);
            }
            Ok(client.clone())
        })?;
        Ok((client, key))
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        self.change(|list| {
            let before = list.len();
            list.retain(|c| c.id != id);
            if list.len() == before {
                Err("client not found".into())
            } else {
                Ok(())
            }
        })
    }

    /// Replaces or adds clients from an export, keeping their key hashes.
    /// Replaces or adds clients from an export, keeping their key hashes.
    /// `accounts` maps account ids of the export to the ids they got here.
    /// With `dry_run`, only checks that the restore would be accepted.
    pub fn restore(
        &self,
        mut restored: Vec<Client>,
        accounts: &BTreeMap<String, String>,
        dry_run: bool,
    ) -> Result<usize, String> {
        for client in &mut restored {
            check_name(&client.name)?;
            check_shares(&client.shares)?;
            check_shares(&client.account_shares)?;
            if client.id.is_empty() || client.key_hash.len() != 64 {
                return Err("a restored client lacks its id or key hash".into());
            }
            client.access.accounts = std::mem::take(&mut client.access.accounts)
                .into_iter()
                .map(|id| accounts.get(&id).cloned().unwrap_or(id))
                .collect();
            let mut account_shares = BTreeMap::new();
            for (id, share) in std::mem::take(&mut client.account_shares) {
                let id = accounts.get(&id).cloned().unwrap_or(id);
                if account_shares.insert(id, share).is_some() {
                    return Err("conflicting account shares after account id remapping".into());
                }
            }
            client.account_shares = account_shares;
        }
        let count = restored.len();
        let merge = move |list: &mut Vec<Client>| {
            for client in restored {
                list.retain(|c| c.id != client.id && c.key_hash != client.key_hash);
                list.push(client);
            }
            if list.len() > MAX_CLIENTS {
                return Err(format!("at most {MAX_CLIENTS} clients"));
            }
            Ok(count)
        };
        if dry_run {
            let mut list = self.list();
            merge(&mut list)?;
            return Ok(count);
        }
        self.change(merge)
    }

    pub fn call_started(&self, account: &str, client: Option<&str>) {
        let mut usage = self.usage();
        *usage
            .running
            .entry(account.to_owned())
            .or_default()
            .entry(client.unwrap_or("").to_owned())
            .or_default() += 1;
    }

    pub fn call_ended(&self, account: &str, client: Option<&str>, weight: f64) {
        let client = client.unwrap_or("");
        let mut usage = self.usage();
        if let Some(running) = usage.running.get_mut(account) {
            if let Some(count) = running.get_mut(client) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    running.remove(client);
                }
            }
            if running.is_empty() {
                usage.running.remove(account);
            }
        }
        if weight > 0.0 && weight.is_finite() {
            *usage
                .pending
                .entry(account.to_owned())
                .or_default()
                .entry(client.to_owned())
                .or_default() += weight;
        }
    }

    /// Attributes each rise of the account's windows since the last reading.
    pub fn observe(&self, snapshot: &QuotaSnapshot, now: i64) {
        let readings = readings(snapshot);
        if readings.is_empty() {
            return;
        }
        let account = snapshot.account_id.as_str();
        let mut usage = self.usage();
        let usage = &mut *usage;
        let running: Vec<(String, f64)> = usage
            .running
            .get(account)
            .map(|r| r.iter().map(|(c, n)| (c.clone(), *n as f64)).collect())
            .unwrap_or_default();
        let weights: Vec<(String, f64)> = match usage.pending.get(account) {
            Some(pending) if !pending.is_empty() => {
                pending.iter().map(|(c, w)| (c.clone(), *w)).collect()
            }
            _ => running.clone(),
        };
        let total: f64 = weights.iter().map(|(_, w)| w).sum();
        let mut rose = false;
        let mut five_hour_reset = false;
        for reading in readings {
            let position = usage
                .windows
                .iter()
                .position(|w| w.account_id == account && w.kind == reading.kind);
            let window = match position {
                Some(index) => &mut usage.windows[index],
                None => {
                    // First sight: what was used before has no known origin.
                    usage.windows.push(WindowUsage {
                        account_id: account.to_owned(),
                        kind: reading.kind.to_owned(),
                        reset: reading.reset,
                        used: reading.used,
                        clients: BTreeMap::new(),
                        outside: 0.0,
                    });
                    usage.dirty = true;
                    continue;
                }
            };
            // A later reset, or usage falling by more than sources disagree
            // (usage reads and inference headers round differently), is a
            // new window.
            let new_window = reading.reset > window.reset + RESET_TOLERANCE
                || reading.used + NEW_WINDOW_DROP < window.used;
            let new_five_hour = new_window && reading.kind == "five-hour";
            five_hour_reset |= new_five_hour;
            if new_window {
                window.reset = reading.reset;
                window.used = 0.0;
                window.clients.clear();
                window.outside = 0.0;
                usage.dirty = true;
            }
            let delta = reading.used - window.used;
            // A slightly lower reading is the other source's rounding: the
            // highest reading stays the base, so an oscillation counts once.
            if delta <= 1e-9 {
                continue;
            }
            rose = true;
            // Pending completions belong to the previous five-hour window.
            // Only active calls can explain usage first observed after its reset.
            let (weights, total) = if new_five_hour {
                (&running, running.iter().map(|(_, w)| w).sum())
            } else {
                (&weights, total)
            };
            if total > 0.0 {
                for (client, weight) in weights {
                    *window.clients.entry(client.clone()).or_default() += delta * weight / total;
                }
            } else {
                window.outside += delta;
            }
            window.used = reading.used;
            usage.dirty = true;
        }
        if rose || five_hour_reset {
            usage.pending.remove(account);
        }
        // Windows that ended a day ago have nothing left to tell.
        let before = usage.windows.len();
        usage.windows.retain(|w| w.reset > now - 86_400);
        usage.dirty |= usage.windows.len() != before;
    }

    /// Drops the usage of accounts that no longer exist.
    pub fn forget_accounts(&self, existing: &BTreeSet<String>) {
        let mut usage = self.usage();
        let before = usage.windows.len();
        usage.windows.retain(|w| existing.contains(&w.account_id));
        usage.pending.retain(|id, _| existing.contains(id));
        usage.dirty |= usage.windows.len() != before;
    }

    pub fn save_usage(&self) {
        let bytes = {
            let mut usage = self.usage();
            if !usage.dirty {
                return;
            }
            usage.dirty = false;
            serde_json::to_vec(&usage.windows).unwrap_or_default()
        };
        if let Err(error) = write_private(&self.usage_path, &bytes) {
            tracing::warn!(%error, "client usage was not saved");
        }
    }

    /// Current windows with their attribution.
    pub fn usage_report(&self, now: i64) -> Vec<WindowUsage> {
        self.usage()
            .windows
            .iter()
            .filter(|w| w.reset > now)
            .cloned()
            .collect()
    }

    /// Effective holders that may actually use this account.
    fn holders(&self, pool: &str, account: &str) -> Vec<(String, f64)> {
        self.read()
            .iter()
            .filter(|c| c.access.allows_in(pool, account))
            .filter_map(|c| c.share_for(pool, account).map(|s| (c.id.clone(), s)))
            .collect()
    }

    /// Attributed usage, remaining budget and whether shares forbid another call.
    /// Overbooking removes reservations and caps each holder at its own share.
    /// Physical exhaustion stays the scheduler's responsibility (including credits).
    fn budget(
        &self,
        holders: &[(String, f64)],
        me: &str,
        account: &str,
        reading: &Reading,
        now: i64,
    ) -> (f64, f64, bool) {
        let usage = self.usage();
        let window = usage.windows.iter().find(|w| {
            w.account_id == account
                && w.kind == reading.kind
                && (w.reset - reading.reset).abs() <= RESET_TOLERANCE
        });
        let used_by = |id: &str| {
            window
                .and_then(|w| w.clients.get(id))
                .copied()
                .unwrap_or(0.0)
        };
        let mine = used_by(me);
        let physical = (100.0 - reading.used).max(0.0);
        let overbooked = holders.iter().map(|(_, s)| s).sum::<f64>() > 100.0 + 1e-9;
        let cap = if reading.kind == "five-hour" || overbooked {
            holders
                .iter()
                .find(|(id, _)| id == me)
                .map(|(_, s)| (s - mine).max(0.0))
        } else {
            None
        };
        let length = window_seconds(reading.kind).unwrap_or(0.0);
        let free = if overbooked
            || (reading.kind != "five-hour" && (reading.reset - now) as f64 <= length / 7.0)
        {
            0.0
        } else {
            holders
                .iter()
                .filter(|(id, _)| id != me)
                .map(|(id, share)| (share - used_by(id)).max(0.0))
                .sum()
        };
        let remaining = (physical - free).max(0.0);
        (
            mine,
            cap.map_or(remaining, |c| remaining.min(c)),
            cap.is_some_and(|c| c <= 1e-9) || (free > 0.0 && physical <= free + 1e-9),
        )
    }

    /// The reset of the window where reservations or an overbooked holder's
    /// own cap forbid another call; `None` when shares allow it.
    /// Within `free_before` seconds of the account's weekly reset, shares
    /// no longer hold anyone back on longer windows. Five-hour limits stay enforced.
    pub fn share_blocks(
        &self,
        client: Option<&Client>,
        account: &AccountCredential,
        snapshot: Option<&QuotaSnapshot>,
        now: i64,
        free_before: i64,
    ) -> Option<i64> {
        let snapshot = snapshot?;
        let holders = self.holders(account.pool(), &account.id);
        if holders.is_empty() {
            return None;
        }
        let weekly_free = snapshot.longest_window_ends_within(free_before, now);
        let me = client.map(|c| c.id.as_str()).unwrap_or("");
        readings(snapshot)
            .into_iter()
            .filter(|r| r.reset > now)
            .filter(|r| !weekly_free || r.kind == "five-hour")
            .find(|reading| self.budget(&holders, me, &account.id, reading, now).2)
            .map(|r| r.reset)
    }

    /// The snapshot as `client` sees it: with an effective share, each
    /// account-wide window shows the part of what it may use that it has
    /// used, so a bar at 100% means it is held back. A client without a share
    /// sees the account's whole quota, other shares included.
    /// Longer windows whose shares are freed before the weekly reset stay raw.
    /// Five-hour windows always show the client's capped budget.
    pub fn client_view(
        &self,
        client: &Client,
        mut snapshot: QuotaSnapshot,
        pool: &str,
        now: i64,
        free_before: i64,
    ) -> QuotaSnapshot {
        if client.share_for(pool, &snapshot.account_id).is_none() {
            return snapshot;
        }
        let weekly_free = snapshot.longest_window_ends_within(free_before, now);
        let holders = self.holders(pool, &snapshot.account_id);
        let account = snapshot.account_id.clone();
        let readings = readings(&snapshot);
        for window in &mut snapshot.windows {
            let Some(kind) = reserve_kind(&window.id) else {
                continue;
            };
            let Some(reading) = readings.iter().find(|r| r.kind == kind) else {
                continue;
            };
            if reading.reset <= now || (weekly_free && reading.kind != "five-hour") {
                continue;
            }
            let (mine, remaining, _) = self.budget(&holders, &client.id, &account, reading, now);
            let budget = mine + remaining;
            let used = if budget <= 1e-9 {
                100.0
            } else {
                (mine / budget * 100.0).clamp(0.0, 100.0)
            };
            window.remaining_percent = Some(100.0 - used);
            window.used = None;
            window.limit = None;
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscriptions::{QuotaWindow, SubscriptionProvider};

    const NOW: i64 = 2_000_000_000;

    fn clients() -> (tempfile::TempDir, Clients) {
        let directory = tempfile::TempDir::new().unwrap();
        let clients = Clients::open(directory.path()).unwrap();
        (directory, clients)
    }

    fn named(name: &str) -> ClientPatch {
        ClientPatch {
            name: Some(name.into()),
            ..ClientPatch::default()
        }
    }

    fn snapshot(account: &str, used: f64, reset: i64) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: account.into(),
            provider: SubscriptionProvider::Claude,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                label: "Weekly".into(),
                remaining_percent: Some(100.0 - used),
                reset_at: Some(
                    time::OffsetDateTime::from_unix_timestamp(reset)
                        .unwrap()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                ),
                used: None,
                limit: None,
            }],
            updated_at: crate::subscriptions::now(),
            error: None,
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        }
    }

    fn account(id: &str) -> AccountCredential {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "provider": "claude", "access_token": "fixture",
            "enabled": true, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    fn bearer(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        headers
    }

    #[test]
    fn shares_totaling_100_despite_float_rounding_keep_their_reservations() {
        let (_dir, clients) = clients();
        for percent in [0.2, 83.9, 15.9] {
            clients
                .create(ClientPatch {
                    shares: Some([("claude".into(), percent)].into()),
                    ..named(&format!("holder {percent}"))
                })
                .unwrap();
        }
        let (unshared, _) = clients.create(named("unshared")).unwrap();
        let reset = NOW + 3 * 86_400;
        let full = snapshot("a", 99.0, reset);
        clients.observe(&full, NOW);
        assert_eq!(
            clients.share_blocks(Some(&unshared), &account("a"), Some(&full), NOW, 0),
            Some(reset)
        );
    }

    #[test]
    fn restore_rejects_colliding_account_overrides_without_changing_clients() {
        for mapped_orphan in [false, true] {
            let (dir, clients) = clients();
            let (original, _) = clients.create(named("unchanged")).unwrap();
            let before = std::fs::read(dir.path().join(".clients")).unwrap();
            let mut restored = original.clone();
            restored.name = "restored".into();
            restored.account_shares = [("a".into(), 10.0), ("b".into(), 95.0)].into();
            let mapping = if mapped_orphan {
                [("a".into(), "c".into()), ("b".into(), "c".into())].into()
            } else {
                [("a".into(), "b".into())].into()
            };
            for dry_run in [true, false] {
                let error = clients
                    .restore(vec![restored.clone()], &mapping, dry_run)
                    .unwrap_err();
                assert!(error.contains("conflicting account shares"), "{error}");
                assert_eq!(clients.list(), vec![original.clone()]);
                assert_eq!(std::fs::read(dir.path().join(".clients")).unwrap(), before);
            }
        }
    }

    #[test]
    fn overbooked_95_plus_10_shares_are_accepted() {
        let (_dir, clients) = clients();
        for percent in [95.0, 10.0] {
            clients
                .create(ClientPatch {
                    name: Some(format!("holder {percent}")),
                    shares: Some([("claude".to_owned(), percent)].into()),
                    ..Default::default()
                })
                .unwrap();
        }
        assert_eq!(clients.list().len(), 2);
        let (_target_dir, target) = self::clients();
        target
            .restore(clients.list(), &BTreeMap::new(), true)
            .unwrap();
        target
            .restore(clients.list(), &BTreeMap::new(), false)
            .unwrap();
        assert_eq!(target.list().len(), 2);
    }

    #[test]
    fn account_shares_json_is_accepted_and_listed() {
        let (_dir, clients) = clients();
        let patch: ClientPatch = serde_json::from_value(serde_json::json!({
            "name": "account holder", "account_shares": {"a": 10.0}
        }))
        .unwrap();
        let (client, _) = clients.create(patch).unwrap();
        assert_eq!(client.public()["account_shares"]["a"], 10.0);
    }

    #[test]
    fn overbooking_is_first_consumer_wins_and_each_holder_has_a_cap() {
        for small_first in [false, true] {
            let (_dir, clients) = clients();
            let mut holders = vec![];
            for percent in [95.0, 10.0] {
                holders.push(
                    clients
                        .create(ClientPatch {
                            name: Some(format!("holder {percent}")),
                            shares: Some([("claude".into(), percent)].into()),
                            ..Default::default()
                        })
                        .unwrap()
                        .0,
                );
            }
            let (unshared, _) = clients.create(named("unshared")).unwrap();
            let a = account("a");
            let reset = NOW + 3 * 86_400;
            clients.observe(&snapshot("a", 0.0, reset), NOW);
            let (first, next, amount, leftover) = if small_first {
                (&holders[1], &holders[0], 10.0, 90.0)
            } else {
                (&holders[0], &holders[1], 95.0, 5.0)
            };
            clients.call_ended("a", Some(&first.id), 1.0);
            let after = snapshot("a", amount, reset);
            clients.observe(&after, NOW);
            assert_eq!(
                clients.share_blocks(Some(first), &a, Some(&after), NOW, 0),
                Some(reset)
            );
            assert_eq!(
                clients
                    .client_view(first, after.clone(), "claude", NOW, 0)
                    .windows[0]
                    .remaining_percent,
                Some(0.0)
            );
            assert_eq!(
                clients.share_blocks(Some(next), &a, Some(&after), NOW, 0),
                None
            );
            assert_eq!(
                clients.share_blocks(Some(&unshared), &a, Some(&after), NOW, 0),
                None
            );
            assert_eq!(
                clients
                    .client_view(&unshared, after.clone(), "claude", NOW, 0)
                    .windows[0]
                    .remaining_percent,
                Some(leftover)
            );
            // The last seventh frees reservations, but does not remove overbooked caps.
            let late = reset - 3600;
            assert_eq!(
                clients.share_blocks(Some(first), &a, Some(&after), late, 0),
                Some(reset)
            );
            assert_eq!(
                clients.share_blocks(Some(first), &a, Some(&after), late, 7200),
                None
            );
            assert_eq!(
                clients
                    .client_view(first, after, "claude", late, 7200)
                    .windows[0]
                    .remaining_percent,
                Some(leftover)
            );
            clients.call_ended("a", Some(&next.id), 1.0);
            let full = snapshot("a", 100.0, reset);
            clients.observe(&full, NOW);
            assert_eq!(
                clients.client_view(next, full, "claude", NOW, 0).windows[0].remaining_percent,
                Some(0.0)
            );
            // A new window clears the cap's attributed usage.
            let fresh = snapshot("a", 0.0, reset + 7 * 86_400);
            clients.observe(&fresh, NOW);
            assert_eq!(
                clients.share_blocks(Some(first), &a, Some(&fresh), NOW, 0),
                None
            );
        }
    }

    #[test]
    fn account_overrides_replace_pool_shares_and_only_authorized_holders_count() {
        let (_dir, clients) = clients();
        let (holder, _) = clients
            .create(ClientPatch {
                name: Some("holder".into()),
                shares: Some([("claude".into(), 95.0)].into()),
                account_shares: Some([("a".into(), 10.0)].into()),
                ..Default::default()
            })
            .unwrap();
        clients
            .create(ClientPatch {
                name: Some("restricted".into()),
                shares: Some([("claude".into(), 95.0)].into()),
                access: Some(Access {
                    all: false,
                    pools: Default::default(),
                    accounts: ["a".into()].into(),
                }),
                ..Default::default()
            })
            .unwrap();
        let (unshared, _) = clients.create(named("unshared")).unwrap();
        assert_eq!(holder.share_for("claude", "a"), Some(10.0));
        assert_eq!(holder.share_for("claude", "b"), Some(95.0));
        let reset = NOW + 3 * 86_400;
        for id in ["a", "b"] {
            clients.observe(&snapshot(id, 0.0, reset), NOW);
            clients.call_ended(id, Some(&holder.id), 1.0);
            clients.observe(&snapshot(id, 10.0, reset), NOW);
        }
        assert_eq!(
            clients.share_blocks(
                Some(&holder),
                &account("a"),
                Some(&snapshot("a", 10.0, reset)),
                NOW,
                0
            ),
            Some(reset)
        );
        assert_eq!(
            clients.share_blocks(
                Some(&holder),
                &account("b"),
                Some(&snapshot("b", 10.0, reset)),
                NOW,
                0
            ),
            None
        );
        // Only 5 points are common on b: the restricted holder cannot overbook it.
        assert_eq!(
            clients.share_blocks(
                Some(&unshared),
                &account("b"),
                Some(&snapshot("b", 15.0, reset)),
                NOW,
                0
            ),
            Some(reset)
        );
        // An account override never authorizes access, even to an unlisted pool.
        let (restricted, _) = clients
            .create(ClientPatch {
                account_shares: Some([("c".into(), 100.0)].into()),
                access: Some(Access {
                    all: false,
                    pools: Default::default(),
                    accounts: Default::default(),
                }),
                ..named("denied")
            })
            .unwrap();
        assert!(!restricted.access.allows(&account("c")));
        assert!(
            clients
                .holders("claude", "c")
                .iter()
                .all(|(id, _)| id != &restricted.id)
        );
    }

    #[test]
    fn overbooked_view_is_bounded_by_the_physical_remainder() {
        let (_dir, clients) = clients();
        let (small, _) = clients
            .create(ClientPatch {
                shares: Some([("claude".into(), 10.0)].into()),
                ..named("small")
            })
            .unwrap();
        let (large, _) = clients
            .create(ClientPatch {
                shares: Some([("claude".into(), 95.0)].into()),
                ..named("large")
            })
            .unwrap();
        let reset = NOW + 3 * 86_400;
        clients.observe(&snapshot("a", 0.0, reset), NOW);
        clients.call_ended("a", Some(&small.id), 1.0);
        clients.observe(&snapshot("a", 5.0, reset), NOW);
        clients.call_ended("a", Some(&large.id), 1.0);
        let almost_full = snapshot("a", 99.0, reset);
        clients.observe(&almost_full, NOW);
        let view = clients.client_view(&small, almost_full, "claude", NOW, 0);
        assert!((view.windows[0].remaining_percent.unwrap() - 100.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn account_shares_persist_validate_and_restore_without_broadening_access() {
        let (dir, clients) = clients();
        let (client, _) = clients
            .create(ClientPatch {
                account_shares: Some([("old".into(), 95.0), ("missing".into(), 10.0)].into()),
                access: Some(Access {
                    all: false,
                    pools: Default::default(),
                    accounts: ["old".into()].into(),
                }),
                ..named("scoped")
            })
            .unwrap();
        assert_eq!(
            Clients::open(dir.path()).unwrap().list(),
            vec![client.clone()]
        );
        let (_target_dir, target) = self::clients();
        let mapping = [("old".into(), "new".into())].into();
        target
            .restore(vec![client.clone()], &mapping, true)
            .unwrap();
        assert!(target.list().is_empty());
        target.restore(vec![client], &mapping, false).unwrap();
        let restored = target.list().remove(0);
        assert_eq!(
            restored.account_shares,
            [("new".into(), 95.0), ("missing".into(), 10.0)].into()
        );
        assert_eq!(restored.access.accounts, ["new".into()].into());
        assert!(!restored.access.all);
        assert!(restored.access.pools.is_empty());
        assert!(!restored.access.allows(&account("missing")));
        for percent in [0.0, -1.0, 101.0, f64::NAN, f64::INFINITY] {
            assert!(
                clients
                    .update(
                        &restored.id,
                        ClientPatch {
                            account_shares: Some([("a".into(), percent)].into()),
                            ..Default::default()
                        }
                    )
                    .is_err()
            );
        }
        clients
            .update(
                &restored.id,
                ClientPatch {
                    account_shares: Some(Default::default()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(clients.list()[0].account_shares.is_empty());
        // Legacy client records inherit pool shares without a migration.
        let mut legacy = serde_json::to_value(restored).unwrap();
        legacy.as_object_mut().unwrap().remove("account_shares");
        assert!(
            serde_json::from_value::<Client>(legacy)
                .unwrap()
                .account_shares
                .is_empty()
        );
    }

    #[test]
    fn every_call_needs_a_client_key_even_before_the_first_client() {
        let (_dir, clients) = clients();
        assert_eq!(
            clients.authenticate(&HeaderMap::new()),
            Err(Refusal::NoClient)
        );
        assert_eq!(
            clients.authenticate(&bearer("boite-subscription-proxy")),
            Err(Refusal::NoClient)
        );
        let (client, key) = clients.create(named("famille")).unwrap();
        assert!(key.starts_with(KEY_PREFIX));
        assert!(
            !serde_json::to_string(&clients.list())
                .unwrap()
                .contains(&key)
        );
        assert_eq!(
            clients.authenticate(&HeaderMap::new()),
            Err(Refusal::Missing)
        );
        assert_eq!(
            clients.authenticate(&bearer("boite-subscription-proxy")),
            Err(Refusal::Unknown)
        );
        assert_eq!(clients.authenticate(&bearer(&key)).unwrap().id, client.id);
        let mut anthropic = HeaderMap::new();
        anthropic.insert("authorization", "Bearer ".parse().unwrap());
        anthropic.insert("x-api-key", key.parse().unwrap());
        assert_eq!(clients.authenticate(&anthropic).unwrap().id, client.id);
        // A rotated key replaces the old one.
        let (_, rotated) = clients
            .update(
                &client.id,
                ClientPatch {
                    rotate_key: true,
                    ..ClientPatch::default()
                },
            )
            .unwrap();
        assert_eq!(clients.authenticate(&bearer(&key)), Err(Refusal::Unknown));
        assert!(clients.authenticate(&bearer(&rotated.unwrap())).is_ok());
    }

    #[test]
    fn clients_survive_a_restart() {
        let (dir, clients) = clients();
        let (client, key) = clients.create(named("m2")).unwrap();
        drop(clients);
        let reopened = Clients::open(dir.path()).unwrap();
        assert_eq!(reopened.list(), vec![client]);
        assert!(reopened.authenticate(&bearer(&key)).is_ok());
    }

    #[test]
    fn access_limits_a_client_to_its_pools_and_accounts() {
        let access = Access {
            all: false,
            pools: ["codex".to_owned()].into(),
            accounts: ["famille".to_owned()].into(),
        };
        assert!(access.allows(&account("famille")));
        assert!(!access.allows(&account("perso")));
        assert!(Access::default().allows(&account("perso")));
    }

    #[test]
    fn individual_shares_remain_valid_with_overbooking() {
        let (_dir, clients) = clients();
        let share = |pool: &str, percent: f64| ClientPatch {
            name: Some("x".into()),
            shares: Some([(pool.to_owned(), percent)].into()),
            ..ClientPatch::default()
        };
        clients.create(share("claude", 60.0)).unwrap();
        clients.create(share("claude", 50.0)).unwrap();
        clients.create(share("claude", 40.0)).unwrap();
        clients.create(share("codex", 100.0)).unwrap();
        assert!(clients.create(share("claude", 0.0)).is_err());
        assert_eq!(clients.list().len(), 4);
    }

    #[test]
    fn a_rise_goes_to_the_calls_that_ended_by_weight_then_to_running_calls_then_outside() {
        let (_dir, clients) = clients();
        let reset = NOW + 3 * 86_400;
        clients.observe(&snapshot("a", 10.0, reset), NOW);
        clients.call_started("a", Some("perso"));
        clients.call_ended("a", Some("perso"), 300.0);
        clients.call_started("a", Some("famille"));
        clients.call_ended("a", Some("famille"), 100.0);
        clients.observe(&snapshot("a", 18.0, reset), NOW);
        // A long call still running explains the next rise.
        clients.call_started("a", Some("famille"));
        clients.observe(&snapshot("a", 20.0, reset), NOW);
        clients.call_ended("a", Some("famille"), 0.0);
        // Nobody calls through douane: someone outside used it.
        clients.observe(&snapshot("a", 23.0, reset), NOW);
        let window = &clients.usage_report(NOW)[0];
        assert!((window.clients["perso"] - 6.0).abs() < 1e-9);
        assert!((window.clients["famille"] - 4.0).abs() < 1e-9);
        assert!((window.outside - 3.0).abs() < 1e-9);
        // The weight of calls with no visible rise waits for the next one.
        clients.call_ended("a", Some("perso"), 50.0);
        clients.observe(&snapshot("a", 23.0, reset), NOW);
        clients.observe(&snapshot("a", 24.0, reset), NOW);
        assert!((clients.usage_report(NOW)[0].clients["perso"] - 7.0).abs() < 1e-9);
        // Two sources rounding differently count each rise once.
        clients.observe(&snapshot("a", 22.0, reset), NOW);
        clients.call_ended("a", Some("perso"), 50.0);
        clients.observe(&snapshot("a", 24.0, reset), NOW);
        clients.observe(&snapshot("a", 25.0, reset), NOW);
        assert!((clients.usage_report(NOW)[0].clients["perso"] - 8.0).abs() < 1e-9);
        // A new window starts from zero.
        clients.call_ended("a", Some("perso"), 10.0);
        clients.observe(&snapshot("a", 2.0, reset + 7 * 86_400), NOW);
        let window = &clients.usage_report(NOW)[0];
        assert_eq!(window.clients.len(), 1);
        assert!((window.clients["perso"] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn unused_shares_stay_reserved_until_the_last_seventh_of_the_window() {
        let (_dir, clients) = clients();
        let (famille, _) = clients
            .create(ClientPatch {
                name: Some("famille".into()),
                shares: Some([("claude".to_owned(), 25.0)].into()),
                ..ClientPatch::default()
            })
            .unwrap();
        let (perso, _) = clients.create(named("perso")).unwrap();
        let a = account("a");
        let reset = NOW + 3 * 86_400;
        clients.observe(&snapshot("a", 0.0, reset), NOW);
        clients.call_ended("a", Some(&perso.id), 1.0);
        clients.observe(&snapshot("a", 70.0, reset), NOW);
        // Perso may still use 5 points before touching famille's 25.
        assert_eq!(
            clients.share_blocks(Some(&perso), &a, Some(&snapshot("a", 70.0, reset)), NOW, 0),
            None
        );
        clients.call_ended("a", Some(&perso.id), 1.0);
        clients.observe(&snapshot("a", 75.0, reset), NOW);
        let full = snapshot("a", 75.0, reset);
        assert_eq!(
            clients.share_blocks(Some(&perso), &a, Some(&full), NOW, 0),
            Some(reset)
        );
        // Famille keeps its share.
        assert_eq!(
            clients.share_blocks(Some(&famille), &a, Some(&full), NOW, 0),
            None
        );
        // Famille's bar shows nothing used of its 25; perso, without a
        // share, sees the account's whole quota, famille's share included.
        let view = |c: &Client| {
            clients
                .client_view(c, full.clone(), "claude", NOW, 0)
                .windows[0]
                .remaining_percent
        };
        assert_eq!(view(&perso), Some(25.0));
        assert_eq!(view(&famille), Some(100.0));
        // In the last day of the week, famille's unused share opens to all.
        let late = NOW + 3 * 86_400 - 20 * 3600;
        assert_eq!(
            clients.share_blocks(Some(&perso), &a, Some(&full), late, 0),
            None
        );
        // Accounts of a pool without shares never hold anyone back.
        let mut codex = account("c");
        codex.provider = SubscriptionProvider::Codex;
        assert_eq!(
            clients.share_blocks(Some(&perso), &codex, Some(&full), NOW, 0),
            None
        );
    }

    #[test]
    fn five_hour_caps_survive_common_quota_and_both_release_periods() {
        for overbooked in [false, true] {
            for five_hour_left in [2 * 3600, 15 * 60] {
                for weekly_free in [false, true] {
                    let (_dir, clients) = clients();
                    let mut holders = vec![];
                    for (name, share) in [
                        ("main", 90.0),
                        ("other", if overbooked { 15.0 } else { 4.0 }),
                        ("third", 4.0),
                    ] {
                        holders.push(
                            clients
                                .create(ClientPatch {
                                    name: Some(name.into()),
                                    account_shares: Some([("a".into(), share)].into()),
                                    ..Default::default()
                                })
                                .unwrap()
                                .0,
                        );
                    }
                    let main = &holders[0];
                    let reset = NOW + five_hour_left;
                    let weekly_reset = NOW + if weekly_free { 3600 } else { 3 * 86_400 };
                    let quota = |used| {
                        let mut q = snapshot("a", 20.0, weekly_reset);
                        let mut short = snapshot("a", used, reset).windows.remove(0);
                        short.id = "five-hour".into();
                        q.windows.push(short);
                        q
                    };
                    clients.observe(&quota(0.0), NOW);
                    clients.call_ended("a", Some(&main.id), 1.0);
                    clients.observe(&quota(89.0), NOW);
                    let view = clients.client_view(main, quota(89.0), "claude", NOW, 8 * 3600);
                    assert!(
                        (view.windows[1].remaining_percent.unwrap() - 100.0 / 90.0).abs() < 1e-9
                    );
                    assert_eq!(
                        clients.share_blocks(
                            Some(main),
                            &account("a"),
                            Some(&quota(89.0)),
                            NOW,
                            8 * 3600
                        ),
                        None
                    );
                    clients.call_ended("a", Some(&main.id), 1.0);
                    clients.observe(&quota(90.0), NOW);
                    assert_eq!(
                        clients.share_blocks(
                            Some(main),
                            &account("a"),
                            Some(&quota(90.0)),
                            NOW,
                            8 * 3600
                        ),
                        Some(reset)
                    );
                    let view = clients.client_view(main, quota(90.0), "claude", NOW, 8 * 3600);
                    assert_eq!(view.windows[1].remaining_percent, Some(0.0));
                    if weekly_free {
                        assert_eq!(view.windows[0].remaining_percent, Some(80.0));
                    }
                    let mut fresh = quota(0.0);
                    fresh.windows[1].reset_at = snapshot("a", 0.0, reset + 18_000)
                        .windows
                        .remove(0)
                        .reset_at;
                    clients.observe(&fresh, NOW);
                    assert_eq!(
                        clients.share_blocks(
                            Some(main),
                            &account("a"),
                            Some(&fresh),
                            NOW,
                            8 * 3600
                        ),
                        None
                    );
                }
            }
        }
    }

    #[test]
    fn five_hour_reservations_still_protect_other_clients_near_resets() {
        let (_dir, clients) = clients();
        clients
            .create(ClientPatch {
                name: Some("holder".into()),
                shares: Some([("claude".into(), 90.0)].into()),
                ..Default::default()
            })
            .unwrap();
        let (unshared, _) = clients.create(named("unshared")).unwrap();
        let mut quota = snapshot("a", 20.0, NOW + 3600);
        let reset = NOW + 15 * 60;
        let mut short = snapshot("a", 10.0, reset).windows.remove(0);
        short.id = "five-hour".into();
        quota.windows.push(short);
        clients.observe(&quota, NOW);
        assert_eq!(
            clients.share_blocks(Some(&unshared), &account("a"), Some(&quota), NOW, 8 * 3600),
            Some(reset)
        );
    }

    #[test]
    fn old_pending_calls_cannot_spend_another_clients_new_five_hour_window() {
        for first_used in [0.0, 10.0] {
            for running in [false, true] {
                let (_dir, clients) = clients();
                let mut holders = vec![];
                for share in [10.0, 20.0] {
                    holders.push(
                        clients
                            .create(ClientPatch {
                                name: Some(format!("holder {share}")),
                                shares: Some([("claude".into(), share)].into()),
                                ..Default::default()
                            })
                            .unwrap()
                            .0,
                    );
                }
                let old = &holders[0];
                let next = &holders[1];
                let quota = |used, reset| {
                    let mut q = snapshot("a", 20.0, NOW + 3 * 86_400);
                    let mut short = snapshot("a", used, reset).windows.remove(0);
                    short.id = "five-hour".into();
                    q.windows.push(short);
                    q
                };
                clients.observe(&quota(9.0, NOW + 60), NOW);
                clients.call_ended("a", Some(&old.id), 300.0);
                if running {
                    clients.call_started("a", Some(&next.id));
                }
                let reset = NOW + 18_060;
                clients.observe(&quota(first_used, reset), NOW + 61);
                clients.observe(&quota(10.0, reset), NOW + 62);
                let usage = clients.usage_report(NOW + 62);
                let short = usage.iter().find(|w| w.kind == "five-hour").unwrap();
                assert_eq!(short.clients.get(&old.id).copied().unwrap_or(0.0), 0.0);
                assert_eq!(
                    short.clients.get(&next.id).copied().unwrap_or(0.0),
                    if running { 10.0 } else { 0.0 }
                );
                assert_eq!(short.outside, if running { 0.0 } else { 10.0 });
                assert_eq!(
                    clients.share_blocks(
                        Some(old),
                        &account("a"),
                        Some(&quota(10.0, reset)),
                        NOW + 62,
                        0
                    ),
                    None
                );
            }
        }
    }

    #[test]
    fn shares_stop_holding_back_hours_before_the_weekly_reset() {
        let (_dir, clients) = clients();
        let (famille, _) = clients
            .create(ClientPatch {
                name: Some("famille".into()),
                shares: Some([("claude".to_owned(), 25.0)].into()),
                ..ClientPatch::default()
            })
            .unwrap();
        let (perso, _) = clients.create(named("perso")).unwrap();
        let a = account("a");
        // The week ends in five hours; a five-hour window started an hour ago.
        let weekly = NOW + 5 * 3600;
        let five = NOW + 4 * 3600;
        let with = |used: f64| {
            let mut snapshot = snapshot("a", 10.0, weekly);
            let mut window = snapshot.windows[0].clone();
            window.id = "five-hour".into();
            window.remaining_percent = Some(100.0 - used);
            window.reset_at = Some(
                time::OffsetDateTime::from_unix_timestamp(five)
                    .unwrap()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            );
            snapshot.windows.push(window);
            snapshot
        };
        clients.observe(&with(0.0), NOW);
        clients.call_ended("a", Some(&perso.id), 1.0);
        clients.observe(&with(75.0), NOW);
        let full = with(75.0);
        // Kept to the end, famille's five-hour share holds perso back.
        assert_eq!(
            clients.share_blocks(Some(&perso), &a, Some(&full), NOW, 0),
            Some(five)
        );
        assert_eq!(
            clients.share_blocks(Some(&perso), &a, Some(&full), NOW, 4 * 3600),
            Some(five)
        );
        // Weekly release never removes the five-hour reservation.
        assert_eq!(
            clients.share_blocks(Some(&perso), &a, Some(&full), NOW, 6 * 3600),
            Some(five)
        );
        let view = |free_before: i64| {
            clients
                .client_view(&famille, full.clone(), "claude", NOW, free_before)
                .windows[1]
                .remaining_percent
        };
        assert_eq!(view(0), Some(100.0));
        assert_eq!(view(6 * 3600), Some(100.0));
    }

    #[test]
    fn usage_survives_a_restart() {
        let (dir, clients) = clients();
        let reset = NOW + 86_400;
        clients.observe(&snapshot("a", 5.0, reset), NOW);
        clients.call_ended("a", Some("perso"), 1.0);
        clients.observe(&snapshot("a", 9.0, reset), NOW);
        clients.save_usage();
        let reopened = Clients::open(dir.path()).unwrap();
        assert_eq!(reopened.usage_report(NOW), clients.usage_report(NOW));
    }
}
