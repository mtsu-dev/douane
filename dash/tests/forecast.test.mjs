import { test } from 'node:test';
import assert from 'node:assert/strict';
import { forecastSummary } from '../src/lib/forecast.ts';

test('global pace preserves a bottleneck alongside unused capacity and excludes disabled accounts', () => {
	const accounts = [{ id: 'fast', enabled: true }, { id: 'slow', enabled: true }, { id: 'disabled', enabled: false }];
	const quotas = accounts.map(a => ({ account_id: a.id, windows: [{ id: 'weekly' }] }));
	const forecasts = [
		{ account_id: 'fast', window_id: 'weekly', exhausts_at: '2026-11-01T00:00:00Z', remaining_at_reset: 0 },
		{ account_id: 'slow', window_id: 'weekly', exhausts_at: null, remaining_at_reset: 60 },
		{ account_id: 'disabled', window_id: 'weekly', exhausts_at: null, remaining_at_reset: 90 }
	];
	const summary = forecastSummary(accounts, quotas, forecasts);
	assert.equal(summary.status, 'mixed');
	assert.deepEqual(summary.counts, { fast: 1, slow: 1, balanced: 0 });
	assert.equal(summary.total, 2);
});

test('unknown limits remain in coverage and do not vote as balanced or idle', () => {
	const accounts = [{ id: 'a', enabled: true }, { id: 'unreported', enabled: true }];
	const quotas = [{ account_id: 'a', windows: [{ id: 'weekly' }, { id: 'five-hour' }] }];
	assert.equal(forecastSummary(accounts, quotas, []).status, 'unknown');
	const summary = forecastSummary(accounts, quotas, [{ account_id: 'a', window_id: 'weekly', exhausts_at: null, remaining_at_reset: 20 }]);
	assert.equal(summary.status, 'balanced');
	assert.equal(summary.known, 1);
	assert.equal(summary.total, 2);
	assert.equal(summary.missingAccounts, 1);
});
