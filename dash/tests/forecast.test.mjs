import { test } from 'node:test';
import assert from 'node:assert/strict';
import { forecastSummary } from '../src/lib/forecast.ts';

const at = '2026-10-30T12:00:00Z';
const reset = hours => new Date(Date.parse(at) + hours * 3600_000).toISOString();
const account = id => ({ id, enabled: true });
const window = (id, remaining_percent, hours) => ({ id, remaining_percent, reset_at: reset(hours) });
const quota = (id, windows) => ({ account_id: id, updated_at: at, windows });
const forecast = (id, window_id, rate_per_hour) => ({ account_id: id, window_id, rate_per_hour });

test('pace compares observed rate with remaining quota divided by time until its actual reset', () => {
  const accounts = [account('a')];
  const forecasts = [forecast('a', 'weekly', 1)];
  assert.equal(forecastSummary(accounts, [quota('a', [window('weekly', 50, 24)])], forecasts).percent, 48);
  assert.equal(forecastSummary(accounts, [quota('a', [window('weekly', 50, 48)])], forecasts).percent, 96);
});

test('provider average counts each enabled account once using its most constrained window', () => {
  const accounts = [account('a'), account('b'), { id: 'disabled', enabled: false }];
  const quotas = [quota('a', [window('weekly', 50, 24), window('five-hour', 10, 2)]), quota('b', [window('weekly', 50, 12)])];
  const forecasts = [forecast('a', 'weekly', 2), forecast('a', 'five-hour', 10), forecast('b', 'weekly', 5)];
  const summary = forecastSummary(accounts, quotas, forecasts);
  assert.equal(summary.percent, 160); // mean(200%, 120%), not mean(96%, 200%, 120%)
  assert.equal(summary.known, 2);
  assert.equal(summary.total, 2);
});

test('unknown constraints stay outside the average and remain in account coverage', () => {
  const accounts = [account('a'), account('b'), account('missing')];
  const quotas = [quota('a', [window('weekly', 50, 24), window('five-hour', null, 2)]), quota('b', [window('weekly', 50, 12)])];
  const forecasts = [forecast('a', 'weekly', 2), forecast('b', 'weekly', 5)];
  const summary = forecastSummary(accounts, quotas, forecasts);
  assert.equal(summary.percent, 120);
  assert.equal(summary.known, 1);
  assert.equal(summary.total, 3);
  assert.equal(forecastSummary(accounts, quotas, []).percent, null);
});

test('observed inactivity is zero pace; an exhausted limit cannot invent a finite rate', () => {
  const accounts = [account('a')];
  const forecasts = [forecast('a', 'weekly', 0)];
  assert.equal(forecastSummary(accounts, [quota('a', [window('weekly', 100, 24)])], forecasts).percent, 0);
  const exhausted = forecastSummary(accounts, [quota('a', [window('weekly', 0, 24)])], forecasts);
  assert.equal(exhausted.exhausted, 1);
  assert.equal(exhausted.percent, null);
  const unavailable = forecastSummary(accounts, [quota('a', [window('weekly', 0, 24)])], []);
  assert.equal(unavailable.exhausted, 0); // no usable forecast can mean the reading is stale
  assert.equal(unavailable.known, 0);
});
