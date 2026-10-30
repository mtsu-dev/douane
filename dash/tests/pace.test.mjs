import { test } from 'node:test';
import assert from 'node:assert/strict';
import { providerPace } from '../src/lib/pace.ts';

const at = '2026-10-30T12:00:00Z';
const day = 24;
const reset = hours => new Date(Date.parse(at) + hours * 3600_000).toISOString();
const account = id => ({ id, enabled: true });
const window = (id, remaining_percent, hours, label = id) => ({ id, label, remaining_percent, reset_at: hours == null ? null : reset(hours) });
const quota = (id, windows) => ({ account_id: id, updated_at: at, windows });

test('pace compares weekly use with the share of the week already elapsed, with no history needed', () => {
  // 3.5 days left of 7: half the week elapsed. 50% used is on pace, 75% is 150%, 25% is 50%.
  for (const [remaining, percent] of [[50, 100], [25, 150], [75, 50]]) {
    assert.equal(providerPace([account('a')], [quota('a', [window('weekly', remaining, 3.5 * day)])]).percent, percent);
  }
});

test('the provider pace weighs accounts by the time their week has run', () => {
  const accounts = [account('a'), account('b'), account('c'), { id: 'off', enabled: false }];
  const quotas = [
    quota('a', [window('five-hour', 10, 2), window('weekly', 15, 4.5 * day)]), // 85 used, 2.5/7 elapsed
    quota('b', [window('five-hour', 100, null), window('seven_day', 99, 6.25 * day)]), // 1 used, 0.75/7 elapsed
    quota('c', [window('weekly', 0, 0.5 * day)]), // 100 used, 6.5/7 elapsed
    quota('off', [window('weekly', 0, 6.9 * day)])
  ];
  const pace = providerPace(accounts, quotas);
  assert.equal(pace.percent, Math.round(186 / (9.75 / 7 * 100) * 100));
  assert.deepEqual([pace.known, pace.total], [3, 3]);
});

test('weekly windows win; otherwise the longest window with a known length counts', () => {
  const quotas = [quota('a', [window('five-hour', 0, 1), window('monthly', 50, 15.5 * day)])];
  assert.equal(providerPace([account('a')], quotas).percent, 100);
  const buckets = [quota('a', [window('bucket-1', 20, 2, 'Gemini 5h'), window('bucket-2', 70, 3.5 * day, 'Gemini Weekly')])];
  assert.equal(providerPace([account('a')], buckets).percent, 60);
});

test('a week that has not started or has no reset date stays out instead of blocking the line', () => {
  const quotas = [quota('a', [window('weekly', 100, null)]), quota('b', [window('weekly', 40, 3.5 * day)]), quota('c', [window('model-x', 10, 3)])];
  const pace = providerPace([account('a'), account('b'), account('c')], quotas);
  assert.equal(pace.percent, 120);
  assert.deepEqual([pace.known, pace.total], [1, 3]);
  assert.equal(providerPace([account('a')], [quota('a', [window('weekly', 100, null)])]).percent, null);
});

test('a window read at its reset instant stays out instead of inflating the pace', () => {
  const quotas = [quota('a', [window('weekly', 50, 7 * day)]), quota('b', [window('weekly', 50, 3.5 * day)])];
  const pace = providerPace([account('a'), account('b')], quotas);
  assert.equal(pace.percent, 100);
  assert.deepEqual([pace.known, pace.total], [1, 2]);
});

test('a weekly bucket just reset does not hide a busier bucket of the same account', () => {
  const quotas = [quota('a', [window('weekly', 100, 7 * day), window('opus-weekly', 50, 3.5 * day, 'Opus weekly')])];
  const pace = providerPace([account('a')], quotas);
  assert.equal(pace.percent, 100);
  assert.equal(pace.known, 1);
});

test('a monthly window spans its real calendar month', () => {
  // Reset on November 1: the window started October 1 and lasts 31 days. Read at its midpoint, 50% used is on pace.
  const reset_at = '2026-11-01T00:00:00Z';
  const updated_at = new Date(Date.parse('2026-10-01T00:00:00Z') + 15.5 * 24 * 3600_000).toISOString();
  const quotas = [{ account_id: 'a', updated_at, windows: [{ id: 'monthly', label: 'monthly', remaining_percent: 50, reset_at }] }];
  assert.equal(providerPace([account('a')], quotas).percent, 100);
  // Read on October 1 at noon, more than 30 days before reset: still counted.
  const early = [{ account_id: 'a', updated_at: '2026-10-01T12:00:00Z', windows: [{ id: 'monthly', label: 'monthly', remaining_percent: 99, reset_at }] }];
  assert.equal(providerPace([account('a')], early).known, 1);
});
