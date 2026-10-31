import type { Account, Quota, QuotaWindow } from './management';

const HOUR = 3600_000;
const LENGTHS: Record<string, number> = { 'five-hour': 5 * HOUR, daily: 24 * HOUR, weekly: 168 * HOUR, 'seven-day': 168 * HOUR, seven_day: 168 * HOUR };

/** A month ending at reset started the same day of the previous month (clamped to its last day). */
function monthLength(reset: number) {
  const end = new Date(reset);
  const year = end.getUTCFullYear(), month = end.getUTCMonth() - 1;
  const lastDay = new Date(Date.UTC(year, month + 1, 0)).getUTCDate();
  return reset - Date.UTC(year, month, Math.min(end.getUTCDate(), lastDay), end.getUTCHours(), end.getUTCMinutes(), end.getUTCSeconds());
}

const isMonthly = (window: QuotaWindow) => window.id === 'monthly' || (!LENGTHS[window.id] && /month/i.test(window.label));

/** Window length in ms, from its id or, for provider buckets, its label. */
function windowLength(window: QuotaWindow, reset: number) {
  if (isMonthly(window)) return reset > 0 ? monthLength(reset) : null;
  const known = LENGTHS[window.id] ?? Number(/^window-(\d+)s$/.exec(window.id)?.[1]) * 1000;
  if (known) return known;
  if (/week/i.test(window.label)) return LENGTHS.weekly;
  if (/five.?hour|\b5\s?h\b/i.test(window.label)) return LENGTHS['five-hour'];
  return null;
}

/** Nominal size used to pick the longest window: calendar months differ by days, yet stay one class. */
function windowRank(window: QuotaWindow) {
  if (isMonthly(window)) return 30 * 24 * HOUR;
  return windowLength(window, 0) ?? 0;
}

/**
 * How fast a provider's subscriptions are used: quota used so far against the
 * share of each window already elapsed. 100% spends the quota exactly at reset.
 * Each account counts only its longest windows (monthly over weekly over 5h),
 * every bucket of that length included, weighted by how far it has run, so a
 * window that just started weighs little. A longest window that says nothing
 * yet keeps the account out rather than falling back to a shorter one.
 */
export function providerPace(accounts: Account[], quotas: Quota[]) {
  const enabled = accounts.filter(a => a.enabled);
  let used = 0, expected = 0, known = 0;
  for (const account of enabled) {
    const quota = quotas.find(q => q.account_id === account.id);
    const at = Date.parse(quota?.updated_at ?? '') || Date.now();
    const all = quota?.windows ?? [];
    const longest = Math.max(0, ...all.map(windowRank));
    if (!longest) continue;
    const windows = all.filter(window => windowRank(window) === longest).flatMap(window => {
      const reset = Date.parse(window.reset_at ?? '');
      const length = windowLength(window, reset);
      const left = reset - at;
      if (!length || !(left > 0) || window.remaining_percent == null) return [];
      const elapsed = Math.min(1, Math.max(0, 1 - left / length)) * 100;
      // Read at the reset instant: no time has run, so the window says nothing yet.
      return elapsed > 0 ? [{ used: 100 - window.remaining_percent, elapsed }] : [];
    });
    if (!windows.length) continue;
    for (const window of windows) {
      used += window.used;
      expected += window.elapsed;
    }
    known++;
  }
  return { percent: expected > 0 ? Math.round(used / expected * 100) : null, known, total: enabled.length };
}
