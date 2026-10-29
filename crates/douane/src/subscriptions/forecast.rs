//! Estimates from observed quota changes. Never used for routing or quota values.
use super::{QuotaSnapshot, QuotaWindow, parse_timestamp, timestamp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const LOOKBACK: i64 = 24 * 3600;
const MIN_SPAN: i64 = 30 * 60;
const MAX_GAP: i64 = 30 * 60;
const SAMPLE_INTERVAL: i64 = 5 * 60;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct History(BTreeMap<String, BTreeMap<String, BTreeMap<Source, Series>>>);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Usage,
    ClaudeHeaders,
}

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
    #[cfg(test)]
    pub fn observe(&mut self, snapshots: &[QuotaSnapshot], now: i64) {
        self.retain(snapshots);
        for quota in snapshots {
            self.record(quota, now);
        }
    }

    pub fn retain(&mut self, snapshots: &[QuotaSnapshot]) {
        self.0.retain(|id, windows| {
            let Some(quota) = snapshots.iter().find(|q| &q.account_id == id) else {
                return false;
            };
            windows.retain(|id, _| quota.windows.iter().any(|w| &w.id == id));
            true
        });
    }

    pub fn record(&mut self, quota: &QuotaSnapshot, now: i64) {
        self.record_from(quota, &quota.windows, now, Source::Usage);
    }

    pub fn record_claude_headers(
        &mut self,
        quota: &QuotaSnapshot,
        windows: &[QuotaWindow],
        now: i64,
    ) {
        self.record_from(quota, windows, now, Source::ClaudeHeaders);
    }

    fn record_from(
        &mut self,
        quota: &QuotaSnapshot,
        observed_windows: &[QuotaWindow],
        now: i64,
        source: Source,
    ) {
        let Some(at) = parse_timestamp(&quota.updated_at).map(|t| t.unix_timestamp()) else {
            return;
        };
        if quota.error.is_some() || quota.verification_required || at > now || now - at > MAX_GAP {
            return;
        }
        let windows = self.0.entry(quota.account_id.clone()).or_default();
        for window in observed_windows {
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
            let sources = windows.entry(window.id.clone()).or_default();
            let last = sources
                .get(&source)
                .and_then(Series::last)
                .map(|p| (p.at, p.remaining));
            if sources.values().filter_map(Series::last).any(|p| at < p.at) {
                continue;
            }
            // Compare refills within one source: Claude headers and usage have
            // different precision. A confirmed refill invalidates both histories.
            if sources.values().any(|s| (s.reset - reset).abs() > 60)
                || last.is_some_and(|(_, previous)| remaining > previous)
            {
                sources.clear();
            }
            let series = sources.entry(source).or_insert_with(|| Series {
                reset,
                points: Vec::new(),
                last_observed: None,
            });
            if last.is_some_and(|(previous_at, _)| at - previous_at > MAX_GAP) {
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
                let Some(sources) = self
                    .0
                    .get(&quota.account_id)
                    .and_then(|s| s.get(&window.id))
                else {
                    continue;
                };
                if window
                    .remaining_percent
                    .is_none_or(|p| !p.is_finite() || !(0.0..=100.0).contains(&p))
                {
                    continue;
                }
                let Some(reset) = window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .map(|t| t.unix_timestamp())
                    .filter(|r| *r > now)
                else {
                    continue;
                };
                if let Some((_, forecast)) = sources
                    .values()
                    .filter_map(|s| s.forecast(&quota.account_id, &window.id, reset, now))
                    .max_by_key(|(at, _)| *at)
                {
                    forecasts.push(forecast);
                }
            }
        }
        forecasts
    }
}

impl Series {
    fn last(&self) -> Option<&Point> {
        self.last_observed.as_ref().or(self.points.last())
    }

    fn forecast(
        &self,
        account_id: &str,
        window_id: &str,
        reset: i64,
        now: i64,
    ) -> Option<(i64, WindowForecast)> {
        let last = self.last()?;
        let first = self.points.iter().find(|p| p.at >= now - LOOKBACK)?;
        let span = last.at - first.at;
        if last.at > now
            || now - last.at > MAX_GAP
            || (reset - self.reset).abs() > 60
            || span < MIN_SPAN
            || last.remaining > first.remaining
        {
            return None;
        }
        let rate = (first.remaining - last.remaining) / span as f64;
        // Empty quotas censor demand: the observed slope cannot predict refill needs.
        let exhausts = if last.remaining == 0.0 {
            Some(last.at)
        } else if rate > 0.0 {
            let seconds = last.remaining / rate;
            (seconds < (reset - last.at) as f64).then(|| last.at + seconds.ceil() as i64)
        } else {
            None
        };
        Some((
            last.at,
            WindowForecast {
                account_id: account_id.into(),
                window_id: window_id.into(),
                observed_seconds: span,
                rate_per_hour: rate * 3600.0,
                remaining_at_reset: (last.remaining - rate * (reset - last.at) as f64).max(0.0),
                exhausts_at: exhausts
                    .and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok())
                    .map(timestamp),
            },
        ))
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
    fn source_precision_stays_separate_and_small_refills_invalidate_both() {
        let mut history = History::default();
        let usage = snapshot(NOW - 1800, 80.0, NOW + 7200);
        history.record(&usage, NOW - 1800);
        let headers = snapshot(NOW - 1800, 79.6, NOW + 7200);
        history.record_claude_headers(&headers, &headers.windows, NOW - 1800);
        let usage = snapshot(NOW, 47.0, NOW + 7200);
        history.record(&usage, NOW);
        let headers = snapshot(NOW, 46.6, NOW + 7200);
        history.record_claude_headers(&headers, &headers.windows, NOW);
        let mut restored: History =
            serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
        let usage = snapshot(NOW + 60, 47.0, NOW + 7200);
        restored.record(&usage, NOW + 60);
        assert_eq!(restored.report(&[usage], NOW + 60).len(), 1);
        for (at, remaining) in [(NOW + 120, 46.5), (NOW + 180, 46.6)] {
            let headers = snapshot(at, remaining, NOW + 7200);
            restored.record_claude_headers(&headers, &headers.windows, at);
            assert_eq!(
                restored.report(&[headers], at).len(),
                usize::from(at == NOW + 120)
            );
        }
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
