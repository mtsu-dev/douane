import type { Account, Quota, QuotaWindow } from './management';

const HOUR = 3600_000;
const LENGTHS: Record<string, number> = { 'five-hour': 5 * HOUR, daily: 24 * HOUR, weekly: 168 * HOUR, 'seven-day': 168 * HOUR, seven_day: 168 * HOUR, monthly: 720 * HOUR };

/** Window length in ms, from its id or, for provider buckets, its label. */
function windowLength(window: QuotaWindow) {
  const known = LENGTHS[window.id] ?? Number(/^window-(\d+)s$/.exec(window.id)?.[1]) * 1000;
  if (known) return known;
  if (/week/i.test(window.label)) return LENGTHS.weekly;
  if (/month/i.test(window.label)) return LENGTHS.monthly;
  return null;
}

/**
 * How fast a provider's subscriptions are used: quota used so far against the
 * share of each window already elapsed. 100% spends the quota exactly at reset.
 * Each account counts its weekly window (else its longest known one), weighted
 * by how far that window has run, so a week that just started weighs little.
 */
export function providerPace(accounts: Account[], quotas: Quota[]) {
  const enabled = accounts.filter(a => a.enabled);
  let used = 0, expected = 0, known = 0;
  for (const account of enabled) {
    const quota = quotas.find(q => q.account_id === account.id);
    const at = Date.parse(quota?.updated_at ?? '') || Date.now();
    const windows = (quota?.windows ?? []).flatMap(window => {
      const length = windowLength(window);
      const left = Date.parse(window.reset_at ?? '') - at;
      if (!length || !(left > 0) || window.remaining_percent == null) return [];
      return [{ length, used: 100 - window.remaining_percent, elapsed: Math.min(1, Math.max(0, 1 - left / length)) * 100 }];
    });
    const longest = Math.max(...windows.map(w => w.length === LENGTHS.weekly ? Infinity : w.length));
    const chosen = windows.filter(w => (w.length === LENGTHS.weekly ? Infinity : w.length) === longest);
    if (!chosen.length) continue;
    // Several buckets of the same length: the busiest one sets the account's pace.
    const busiest = chosen.reduce((a, b) => (b.used * a.elapsed > a.used * b.elapsed ? b : a));
    used += busiest.used;
    expected += busiest.elapsed;
    known++;
  }
  return { percent: expected > 0 ? Math.round(used / expected * 100) : null, known, total: enabled.length };
}
