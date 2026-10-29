import type { Account, Quota, WindowForecast } from './management';

export type Pace = 'fast' | 'slow' | 'balanced' | 'mixed' | 'unknown';
export function pace(forecast: WindowForecast): 'fast' | 'slow' | 'balanced' {
	return forecast.exhausts_at ? 'fast' : forecast.remaining_at_reset > 20 ? 'slow' : 'balanced';
}

/** Count limits, never add percentages from different plans or overlapping windows. */
export function forecastSummary(accounts: Account[], quotas: Quota[], forecasts: WindowForecast[]) {
	const enabled = new Set(accounts.filter(a => a.enabled).map(a => a.id));
	const windows = quotas.filter(q => enabled.has(q.account_id)).flatMap(q => q.windows.map(window => ({ account: q.account_id, window })));
	const rows = windows.map(({ account, window }) => ({
		account: accounts.find(a => a.id === account)!, window,
		forecast: forecasts.find(f => f.account_id === account && f.window_id === window.id)
	}));
	const known = rows.filter(row => row.forecast != null);
	const counts = { fast: 0, slow: 0, balanced: 0 };
	for (const row of known) counts[pace(row.forecast!)]++;
	const status: Pace = counts.fast && counts.slow ? 'mixed' : counts.fast ? 'fast' : counts.slow ? 'slow' : known.length ? 'balanced' : 'unknown';
	const missingAccounts = [...enabled].filter(id => !quotas.some(q => q.account_id === id && q.windows.length)).length;
	return { rows, counts, status, known: known.length, total: rows.length, missingAccounts };
}
