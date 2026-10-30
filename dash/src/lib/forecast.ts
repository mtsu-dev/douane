import type { Account, Quota, WindowForecast } from './management';

/** Normalize each limit against its own reset, then count each account once. */
export function forecastSummary(accounts: Account[], quotas: Quota[], forecasts: WindowForecast[]) {
  const enabled = accounts.filter(a => a.enabled);
  const ratios: number[] = [];
  let exhausted = 0;
  for (const account of enabled) {
    const quota = quotas.find(q => q.account_id === account.id);
    if (!quota?.windows.length) continue;
    const at = Date.parse(quota.updated_at ?? '');
    const limits = quota.windows.map(window => {
      const hours = (Date.parse(window.reset_at ?? '') - at) / 3600_000;
      if (!Number.isFinite(hours) || hours <= 0 || window.remaining_percent == null) return null;
      const forecast = forecasts.find(f => f.account_id === account.id && f.window_id === window.id);
      if (!forecast) return null;
      return window.remaining_percent === 0 ? Infinity : forecast.rate_per_hour * hours / window.remaining_percent;
    });
    if (limits.includes(Infinity)) { exhausted++; continue; }
    if (limits.some(ratio => ratio == null)) continue;
    ratios.push(Math.max(...limits as number[]));
  }
  const percent = ratios.length && !exhausted ? Math.round(ratios.reduce((sum, ratio) => sum + ratio, 0) / ratios.length * 100) : null;
  return { percent, known: ratios.length + exhausted, total: enabled.length, exhausted };
}
