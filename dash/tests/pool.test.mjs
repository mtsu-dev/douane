import { test } from 'node:test';
import assert from 'node:assert/strict';
import { accountWeights, weightNotes, poolSeries, capacity, valueAt, poolValue, recordedLine, burnRate, project, firstReset, drainLine, historyRequests, HOUR, DAY } from '../src/lib/pool.ts';

const now = Date.parse('2026-11-05T12:00:00Z');
const iso = ms => new Date(ms).toISOString();
const sub = (id, label, multiplier = null) => ({ id, label, multiplier, source: 'provider_usage' });
const account = (id, subscription, enabled = true) => ({ id, name: id, provider: 'claude', enabled, status: 'ready', subscription });
const weekly = (remaining, resetIn) => ({ id: 'weekly', label: 'Weekly', remaining_percent: remaining, reset_at: resetIn == null ? null : iso(now + resetIn) });
const quota = (id, windows) => ({ account_id: id, windows, updated_at: iso(now) });
const ultra = sub('ultra', 'Claude Ultra', 20), pro = sub('pro', 'Claude Pro');
const s = seconds => Math.floor(seconds / 1000);

test('weights normalise by the most common plan, ties going to the larger', () => {
	const accounts = [account('a', ultra), account('b', ultra), account('c', ultra), account('d', pro)];
	const weights = accountWeights(accounts, [], undefined);
	assert.deepEqual([...weights.values()], [1, 1, 1, 0.05]);
	const series = poolSeries(accounts, accounts.map(a => quota(a.id, [weekly(100, 3 * DAY)])), [], weights, now);
	assert.equal(Math.round(capacity(series)), 305);
	assert.deepEqual(weightNotes(accounts, [], weights), [{ label: 'Claude Pro', weight: 0.05 }]);
	// One Ultra and one Pro: a tie, the larger (Ultra) is the unit.
	assert.deepEqual([...accountWeights([account('a', ultra), account('d', pro)], []).values()], [1, 0.05]);
	// A user weight overrides the multiplier; a disabled account stays out.
	const custom = accountWeights([account('a', ultra), account('d', pro), account('x', ultra, false)], [], { claude: { pro: 10 } });
	assert.deepEqual([...custom], [['a', 1], ['d', 0.5]]);
});

test('an account with two buckets of its longest length gives two half-weight series; shorter windows stay out', () => {
	const accounts = [account('a', ultra)];
	const quotas = [quota('a', [{ id: 'five-hour', label: '5 hours', remaining_percent: 10, reset_at: iso(now + HOUR) }, weekly(80, DAY), { id: 'opus', label: 'Opus weekly', remaining_percent: 40, reset_at: iso(now + DAY) }])];
	const series = poolSeries(accounts, quotas, [], new Map([['a', 1]]), now);
	assert.deepEqual(series.map(x => [x.windowId, x.weight]), [['weekly', 0.5], ['opus', 0.5]]);
	assert.equal(poolValue(series, now), 60);
});

test('a reading holds until the next one and returns to 100 once its reset passes', () => {
	const history = [{ account_id: 'a', window_id: 'weekly', points: [[s(now - 10 * HOUR), 30, s(now - 5 * HOUR)], [s(now - 4 * HOUR), 95, s(now + 6 * DAY)]] }];
	const [series] = poolSeries([account('a', ultra)], [quota('a', [weekly(90, 6 * DAY)])], history, new Map([['a', 1]]), now);
	assert.equal(valueAt(series, now - 8 * HOUR), 30);
	assert.equal(valueAt(series, now - 5 * HOUR), 100);
	assert.equal(valueAt(series, now - 3 * HOUR), 95);
	assert.equal(valueAt(series, now), 90);
	// The recorded line draws a vertical step at the reset.
	const line = recordedLine([series], now - 12 * HOUR, now);
	const at = line.findIndex(p => p.t === now - 5 * HOUR);
	assert.deepEqual([line[at - 1].v, line[at].v], [30, 100]);
	assert.equal(line[0].t, now - 10 * HOUR);
	assert.equal(line.at(-1).v, 90);
});

test('with no history the pool still has its current point', () => {
	const [series] = poolSeries([account('a', ultra)], [quota('a', [weekly(70, 3 * DAY)])], [], new Map([['a', 1]]), now);
	assert.deepEqual(recordedLine([series], now - 4 * DAY, now), [{ t: now, v: 70 }]);
});

test('burn sums drops over the window, ignores resets, and falls back to the average since reset without history', () => {
	const history = [{ account_id: 'a', window_id: 'weekly', points: [[s(now - 5 * HOUR), 80, s(now + DAY)], [s(now - 2 * HOUR), 78, s(now + DAY)], [s(now - HOUR), 76, s(now + DAY)]] }];
	const weights = new Map([['a', 1], ['b', 0.05]]);
	const accounts = [account('a', ultra), account('b', pro)];
	// b: 6 of 7 days left, 40% used in its first day: 40/24 %/h, times 0.05.
	const quotas = [quota('a', [weekly(73, DAY)]), quota('b', [weekly(60, 6 * DAY)])];
	const series = poolSeries(accounts, quotas, history, weights, now);
	const burn = burnRate(series, now, 3);
	// a: 2 (at -2h, counted since it falls after -3h) + 2 + 3 over 3h.
	assert.ok(Math.abs(burn.rates[0] - 7 / 3) < 1e-9);
	assert.ok(Math.abs(burn.rates[1] - 0.05 * 40 / 24) < 1e-9);
	assert.equal(burn.fallback, true);
	// A rise between readings is a reset: only the drop after it counts, from 100.
	const reset = [{ account_id: 'a', window_id: 'weekly', points: [[s(now - 2 * HOUR), 5, s(now - 1.5 * HOUR)], [s(now - HOUR), 96, s(now + 7 * DAY - HOUR)]] }];
	const one = poolSeries([account('a', ultra)], [quota('a', [weekly(95, 7 * DAY - HOUR)])], reset, new Map([['a', 1]]), now);
	const r = burnRate(one, now, 3);
	// Span is 2h of history; drops: 100-96 then 1.
	assert.ok(Math.abs(r.rate - 5 / 2) < 1e-9);
	assert.equal(r.fallback, false);
	// Drain buckets per hour.
	const drain = drainLine(one, now - 3 * HOUR, now);
	assert.deepEqual(drain.map(d => d.v), [0, 4, 1]);
});

test('projection moves an emptied series share to the series with quota left', () => {
	const weights = new Map([['a', 1], ['b', 1]]);
	const accounts = [account('a', ultra), account('b', ultra)];
	const quotas = [quota('a', [weekly(10, 6 * DAY)]), quota('b', [weekly(100, 6 * DAY)])];
	const series = poolSeries(accounts, quotas, [], weights, now);
	// 4 pool %/h, all on a: a empties after 2.5h, then b takes the whole rate.
	const p = project(series, now, now + 10 * HOUR, 4, [4, 0]);
	assert.equal(p.firstEmpty.accountId, 'a');
	assert.equal(p.firstEmpty.t, now + 2.5 * HOUR);
	assert.ok(Math.abs(p.line.at(-1).v - (110 - 40)) < 1e-6);
	assert.ok(Math.abs(p.accounts.get('b').at(-1).v - 70) < 1e-6);
	assert.equal(p.poolEmpty, null);
	// Both empty: the pool empties when the total runs out.
	const fast = project(series, now, now + 10 * HOUR, 22, [1, 1]);
	assert.equal(fast.poolEmpty, now + 5 * HOUR);
	// No share anywhere: an equal split.
	const flat = project(series, now, now + HOUR, 2, [0, 0]);
	assert.ok(Math.abs(flat.accounts.get('a').at(-1).v - 9) < 1e-6);
});

test('each reset brings its series back to 100 with a weighted gain, then repeats a window later', () => {
	const weights = new Map([['a', 1], ['b', 0.05]]);
	const accounts = [account('a', ultra), account('b', pro)];
	const quotas = [quota('a', [weekly(40, 2 * HOUR)]), quota('b', [weekly(0, 3 * HOUR)])];
	const series = poolSeries(accounts, quotas, [], weights, now);
	const p = project(series, now, now + 8 * DAY, 0, [0, 0]);
	assert.deepEqual(p.resets.slice(0, 3).map(r => [r.t - now, Math.round(r.gain * 100) / 100]), [[2 * HOUR, 60], [3 * HOUR, 5], [7 * DAY + 2 * HOUR, 0]]);
	assert.deepEqual(firstReset(p, now), { t: now + 2 * HOUR, gain: 60 });
	// The line steps vertically at the reset.
	const at = p.line.findIndex(x => x.t === now + 2 * HOUR);
	assert.deepEqual([p.line[at].v, p.line[at + 1].v], [40, 100]);
	// An empty series refilled by its reset is not reported as emptying.
	assert.equal(p.firstEmpty, null);
});

test('windows of no known length still count when an account has nothing longer (Copilot, Antigravity, Kimi)', () => {
	const copilot = { id: 'c', name: 'c', provider: 'copilot', enabled: true, status: 'ready' };
	const quotas = [{ account_id: 'c', windows: [{ id: 'premium', label: 'Premium requests', remaining_percent: 65, reset_at: iso(now + 9 * DAY) }], updated_at: iso(now) }];
	const series = poolSeries([copilot], quotas, [], new Map([['c', 1]]), now);
	assert.equal(capacity(series), 100);
	assert.equal(poolValue(series, now), 65);
	// A bucket id that names its hours gets that length.
	const gemini = [{ account_id: 'c', windows: [{ id: 'gemini-5h', label: 'Gemini', remaining_percent: 30, reset_at: iso(now + HOUR) }], updated_at: iso(now) }];
	assert.equal(poolSeries([copilot], gemini, [], new Map([['c', 1]]), now)[0].length, 5 * HOUR);
});

test('a stale snapshot keeps its read time and invents no use after a reset it has not seen', () => {
	const reset = now - HOUR;
	const read = now - 2 * HOUR;
	const history = [{ account_id: 'a', window_id: 'weekly', points: [[s(read), 10, s(reset)]] }];
	const stale = { account_id: 'a', windows: [{ id: 'weekly', label: 'Weekly', remaining_percent: 10, reset_at: iso(reset) }], updated_at: iso(read) };
	const series = poolSeries([account('a', ultra)], [stale], history, new Map([['a', 1]]), now);
	assert.equal(series[0].points.at(-1).t, read);
	assert.equal(poolValue(series, now), 100);
	assert.equal(burnRate(series, now, 3).rate, 0);
	assert.equal(project(series, now, now + DAY, 0, [0]).firstEmpty, null);
});

test('history requests always reach now, so a past range keeps the readings the burn needs', () => {
	// A range entirely in the past, within the server's 45 days: one request up to now.
	assert.deepEqual(historyRequests(now - 20 * DAY, now - 6 * DAY, now), [{ from: now - 20 * DAY, to: null }]);
	// Far in the past: the range itself, plus the last week up to now.
	assert.deepEqual(historyRequests(now - 60 * DAY, now - 46 * DAY, now), [{ from: now - 60 * DAY, to: now - 46 * DAY }, { from: now - 7 * DAY - HOUR, to: null }]);
	// The usual range around today reaches back a week for the averages.
	assert.deepEqual(historyRequests(now - 4 * DAY, now + 10 * DAY, now), [{ from: now - 7 * DAY - HOUR, to: null }]);
});
