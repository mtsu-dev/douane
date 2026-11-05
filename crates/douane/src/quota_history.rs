//! Quota readings kept over time for the dashboard's pool chart. The poller
//! offers every current snapshot each pass; a window is stored when its value
//! moves, its reset changes, or an hour went by without a stored reading.

use crate::{
    ledger::QuotaSample,
    subscriptions::{QuotaSnapshot, parse_timestamp},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Smallest move of the remaining percent worth a reading.
const STEP: f64 = 0.5;
/// A reset that moves by less is the same reset read with jitter.
const RESET_JITTER: i64 = 120;
/// A steady window still gets one reading an hour, so a flat line is known flat.
const HEARTBEAT: i64 = 3600;
/// How long readings are kept, and the widest range a request may ask for.
pub const RETENTION_DAYS: i64 = 45;

#[derive(Default)]
pub struct Sampler {
    last: std::sync::Mutex<BTreeMap<(String, String), QuotaSample>>,
}

impl Sampler {
    /// The readings among `snapshots` worth storing; remembers them as stored.
    pub fn due(&self, snapshots: &[QuotaSnapshot]) -> Vec<QuotaSample> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let mut due = Vec::new();
        for snapshot in snapshots {
            // An account whose reads fail for good keeps showing stale figures.
            if snapshot.error.is_some() {
                continue;
            }
            let Some(at) = parse_timestamp(&snapshot.updated_at).map(|t| t.unix_timestamp()) else {
                continue;
            };
            for window in &snapshot.windows {
                let Some(remaining) = window
                    .remaining_percent
                    .filter(|value| value.is_finite())
                    .map(|value| value.clamp(0.0, 100.0))
                else {
                    continue;
                };
                let reset_at = window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .map(|t| t.unix_timestamp());
                let key = (snapshot.account_id.clone(), window.id.clone());
                let store = match last.get(&key) {
                    None => true,
                    Some(previous) if at <= previous.at => false,
                    Some(previous) => {
                        (remaining - previous.remaining).abs() >= STEP
                            || match (reset_at, previous.reset_at) {
                                (Some(a), Some(b)) => (a - b).abs() > RESET_JITTER,
                                (a, b) => a.is_some() != b.is_some(),
                            }
                            || at - previous.at >= HEARTBEAT
                    }
                };
                if store {
                    let sample = QuotaSample {
                        account_id: key.0.clone(),
                        window_id: key.1.clone(),
                        at,
                        remaining,
                        reset_at,
                    };
                    last.insert(key, sample.clone());
                    due.push(sample);
                }
            }
        }
        due
    }
}

/// Groups ordered readings into one series per account window, points as
/// `[at, remaining, reset_at]` to keep two weeks of history small.
pub fn series(samples: Vec<QuotaSample>) -> Value {
    let mut series: Vec<Value> = Vec::new();
    let mut current: Option<(String, String, Vec<Value>)> = None;
    for s in samples {
        let same = current
            .as_ref()
            .is_some_and(|(account, window, _)| *account == s.account_id && *window == s.window_id);
        if !same {
            if let Some((account, window, points)) = current.take() {
                series.push(json!({"account_id": account, "window_id": window, "points": points}));
            }
            current = Some((s.account_id.clone(), s.window_id.clone(), Vec::new()));
        }
        if let Some((_, _, points)) = current.as_mut() {
            points.push(json!([
                s.at,
                (s.remaining * 10.0).round() / 10.0,
                s.reset_at
            ]));
        }
    }
    if let Some((account, window, points)) = current {
        series.push(json!({"account_id": account, "window_id": window, "points": points}));
    }
    json!({ "series": series })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscriptions::{QuotaWindow, SubscriptionProvider};

    fn snapshot(at: &str, remaining: f64, reset: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: "a".into(),
            provider: SubscriptionProvider::Claude,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                label: "Weekly".into(),
                remaining_percent: Some(remaining),
                reset_at: Some(reset.into()),
                used: None,
                limit: None,
            }],
            updated_at: at.into(),
            error: None,
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        }
    }

    #[test]
    fn a_window_is_stored_when_it_moves_resets_or_an_hour_passed() {
        let sampler = Sampler::default();
        let reset = "2026-11-08T03:00:00Z";
        let stored = |s: &QuotaSnapshot| sampler.due(std::slice::from_ref(s)).len();
        assert_eq!(stored(&snapshot("2026-11-05T10:00:00Z", 80.0, reset)), 1);
        // Same reading offered again, then a move under half a point.
        assert_eq!(stored(&snapshot("2026-11-05T10:00:00Z", 80.0, reset)), 0);
        assert_eq!(stored(&snapshot("2026-11-05T10:05:00Z", 79.7, reset)), 0);
        assert_eq!(stored(&snapshot("2026-11-05T10:06:00Z", 79.4, reset)), 1);
        // Reset jitter of a few seconds is the same reset; a new one is stored.
        assert_eq!(
            stored(&snapshot(
                "2026-11-05T10:07:00Z",
                79.4,
                "2026-11-08T03:00:30Z"
            )),
            0
        );
        assert_eq!(
            stored(&snapshot(
                "2026-11-05T10:08:00Z",
                79.4,
                "2026-11-15T03:00:00Z"
            )),
            1
        );
        // A flat window still gets its hourly reading.
        assert_eq!(
            stored(&snapshot(
                "2026-11-05T11:07:59Z",
                79.4,
                "2026-11-15T03:00:00Z"
            )),
            0
        );
        assert_eq!(
            stored(&snapshot(
                "2026-11-05T11:08:00Z",
                79.4,
                "2026-11-15T03:00:00Z"
            )),
            1
        );
        // Stale figures of an account in error are not history.
        let mut failing = snapshot("2026-11-05T12:30:00Z", 10.0, reset);
        failing.error = Some("revoked".into());
        assert_eq!(stored(&failing), 0);
    }

    #[test]
    fn series_groups_points_by_account_window() {
        let sample = |account: &str, at| QuotaSample {
            account_id: account.into(),
            window_id: "weekly".into(),
            at,
            remaining: 41.26,
            reset_at: None,
        };
        let value = series(vec![sample("a", 1), sample("a", 2), sample("b", 3)]);
        assert_eq!(
            value,
            json!({"series":[
                {"account_id":"a","window_id":"weekly","points":[[1,41.3,null],[2,41.3,null]]},
                {"account_id":"b","window_id":"weekly","points":[[3,41.3,null]]}
            ]})
        );
    }
}
