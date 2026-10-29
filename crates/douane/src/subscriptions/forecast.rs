//! Estimates from observed quota changes. Never used for routing or quota values.
use super::{QuotaSnapshot, parse_timestamp, timestamp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const LOOKBACK: i64 = 24 * 3600;
const MIN_SPAN: i64 = 30 * 60;
const MAX_GAP: i64 = 30 * 60;
const SAMPLE_INTERVAL: i64 = 5 * 60;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct History(BTreeMap<String, BTreeMap<String, Series>>);

#[derive(Serialize, Deserialize)]
struct Series {
    reset: i64,
    points: Vec<Point>,
    #[serde(default)]
    last_observed: Option<Point>,
}

#[derive(Serialize, Deserialize)]
struct Point {
    at: i64,
    remaining: f64,
}

#[derive(Serialize)]
pub struct WindowForecast {
    pub account_id: String,
    pub window_id: String,
    pub observed_seconds: i64,
    /// Percentage points consumed per hour, not tokens or money.
    pub rate_per_hour: f64,
    pub remaining_at_reset: f64,
    pub exhausts_at: Option<String>,
}

impl History {
    pub fn observe(&mut self, snapshots: &[QuotaSnapshot], now: i64) {
        self.0
            .retain(|id, _| snapshots.iter().any(|q| &q.account_id == id));
        for quota in snapshots {
            self.record(quota, now);
        }
    }

    pub fn record(&mut self, quota: &QuotaSnapshot, now: i64) {
        let Some(at) = parse_timestamp(&quota.updated_at).map(|t| t.unix_timestamp()) else {
            return;
        };
        if quota.error.is_some() || quota.verification_required || at > now || now - at > MAX_GAP {
            return;
        }
        let windows = self.0.entry(quota.account_id.clone()).or_default();
        windows.retain(|id, _| quota.windows.iter().any(|w| &w.id == id));
        for window in &quota.windows {
            let Some(remaining) = window
                .remaining_percent
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
            else {
                continue;
            };
            let Some(reset) = window
                .reset_at
                .as_deref()
                .and_then(parse_timestamp)
                .map(|t| t.unix_timestamp())
                .filter(|r| *r > now)
            else {
                continue;
            };
            let series = windows.entry(window.id.clone()).or_insert_with(|| Series {
                reset,
                points: Vec::new(),
                last_observed: None,
            });
            let last = series.last_observed.as_ref().or(series.points.last());
            if last.is_some_and(|p| at < p.at) {
                continue;
            }
            // New reset, refill, or an observation gap: never bridge cycles or downtime.
            if (series.reset - reset).abs() > 60
                || last.is_some_and(|p| at - p.at > MAX_GAP || remaining > p.remaining)
            {
                series.points.clear();
            }
            series.reset = reset;
            series.last_observed = Some(Point { at, remaining });
            series
                .points
                .retain(|p| p.at >= now - LOOKBACK && p.at <= at);
            if series
                .points
                .last()
                .is_none_or(|p| at - p.at >= SAMPLE_INTERVAL)
            {
                series.points.push(Point { at, remaining });
            }
        }
    }

    pub fn report(&self, snapshots: &[QuotaSnapshot], now: i64) -> Vec<WindowForecast> {
        let mut forecasts = Vec::new();
        for quota in snapshots {
            let Some(at) = parse_timestamp(&quota.updated_at).map(|t| t.unix_timestamp()) else {
                continue;
            };
            if quota.error.is_some()
                || quota.verification_required
                || at > now
                || now - at > MAX_GAP
            {
                continue;
            }
            for window in &quota.windows {
                let Some(series) = self
                    .0
                    .get(&quota.account_id)
                    .and_then(|s| s.get(&window.id))
                else {
                    continue;
                };
                let Some(first) = series.points.iter().find(|p| p.at >= now - LOOKBACK) else {
                    continue;
                };
                let Some(remaining) = window.remaining_percent.filter(|p| {
                    p.is_finite() && (0.0..=100.0).contains(p) && *p <= first.remaining
                }) else {
                    continue;
                };
                let Some(reset) = window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .map(|t| t.unix_timestamp())
                    .filter(|r| *r > now && (*r - series.reset).abs() <= 60)
                else {
                    continue;
                };
                let span = at - first.at;
                if span < MIN_SPAN {
                    continue;
                }
                let rate = (first.remaining - remaining) / span as f64;
                // Empty quotas censor demand: the observed slope cannot predict refill needs.
                let exhausts = if remaining == 0.0 {
                    Some(at)
                } else if rate > 0.0 {
                    let seconds = remaining / rate;
                    (seconds < (reset - at) as f64).then(|| at + seconds.ceil() as i64)
                } else {
                    None
                };
                forecasts.push(WindowForecast {
                    account_id: quota.account_id.clone(),
                    window_id: window.id.clone(),
                    observed_seconds: span,
                    rate_per_hour: rate * 3600.0,
                    remaining_at_reset: (remaining - rate * (reset - at) as f64).max(0.0),
                    exhausts_at: exhausts
                        .and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok())
                        .map(timestamp),
                });
            }
        }
        forecasts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscriptions::{QuotaWindow, SubscriptionProvider};
    const NOW: i64 = 1_900_000_000;
    fn snapshot(at: i64, remaining: f64, reset: i64) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: "a".into(),
            provider: SubscriptionProvider::Antigravity,
            windows: vec![QuotaWindow {
                id: "model-specific".into(),
                label: "Model".into(),
                remaining_percent: Some(remaining),
                reset_at: Some(timestamp(
                    time::OffsetDateTime::from_unix_timestamp(reset).unwrap(),
                )),
                used: None,
                limit: None,
            }],
            updated_at: timestamp(time::OffsetDateTime::from_unix_timestamp(at).unwrap()),
            error: None,
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        }
    }
    fn tracked(remaining: f64) -> (History, Vec<QuotaSnapshot>) {
        let mut history = History::default();
        history.observe(&[snapshot(NOW - 1800, 80.0, NOW + 7200)], NOW - 1800);
        let quotas = vec![snapshot(NOW, remaining, NOW + 7200)];
        history.observe(&quotas, NOW);
        (history, quotas)
    }
    #[test]
    fn projects_real_changes_without_assuming_a_window_duration() {
        let (history, quotas) = tracked(60.0);
        let report = history.report(&quotas, NOW);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].rate_per_hour, 40.0);
        assert_eq!(report[0].remaining_at_reset, 0.0);
        assert_eq!(
            parse_timestamp(report[0].exhausts_at.as_ref().unwrap())
                .unwrap()
                .unix_timestamp(),
            NOW + 5400
        );
        let (history, quotas) = tracked(75.0);
        assert_eq!(history.report(&quotas, NOW)[0].remaining_at_reset, 55.0);
    }
    #[test]
    fn idle_quotas_leave_margin_and_history_survives_serialization() {
        let (history, quotas) = tracked(80.0);
        let restored: History =
            serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
        let report = restored.report(&quotas, NOW);
        assert_eq!(report[0].rate_per_hour, 0.0);
        assert_eq!(report[0].remaining_at_reset, 80.0);
        assert!(report[0].exhausts_at.is_none());
    }
    #[test]
    fn a_refill_between_sample_points_invalidates_the_old_pace() {
        let (mut history, _) = tracked(60.0);
        let quotas = [snapshot(NOW + 60, 40.0, NOW + 7200)];
        history.observe(&quotas, NOW + 60);
        assert!(!history.report(&quotas, NOW + 60).is_empty());
        let quotas = [snapshot(NOW + 120, 50.0, NOW + 7200)];
        history.observe(&quotas, NOW + 120);
        assert!(history.report(&quotas, NOW + 120).is_empty());
    }
    #[test]
    fn resets_refills_stale_readings_and_short_histories_stay_unknown() {
        let (mut history, mut quotas) = tracked(60.0);
        assert!(history.report(&quotas, NOW + MAX_GAP + 1).is_empty());
        quotas[0] = snapshot(NOW + 300, 90.0, NOW + 7200);
        history.observe(&quotas, NOW + 300);
        assert!(history.report(&quotas, NOW + 300).is_empty());
        quotas[0] = snapshot(NOW + 600, 80.0, NOW + 15000);
        history.observe(&quotas, NOW + 600);
        assert!(history.report(&quotas, NOW + 600).is_empty());
    }
    #[test]
    fn gaps_missing_numbers_and_expired_resets_never_invent_a_rate() {
        let (mut history, mut quotas) = tracked(60.0);
        quotas[0] = snapshot(NOW + MAX_GAP + 1, 40.0, NOW + 7200);
        history.observe(&quotas, NOW + MAX_GAP + 1);
        assert!(history.report(&quotas, NOW + MAX_GAP + 1).is_empty());
        quotas[0].windows[0].remaining_percent = None;
        assert!(history.report(&quotas, NOW).is_empty());
        let (history, mut quotas) = tracked(60.0);
        quotas[0].windows[0].reset_at = Some(timestamp(
            time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        ));
        assert!(history.report(&quotas, NOW).is_empty());
    }
}
