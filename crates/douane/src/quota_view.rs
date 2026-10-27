//! Client quota overview served at `/v1/quotas` for Boite's native quota bars.
//!
//! Only sanitized fields leave this module: account names with masked emails,
//! provider-reported percentages, reset times, credit balances and the short
//! error categories already stored in snapshots and account summaries.

use crate::{
    settings::{QuotaDisplay, QuotaWindows, Settings},
    subscriptions::{AccountSummary, QuotaCredits, QuotaSnapshot, QuotaWindow},
    subscriptions::{SubscriptionProvider, parse_timestamp},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Display name shared by `/admin/api/providers` and `/v1/quotas`.
pub fn provider_name(provider: SubscriptionProvider) -> &'static str {
    match provider {
        SubscriptionProvider::Claude => "Claude",
        SubscriptionProvider::Codex => "Codex",
        SubscriptionProvider::Gemini => "Gemini",
        SubscriptionProvider::Antigravity => "Antigravity",
        SubscriptionProvider::Qwen => "Qwen",
        SubscriptionProvider::Kimi => "Kimi",
        SubscriptionProvider::Xai => "Grok",
        SubscriptionProvider::OpencodeGo => "OpenCode Go",
        SubscriptionProvider::Copilot => "GitHub Copilot",
        SubscriptionProvider::Meta => "Muse Code",
        SubscriptionProvider::OpenaiCompatible => "Compatible OpenAI",
    }
}

fn rank(provider: SubscriptionProvider) -> (usize, &'static str) {
    let first = [
        SubscriptionProvider::Claude,
        SubscriptionProvider::Codex,
        SubscriptionProvider::Antigravity,
    ];
    (
        first
            .iter()
            .position(|p| *p == provider)
            .unwrap_or(first.len()),
        provider_name(provider),
    )
}

type Member<'a> = (&'a AccountSummary, Option<&'a QuotaSnapshot>);

pub fn overview(
    accounts: Vec<AccountSummary>,
    snapshots: Vec<QuotaSnapshot>,
    settings: &Settings,
    updated_at: String,
) -> Value {
    let snapshots: BTreeMap<_, _> = snapshots
        .iter()
        .map(|snapshot| (snapshot.account_id.as_str(), snapshot))
        .collect();
    let mut groups: BTreeMap<SubscriptionProvider, Vec<Member>> = BTreeMap::new();
    // A disabled account serves no request, so it takes no bar and no part
    // in an average; the admin dashboard still shows it.
    let shown = accounts
        .iter()
        .filter(|a| a.enabled && a.provider.reports_quota());
    for account in shown {
        groups
            .entry(account.provider)
            .or_default()
            .push((account, snapshots.get(account.id.as_str()).copied()));
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|(provider, _)| rank(*provider));
    let providers: Vec<Value> = groups
        .into_iter()
        .map(|(provider, mut members)| {
            members.sort_by_cached_key(|(account, _)| {
                (label(&account.name).to_lowercase(), account.id.clone())
            });
            let display = settings.quota_display(provider);
            let mut entries: Vec<Value> = match display {
                QuotaDisplay::Accounts => members
                    .iter()
                    .map(|(account, snapshot)| account_entry(account, *snapshot))
                    .collect(),
                QuotaDisplay::Average => vec![average_entry(provider, &members)],
            };
            if let Some(choice) = settings.quota_windows(provider) {
                for entry in &mut entries {
                    arrange(&mut entry["windows"], choice);
                }
            }
            json!({"provider":provider.as_str(),"name":provider_name(provider),"display":display,"entries":entries})
        })
        .collect();
    json!({"object":"quotas","updated_at":timestamp(Some(&updated_at)),"providers":providers})
}

fn account_entry(account: &AccountSummary, snapshot: Option<&QuotaSnapshot>) -> Value {
    let status = status(account);
    json!({
        "id": account.id,
        "label": label(&account.name),
        "plan": plan(account, snapshot),
        "accounts": 1,
        "status": status,
        "error": error(account, snapshot, status),
        "updated_at": timestamp(snapshot.map(|s| s.updated_at.as_str())),
        "windows": snapshot.map(|s| s.windows.iter().map(window).collect::<Vec<_>>()).unwrap_or_default(),
        "credits": snapshot.map(|s| s.credits.iter().map(credit).collect::<Vec<_>>()).unwrap_or_default(),
    })
}

fn average_entry(provider: SubscriptionProvider, members: &[Member]) -> Value {
    let count = members.len();
    let statuses: Vec<_> = members.iter().map(|(account, _)| status(account)).collect();
    let overall = if statuses.contains(&"ready") {
        "ready"
    } else {
        statuses
            .iter()
            .copied()
            .max_by_key(|status| severity(status))
            .unwrap_or("error")
    };
    let produced = members.iter().any(|(_, snapshot)| {
        snapshot
            .is_some_and(|s| s.error.is_none() || !s.windows.is_empty() || !s.credits.is_empty())
    });
    let error = if produced {
        None
    } else {
        members
            .iter()
            .find_map(|(account, snapshot)| error(account, *snapshot, status(account)))
    };
    let plans: Vec<_> = members
        .iter()
        .map(|(account, snapshot)| plan(account, *snapshot))
        .collect();
    let plan = plans.first().cloned().flatten().filter(|first| {
        plans
            .iter()
            .all(|plan| plan.as_deref() == Some(first.as_str()))
    });
    let snapshots: Vec<_> = members
        .iter()
        .filter_map(|(_, snapshot)| *snapshot)
        .collect();
    let updated_at = snapshots
        .iter()
        .filter_map(|s| parse_timestamp(&s.updated_at).map(|time| (time, s.updated_at.as_str())))
        .min()
        .map(|(_, value)| value);
    json!({
        "id": provider.as_str(),
        "label": format!("{} · {count} {}", provider_name(provider), if count == 1 { "compte" } else { "comptes" }),
        "plan": plan,
        "accounts": count,
        "status": overall,
        "error": error,
        "updated_at": timestamp(updated_at),
        "windows": merge_windows(&snapshots),
        "credits": merge_credits(&snapshots),
    })
}

/// Windows merge by id. Accounts with a known percentage contribute to the
/// mean and the earliest reset; the label comes from the first account.
fn merge_windows(snapshots: &[&QuotaSnapshot]) -> Vec<Value> {
    struct Merge<'a> {
        first: &'a QuotaWindow,
        remaining: Vec<f64>,
        resets: Vec<&'a str>,
        other_resets: Vec<&'a str>,
    }
    let mut merged: Vec<Merge> = vec![];
    for window in snapshots.iter().flat_map(|s| &s.windows) {
        let index = match merged.iter().position(|m| m.first.id == window.id) {
            Some(index) => index,
            None => {
                merged.push(Merge {
                    first: window,
                    remaining: vec![],
                    resets: vec![],
                    other_resets: vec![],
                });
                merged.len() - 1
            }
        };
        let entry = &mut merged[index];
        let reset = window.reset_at.as_deref();
        if let Some(remaining) = percent(window.remaining_percent) {
            entry.remaining.push(remaining);
            entry.resets.extend(reset);
        } else {
            entry.other_resets.extend(reset);
        }
    }
    merged
        .into_iter()
        .map(|m| {
            let remaining = (!m.remaining.is_empty())
                .then(|| m.remaining.iter().sum::<f64>() / m.remaining.len() as f64);
            let resets = if m.remaining.is_empty() {
                m.other_resets
            } else {
                m.resets
            };
            let reset = resets
                .into_iter()
                .filter_map(|r| parse_timestamp(r).map(|time| (time, r)))
                .min()
                .map(|(_, r)| r);
            window_value(&m.first.id, &m.first.label, remaining, reset)
        })
        .collect()
}

/// Credits merge by id. A total is known only when every contributor knows it.
fn merge_credits(snapshots: &[&QuotaSnapshot]) -> Vec<Value> {
    let mut merged: Vec<(&QuotaCredits, Option<f64>, Option<f64>, Option<f64>)> = vec![];
    for credit in snapshots.iter().flat_map(|s| &s.credits) {
        let add =
            |total: Option<f64>, value: Option<f64>| total.zip(number(value)).map(|(a, b)| a + b);
        match merged.iter_mut().find(|(first, ..)| first.id == credit.id) {
            Some((_, remaining, limit, used)) => {
                *remaining = add(*remaining, credit.remaining);
                *limit = add(*limit, credit.limit);
                *used = add(*used, credit.used);
            }
            None => merged.push((
                credit,
                number(credit.remaining),
                number(credit.limit),
                number(credit.used),
            )),
        }
    }
    merged
        .into_iter()
        .map(|(first, remaining, limit, used)| {
            json!({"id":first.id,"label":first.label,"unit":first.unit,"remaining":remaining,"limit":limit,"used":used})
        })
        .collect()
}

/// Drops the provider's hidden windows and lists its primary window first,
/// flagged `primary`. The primary window stays even when it is also hidden.
fn arrange(windows: &mut Value, choice: &QuotaWindows) {
    let Some(list) = windows.as_array_mut() else {
        return;
    };
    let primary = choice.primary.as_deref();
    list.retain(|window| {
        let id = window["id"].as_str().unwrap_or_default();
        Some(id) == primary || !choice.hidden.contains(id)
    });
    if let Some(index) = list
        .iter()
        .position(|window| primary.is_some() && window["id"].as_str() == primary)
    {
        let mut first = list.remove(index);
        first["primary"] = json!(true);
        list.insert(0, first);
    }
}

fn window(window: &QuotaWindow) -> Value {
    window_value(
        &window.id,
        &window.label,
        percent(window.remaining_percent),
        window.reset_at.as_deref(),
    )
}

/// Percents go out whole: averages and client views would otherwise show
/// decimals that readings never had.
fn window_value(id: &str, label: &str, remaining: Option<f64>, reset_at: Option<&str>) -> Value {
    let remaining = remaining.map(f64::round);
    json!({"id":id,"label":label,"used_percent":remaining.map(|r| 100.0 - r),"remaining_percent":remaining,"reset_at":timestamp(reset_at)})
}

fn credit(credit: &QuotaCredits) -> Value {
    json!({"id":credit.id,"label":credit.label,"unit":credit.unit,"remaining":number(credit.remaining),"limit":number(credit.limit),"used":number(credit.used)})
}

fn status(account: &AccountSummary) -> &'static str {
    match account.status.as_str() {
        "ready" => "ready",
        "cooldown" | "exhausted" => "cooldown",
        "disabled" => "disabled",
        _ => "error",
    }
}

fn severity(status: &str) -> u8 {
    match status {
        "ready" => 0,
        "disabled" => 1,
        "cooldown" => 2,
        _ => 3,
    }
}

fn error(
    account: &AccountSummary,
    snapshot: Option<&QuotaSnapshot>,
    status: &str,
) -> Option<String> {
    snapshot.and_then(|s| s.error.clone()).or_else(|| {
        (status != "ready")
            .then(|| account.last_error.clone())
            .flatten()
    })
}

fn plan(account: &AccountSummary, snapshot: Option<&QuotaSnapshot>) -> Option<String> {
    snapshot
        .and_then(|s| s.subscription.as_ref())
        .or(account.subscription.as_ref())
        .map(|s| s.label.clone())
        .or_else(|| account.plan.clone())
}

fn percent(value: Option<f64>) -> Option<f64> {
    number(value).map(|v| v.clamp(0.0, 100.0))
}

fn number(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite())
}

fn timestamp(value: Option<&str>) -> Option<&str> {
    value.filter(|v| parse_timestamp(v).is_some())
}

/// The dashboard's account name, with each email reduced to `local@…`.
fn label(name: &str) -> String {
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || ".-".contains(c);
    let chars: Vec<char> = name.chars().collect();
    let mut output = String::with_capacity(name.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@' && index > 0 && local(chars[index - 1]) {
            let end = (index + 1..chars.len())
                .find(|&next| !domain(chars[next]))
                .unwrap_or(chars.len());
            let host: String = chars[index + 1..end].iter().collect();
            if host.trim_matches('.').contains('.') {
                output.push_str("@…");
                index = end;
                continue;
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{settings::Config, state::AppState};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn summary(id: &str, provider: SubscriptionProvider, status: &str) -> AccountSummary {
        AccountSummary {
            id: id.into(),
            name: format!("{id}@example.com"),
            provider,
            email: Some(format!("{id}@example.com")),
            plan: None,
            subscription: None,
            enabled: status != "disabled",
            status: status.into(),
            last_error: (status == "error").then(|| "provider request failed".into()),
            reserve_percent: None,
            reserve_windows: vec![],
            route: None,
            base_url: None,
            dialect: None,
            models: vec![],
            created_at: "2026-10-01T00:00:00Z".into(),
            updated_at: "2026-10-01T00:00:00Z".into(),
        }
    }

    fn snapshot(id: &str, windows: Vec<QuotaWindow>, credits: Vec<QuotaCredits>) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: id.into(),
            provider: SubscriptionProvider::Claude,
            windows,
            updated_at: "2026-10-04T11:59:00Z".into(),
            error: None,
            subscription: None,
            credits,
            verification_required: false,
            verification_available: false,
        }
    }

    fn quota_window(id: &str, remaining: Option<f64>, reset: Option<&str>) -> QuotaWindow {
        QuotaWindow {
            id: id.into(),
            label: format!("{id} label"),
            remaining_percent: remaining,
            reset_at: reset.map(Into::into),
            used: None,
            limit: None,
        }
    }

    fn quota_credit(id: &str, remaining: Option<f64>, limit: Option<f64>) -> QuotaCredits {
        QuotaCredits {
            id: id.into(),
            label: "Credits".into(),
            used: None,
            limit,
            remaining,
            unit: "credits".into(),
            reset_at: None,
            unlimited: None,
            enabled: None,
        }
    }

    fn average(provider: SubscriptionProvider) -> Settings {
        let mut settings = Settings::default();
        settings
            .quota_display
            .insert(provider.as_str().into(), QuotaDisplay::Average);
        settings
    }

    #[test]
    fn percents_go_out_whole() {
        let value = window_value("weekly", "Weekly", Some(70.4), None);
        assert_eq!(value["remaining_percent"], 70.0);
        assert_eq!(value["used_percent"], 30.0);
        assert_eq!(
            window_value("weekly", "Weekly", Some(99.6), None)["remaining_percent"],
            100.0
        );
    }

    #[test]
    fn providers_without_a_quota_reader_stay_out() {
        let accounts = vec![
            summary("claude", SubscriptionProvider::Claude, "ready"),
            summary(
                "openrouter",
                SubscriptionProvider::OpenaiCompatible,
                "ready",
            ),
            summary("qwen", SubscriptionProvider::Qwen, "ready"),
        ];
        let mut unsupported = snapshot("openrouter", vec![], vec![]);
        unsupported.provider = SubscriptionProvider::OpenaiCompatible;
        unsupported.error = Some("provider does not support this operation".into());
        let value = overview(
            accounts,
            vec![unsupported],
            &Settings::default(),
            "2026-10-04T12:00:00Z".into(),
        );
        let providers: Vec<&str> = value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["provider"].as_str().unwrap())
            .collect();
        assert_eq!(providers, ["claude"]);
    }

    #[test]
    fn disabled_accounts_take_no_bar() {
        let accounts = vec![
            summary("kept", SubscriptionProvider::Claude, "ready"),
            summary("parked", SubscriptionProvider::Claude, "disabled"),
            summary("refunded", SubscriptionProvider::Codex, "disabled"),
        ];
        let mut failed = snapshot("parked", vec![], vec![]);
        failed.error = Some("provider is rate-limiting quota reads (HTTP 429)".into());
        let value = overview(
            accounts,
            vec![snapshot("kept", vec![], vec![]), failed],
            &Settings::default(),
            "2026-10-04T12:00:00Z".into(),
        );
        // A provider whose accounts are all disabled leaves the list.
        let providers = value["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        let entries = providers[0]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "kept");
        assert!(!value.to_string().contains("HTTP 429"));
    }

    #[test]
    fn provider_windows_hide_and_lead_with_the_primary() {
        let accounts = vec![
            summary("a", SubscriptionProvider::Claude, "ready"),
            summary("b", SubscriptionProvider::Claude, "ready"),
            summary("codex", SubscriptionProvider::Codex, "ready"),
        ];
        let windows = || {
            vec![
                quota_window("five-hour", Some(60.0), None),
                quota_window("seven-day", Some(70.0), None),
                quota_window("seven-day-opus", Some(90.0), None),
            ]
        };
        let snapshots = vec![
            snapshot("a", windows(), vec![]),
            snapshot("b", windows(), vec![]),
            snapshot("codex", windows(), vec![]),
        ];
        let ids = |entry: &Value| -> Vec<String> {
            entry["windows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let mut settings = Settings::default();
        settings.quota_windows.insert(
            "claude".into(),
            QuotaWindows {
                hidden: ["seven-day-opus".into(), "five-hour".into()].into(),
                primary: Some("five-hour".into()),
            },
        );
        let value = overview(
            accounts.clone(),
            snapshots.clone(),
            &settings,
            "2026-10-04T12:00:00Z".into(),
        );
        let claude = &value["providers"][0]["entries"][0];
        // The primary stays although hidden, and leads.
        assert_eq!(ids(claude), ["five-hour", "seven-day"]);
        assert_eq!(claude["windows"][0]["primary"], true);
        assert_eq!(claude["windows"][1].get("primary"), None);
        // Another provider keeps every window, none flagged.
        let codex = &value["providers"][1]["entries"][0];
        assert_eq!(ids(codex), ["five-hour", "seven-day", "seven-day-opus"]);
        assert!(!value["providers"][1].to_string().contains("primary"));

        // The same choice applies to the averaged entry.
        settings
            .quota_display
            .insert("claude".into(), QuotaDisplay::Average);
        settings.quota_windows.insert(
            "claude".into(),
            QuotaWindows {
                hidden: ["five-hour".into()].into(),
                primary: Some("seven-day".into()),
            },
        );
        let value = overview(
            accounts,
            snapshots,
            &settings,
            "2026-10-04T12:00:00Z".into(),
        );
        let average = &value["providers"][0]["entries"][0];
        assert_eq!(ids(average), ["seven-day", "seven-day-opus"]);
        assert_eq!(average["windows"][0]["primary"], true);
    }

    #[test]
    fn a_primary_the_reading_lacks_flags_nothing() {
        let mut settings = Settings::default();
        settings.quota_windows.insert(
            "claude".into(),
            QuotaWindows {
                hidden: Default::default(),
                primary: Some("monthly".into()),
            },
        );
        let value = overview(
            vec![summary("a", SubscriptionProvider::Claude, "ready")],
            vec![snapshot(
                "a",
                vec![
                    quota_window("five-hour", Some(60.0), None),
                    quota_window("seven-day", Some(70.0), None),
                ],
                vec![],
            )],
            &settings,
            "2026-10-04T12:00:00Z".into(),
        );
        let entry = &value["providers"][0]["entries"][0];
        assert_eq!(entry["windows"].as_array().unwrap().len(), 2);
        assert!(!entry.to_string().contains("primary"));
    }

    #[test]
    fn labels_keep_names_and_mask_email_domains() {
        assert_eq!(label("chris@example.com"), "chris@…");
        assert_eq!(
            label("Travail (a.b+c@mail.example.org)"),
            "Travail (a.b+c@…)"
        );
        assert_eq!(label("Max @ home"), "Max @ home");
        assert_eq!(label("Équipe"), "Équipe");
    }

    #[test]
    fn providers_are_ordered_and_default_to_one_entry_per_account() {
        let accounts = vec![
            summary("kimi", SubscriptionProvider::Kimi, "ready"),
            summary("zeta", SubscriptionProvider::Claude, "ready"),
            summary("google", SubscriptionProvider::Antigravity, "ready"),
            summary("codex", SubscriptionProvider::Codex, "cooldown"),
            summary("alpha", SubscriptionProvider::Claude, "exhausted"),
            summary("grok", SubscriptionProvider::Xai, "expired"),
        ];
        let value = overview(
            accounts,
            vec![],
            &Settings::default(),
            "2026-10-04T12:00:00Z".into(),
        );
        let ids: Vec<_> = value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["provider"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["claude", "codex", "antigravity", "xai", "kimi"]);
        let claude = &value["providers"][0];
        assert_eq!(claude["name"], "Claude");
        assert_eq!(claude["display"], "accounts");
        assert_eq!(claude["entries"][0]["id"], "alpha");
        assert_eq!(claude["entries"][0]["label"], "alpha@…");
        assert_eq!(claude["entries"][0]["status"], "cooldown");
        assert_eq!(claude["entries"][0]["accounts"], 1);
        assert_eq!(claude["entries"][0]["updated_at"], Value::Null);
        assert_eq!(claude["entries"][0]["plan"], Value::Null);
        assert_eq!(claude["entries"][0]["windows"], json!([]));
        assert_eq!(value["providers"][3]["name"], "Grok");
        assert_eq!(value["providers"][3]["entries"][0]["status"], "error");
        assert!(!value.to_string().contains("example.com"));
    }

    #[test]
    fn average_merges_windows_and_credits_across_accounts() {
        let accounts = vec![
            summary("a", SubscriptionProvider::Claude, "ready"),
            summary("b", SubscriptionProvider::Claude, "cooldown"),
            summary("c", SubscriptionProvider::Claude, "error"),
            summary("d", SubscriptionProvider::Claude, "disabled"),
        ];
        let snapshots = vec![
            snapshot(
                "a",
                vec![
                    quota_window("five-hour", Some(80.0), Some("2026-10-04T15:00:00Z")),
                    quota_window("weekly", None, Some("2026-10-05T00:00:00Z")),
                ],
                vec![quota_credit("codex-credits", Some(10.0), None)],
            ),
            snapshot(
                "b",
                vec![quota_window(
                    "five-hour",
                    Some(40.0),
                    Some("2026-10-04T13:00:00Z"),
                )],
                vec![quota_credit("codex-credits", Some(5.5), Some(100.0))],
            ),
            snapshot(
                "c",
                vec![
                    quota_window("five-hour", None, Some("2026-10-04T12:30:00Z")),
                    quota_window("weekly", None, Some("2026-10-06T00:00:00Z")),
                ],
                vec![],
            ),
            // A disabled account counts nowhere: not in the mean, the
            // earliest reset, the credits nor the account count.
            snapshot(
                "d",
                vec![quota_window(
                    "five-hour",
                    Some(0.0),
                    Some("2026-10-04T12:10:00Z"),
                )],
                vec![quota_credit("codex-credits", Some(100.0), None)],
            ),
        ];
        let value = overview(
            accounts,
            snapshots,
            &average(SubscriptionProvider::Claude),
            "2026-10-04T12:00:00Z".into(),
        );
        let provider = &value["providers"][0];
        assert_eq!(provider["display"], "average");
        assert_eq!(provider["entries"].as_array().unwrap().len(), 1);
        let entry = &provider["entries"][0];
        assert_eq!(entry["id"], "claude");
        assert_eq!(entry["label"], "Claude · 3 comptes");
        assert_eq!(entry["accounts"], 3);
        assert_eq!(entry["status"], "ready");
        assert_eq!(entry["error"], Value::Null);
        assert_eq!(
            entry["windows"],
            json!([
                {"id":"five-hour","label":"five-hour label","used_percent":40.0,"remaining_percent":60.0,"reset_at":"2026-10-04T13:00:00Z"},
                {"id":"weekly","label":"weekly label","used_percent":null,"remaining_percent":null,"reset_at":"2026-10-05T00:00:00Z"}
            ])
        );
        assert_eq!(
            entry["credits"],
            json!([{"id":"codex-credits","label":"Credits","unit":"credits","remaining":15.5,"limit":null,"used":null}])
        );
    }

    #[test]
    fn average_status_and_error_follow_the_worst_account_without_snapshots() {
        let accounts = vec![
            summary("a", SubscriptionProvider::Codex, "cooldown"),
            summary("b", SubscriptionProvider::Codex, "error"),
            summary("c", SubscriptionProvider::Codex, "disabled"),
        ];
        let value = overview(
            accounts.clone(),
            vec![],
            &average(SubscriptionProvider::Codex),
            "2026-10-04T12:00:00Z".into(),
        );
        let entry = &value["providers"][0]["entries"][0];
        assert_eq!(entry["status"], "error");
        assert_eq!(entry["error"], "provider request failed");
        assert_eq!(entry["label"], "Codex · 2 comptes");
        assert_eq!(entry["updated_at"], Value::Null);

        let mut failed = snapshot("a", vec![], vec![]);
        failed.error = Some("upstream authentication or quota request failed (HTTP 401)".into());
        let value = overview(
            accounts[..1].to_vec(),
            vec![failed.clone()],
            &average(SubscriptionProvider::Codex),
            "2026-10-04T12:00:00Z".into(),
        );
        let entry = &value["providers"][0]["entries"][0];
        assert_eq!(entry["label"], "Codex · 1 compte");
        assert_eq!(entry["status"], "cooldown");
        assert_eq!(
            entry["error"],
            "upstream authentication or quota request failed (HTTP 401)"
        );

        let mut observed = snapshot("b", vec![], vec![]);
        observed.updated_at = "2026-10-04T11:58:00Z".into();
        let value = overview(
            accounts[..2].to_vec(),
            vec![failed, observed],
            &average(SubscriptionProvider::Codex),
            "2026-10-04T12:00:00Z".into(),
        );
        let entry = &value["providers"][0]["entries"][0];
        assert_eq!(entry["error"], Value::Null);
        assert_eq!(entry["updated_at"], "2026-10-04T11:58:00Z");
    }

    #[tokio::test]
    async fn route_serves_snapshots_per_account_and_averaged_without_secrets() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for (name, provider) in [
            ("first@example.com", "claude"),
            ("Second", "claude"),
            ("Codex", "codex"),
        ] {
            let account = state
                .accounts
                .import(
                    json!({"type":provider,"access_token":"fixture-access","refresh_token":"fixture-refresh","name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        state
            .quotas
            .insert_for_test(snapshot(
                &ids[0],
                vec![quota_window(
                    "five-hour",
                    Some(89.0),
                    Some("2026-10-04T13:49:59Z"),
                )],
                vec![quota_credit("extra", None, Some(50.0))],
            ))
            .await;
        state
            .quotas
            .insert_for_test(snapshot(
                &ids[1],
                vec![quota_window(
                    "five-hour",
                    Some(51.0),
                    Some("2026-10-04T13:00:00Z"),
                )],
                vec![quota_credit("extra", Some(2.0), Some(25.0))],
            ))
            .await;
        let (_, key) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("tests".into()),
                ..Default::default()
            })
            .unwrap();
        // Copied into each request the closure below builds.
        let bearer: &'static str = Box::leak(format!("Bearer {key}").into_boxed_str());
        let get = |state: std::sync::Arc<AppState>| async move {
            let response = crate::router(state)
                .oneshot(
                    Request::builder()
                        .uri("/v1/quotas")
                        .header("authorization", bearer)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let text = String::from_utf8(body.to_vec()).unwrap();
            assert!(!text.contains("fixture-"));
            assert!(!text.contains("example.com"));
            serde_json::from_str::<Value>(&text).unwrap()
        };

        let value = get(state.clone()).await;
        assert_eq!(value["object"], "quotas");
        assert!(parse_timestamp(value["updated_at"].as_str().unwrap()).is_some());
        let claude = &value["providers"][0];
        assert_eq!(claude["display"], "accounts");
        assert_eq!(claude["entries"][0]["id"], ids[0].as_str());
        assert_eq!(claude["entries"][0]["label"], "first@…");
        assert_eq!(claude["entries"][0]["status"], "ready");
        assert_eq!(
            claude["entries"][0]["windows"][0],
            json!({"id":"five-hour","label":"five-hour label","used_percent":11.0,"remaining_percent":89.0,"reset_at":"2026-10-04T13:49:59Z"})
        );
        assert_eq!(claude["entries"][1]["label"], "Second");
        assert_eq!(value["providers"][1]["entries"][0]["windows"], json!([]));

        state.config.write().await.settings = average(SubscriptionProvider::Claude);
        let value = get(state.clone()).await;
        let claude = &value["providers"][0];
        assert_eq!(claude["display"], "average");
        assert_eq!(claude["entries"][0]["label"], "Claude · 2 comptes");
        assert_eq!(
            claude["entries"][0]["windows"][0]["remaining_percent"],
            70.0
        );
        assert_eq!(claude["entries"][0]["windows"][0]["used_percent"], 30.0);
        assert_eq!(
            claude["entries"][0]["windows"][0]["reset_at"],
            "2026-10-04T13:00:00Z"
        );
        assert_eq!(claude["entries"][0]["credits"][0]["remaining"], Value::Null);
        assert_eq!(claude["entries"][0]["credits"][0]["limit"], 75.0);
        assert_eq!(value["providers"][1]["display"], "accounts");
    }
}
