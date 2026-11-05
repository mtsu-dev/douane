import type { Account, HistorySeries, PlanWeights, Quota, Subscription } from './management';
import { isMonthly, windowLength, windowRank } from './pace.ts';

export const HOUR = 3600_000;
export const DAY = 24 * HOUR;
/** Projection resolution. */
export const STEP = 15 * 60_000;
/** Below this much recorded time, a burn rate falls back to the window's average since reset. */
const MIN_SPAN = 15 * 60_000;

/** One reading: time and reset in ms, remaining in percent of the window. */
export type Point = { t: number; v: number; reset: number | null };
/** One quota window of one account, weighted by its share of the pool. */
export type Series = { accountId: string; windowId: string; weight: number; monthly: boolean; length: number | null; points: Point[]; history: number };
export type Sample = { t: number; v: number };

const subscriptionOf = (account: Account, quotas: Quota[]) => quotas.find(q => q.account_id === account.id)?.subscription ?? account.subscription ?? null;

/** The weight a plan carries before normalisation: the user's choice, else the plan multiplier, else 1. */
export function rawWeight(provider: string, subscription: Subscription | null | undefined, planWeights?: PlanWeights) {
	const chosen = subscription?.id ? planWeights?.[provider]?.[subscription.id] : undefined;
	return chosen && chosen > 0 ? chosen : subscription?.multiplier && subscription.multiplier > 0 ? subscription.multiplier : 1;
}

/**
 * Normalised weight per enabled account: raw weights divided by the most
 * common one (ties go to the larger), so the usual plan counts 1x.
 */
export function accountWeights(accounts: Account[], quotas: Quota[], planWeights?: PlanWeights) {
	const raw = new Map(accounts.filter(a => a.enabled).map(a => [a.id, rawWeight(a.provider, subscriptionOf(a, quotas), planWeights)]));
	const counts = new Map<number, number>();
	for (const weight of raw.values()) counts.set(weight, (counts.get(weight) ?? 0) + 1);
	let base = 1, best = 0;
	for (const [weight, count] of counts) if (count > best || (count === best && weight > base)) { base = weight; best = count; }
	return new Map([...raw].map(([id, weight]) => [id, weight / base]));
}

/** Plans whose normalised weight differs from 1x, for the muted note beside the pool total. */
export function weightNotes(accounts: Account[], quotas: Quota[], weights: Map<string, number>) {
	const notes = new Map<string, number>();
	for (const account of accounts) {
		const weight = weights.get(account.id);
		if (weight == null || Math.abs(weight - 1) < 1e-9) continue;
		notes.set(subscriptionOf(account, quotas)?.label ?? account.plan ?? account.name, weight);
	}
	return [...notes].map(([label, weight]) => ({ label, weight }));
}

/**
 * The pool's series: per enabled account, only its longest windows (monthly
 * over weekly over 5h); n buckets of that length share the account weight.
 * Recorded points come before `now`; the current snapshot is the last point.
 */
export function poolSeries(accounts: Account[], quotas: Quota[], history: HistorySeries[], weights: Map<string, number>, now: number): Series[] {
	const series: Series[] = [];
	for (const account of accounts) {
		const weight = weights.get(account.id);
		if (!account.enabled || weight == null) continue;
		const all = quotas.find(q => q.account_id === account.id)?.windows ?? [];
		const longest = Math.max(0, ...all.map(windowRank));
		if (!longest) continue;
		const chosen = all.filter(w => windowRank(w) === longest && w.remaining_percent != null);
		for (const window of chosen) {
			const reset = Date.parse(window.reset_at ?? '') || null;
			const recorded = (history.find(h => h.account_id === account.id && h.window_id === window.id)?.points ?? [])
				.map(([t, v, r]) => ({ t: t * 1000, v, reset: r == null ? null : r * 1000 }))
				.filter(p => p.t < now);
			series.push({
				accountId: account.id, windowId: window.id, weight: weight / chosen.length, monthly: isMonthly(window),
				length: reset ? windowLength(window, reset) : windowLength(window, 0),
				points: [...recorded, { t: now, v: window.remaining_percent as number, reset }], history: recorded.length
			});
		}
	}
	return series;
}

export const capacity = (series: Series[]) => series.reduce((sum, s) => sum + s.weight * 100, 0);

/** The next reset after `reset`: a calendar month later for monthly windows, else one window length. */
export function nextReset(series: Series, reset: number) {
	if (series.monthly) {
		const date = new Date(reset);
		const year = date.getUTCFullYear(), month = date.getUTCMonth() + 1;
		const lastDay = new Date(Date.UTC(year, month + 1, 0)).getUTCDate();
		return Date.UTC(year, month, Math.min(date.getUTCDate(), lastDay), date.getUTCHours(), date.getUTCMinutes(), date.getUTCSeconds());
	}
	return series.length ? reset + series.length : null;
}

function lastIndexAtOrBefore(points: Point[], t: number) {
	let low = 0, high = points.length - 1, found = -1;
	while (low <= high) {
		const mid = (low + high) >> 1;
		if (points[mid].t <= t) { found = mid; low = mid + 1; } else high = mid - 1;
	}
	return found;
}

/** Remaining percent at `t`: the last reading, back to 100 once its reset has passed. Before any reading, the first one holds. */
export function valueAt(series: Series, t: number) {
	const index = lastIndexAtOrBefore(series.points, t);
	const point = series.points[Math.max(0, index)];
	return index >= 0 && point.reset != null && point.reset <= t ? 100 : point.v;
}

export const poolValue = (series: Series[], t: number) => series.reduce((sum, s) => sum + s.weight * valueAt(s, t), 0);

/** When the recorded line starts: the earliest reading, or now with no history. */
export const recordedStart = (series: Series[]) => Math.min(...series.map(s => s.points[0]?.t ?? Infinity));

/** Recorded pool values between `from` and `to`, with a vertical step at each reset. */
export function recordedLine(series: Series[], from: number, to: number): Sample[] {
	const start = Math.max(from, recordedStart(series));
	if (!(start <= to)) return [];
	const times = new Set<number>([start, to]);
	for (const s of series) {
		s.points.forEach((point, i) => {
			if (point.t >= start && point.t <= to) times.add(point.t);
			const until = s.points[i + 1]?.t ?? to;
			if (point.reset != null && point.reset > point.t && point.reset <= until && point.reset > start && point.reset <= to) { times.add(point.reset - 1); times.add(point.reset); }
		});
	}
	return [...times].sort((a, b) => a - b).map(t => ({ t, v: poolValue(series, t) }));
}

/** Percent used between two readings: rises are resets, a reset between them counts from 100. */
function drop(a: Point, b: Point) {
	const base = a.reset != null && a.reset <= b.t ? 100 : a.v;
	return Math.max(0, base - b.v);
}

/** Pool percent burnt per hour, hour by hour, from the recorded readings. */
export function drainLine(series: Series[], from: number, to: number): Sample[] {
	const first = Math.floor(Math.max(from, recordedStart(series)) / HOUR) * HOUR;
	const buckets = new Map<number, number>();
	for (let t = first; t <= to; t += HOUR) buckets.set(t, 0);
	for (const s of series) {
		for (let i = 1; i < s.points.length; i++) {
			const bucket = Math.floor(s.points[i].t / HOUR) * HOUR;
			if (buckets.has(bucket)) buckets.set(bucket, (buckets.get(bucket) ?? 0) + s.weight * drop(s.points[i - 1], s.points[i]));
		}
	}
	return [...buckets].map(([t, v]) => ({ t, v }));
}

/** Average use of a window since its reset, in percent per hour, from the snapshot alone. */
function sinceReset(series: Series, now: number) {
	const last = series.points[series.points.length - 1];
	if (!last.reset || !series.length || last.reset <= now) return 0;
	const elapsed = series.length - (last.reset - now);
	return elapsed > 0 ? (100 - last.v) / (elapsed / HOUR) : 0;
}

/**
 * Pool percent per hour over the last `hours`: per series, the drops between
 * readings in that span over the time covered. A series with no history or
 * less than 15 minutes of it uses its average since reset (`fallback`).
 */
export function burnRate(series: Series[], now: number, hours: number) {
	const from = now - hours * HOUR;
	let fallback = false;
	const rates = series.map(s => {
		const span = s.history ? Math.min(hours * HOUR, now - s.points[0].t) : 0;
		if (span < MIN_SPAN) { fallback = true; return s.weight * sinceReset(s, now); }
		let used = 0;
		for (let i = 1; i < s.points.length; i++) if (s.points[i].t > from && s.points[i].t <= now) used += drop(s.points[i - 1], s.points[i]);
		return s.weight * used / (span / HOUR);
	});
	return { rate: rates.reduce((sum, r) => sum + r, 0), rates, fallback };
}

export type Projection = {
	line: Sample[];
	/** Per account, its weighted share of the pool. */
	accounts: Map<string, Sample[]>;
	resets: { t: number; gain: number }[];
	poolEmpty: number | null;
	firstEmpty: { accountId: string; t: number } | null;
};

/**
 * The pool from `now` to `until` at total rate `rate` (pool percent per hour),
 * split over series by their own `rates`. A series that empties hands its share
 * to those with quota left (douane fails over within a provider), equally when
 * none has a share. Each reset brings its series back to 100.
 */
export function project(series: Series[], now: number, until: number, rate: number, rates: number[]): Projection {
	const state = series.map(s => {
		const last = s.points[s.points.length - 1];
		let v = last.v, next = last.reset;
		while (next != null && next <= now) { v = 100; next = nextReset(s, next); }
		return { v, next };
	});
	const accountIds = [...new Set(series.map(s => s.accountId))];
	const accounts = new Map<string, Sample[]>(accountIds.map(id => [id, []]));
	const total = () => state.reduce((sum, st, i) => sum + series[i].weight * st.v, 0);
	const record = (t: number) => {
		line.push({ t, v: total() });
		for (const id of accountIds) accounts.get(id)?.push({ t, v: state.reduce((sum, st, i) => sum + (series[i].accountId === id ? series[i].weight * st.v : 0), 0) });
	};
	const line: Sample[] = [];
	const resets: Projection['resets'] = [];
	let poolEmpty: number | null = null, firstEmpty: Projection['firstEmpty'] = null;
	const empty = state.map(st => st.v <= 0);
	if (series.length && empty.every(Boolean)) poolEmpty = now;
	record(now);
	function consume(hours: number) {
		let amount = rate * hours;
		for (let round = 0; amount > 1e-9 && round < series.length + 1; round++) {
			const active = state.map((st, i) => st.v > 1e-9 ? i : -1).filter(i => i >= 0);
			if (!active.length) return;
			const shares = active.map(i => Math.max(0, rates[i] ?? 0));
			const sum = shares.reduce((a, b) => a + b, 0);
			let left = 0;
			active.forEach((i, k) => {
				const part = amount * (sum > 0 ? shares[k] / sum : 1 / active.length);
				const take = Math.min(state[i].v, part / series[i].weight);
				state[i].v -= take;
				left += part - take * series[i].weight;
			});
			amount = left;
		}
	}
	let t = now;
	while (t < until) {
		const upcoming = Math.min(...state.map(st => st.next != null && st.next > t ? st.next : Infinity));
		const to = Math.min(t + STEP, upcoming, until);
		consume((to - t) / HOUR);
		t = to;
		state.forEach((st, i) => {
			if (st.v <= 1e-9 && !empty[i]) { st.v = 0; empty[i] = true; firstEmpty ??= { accountId: series[i].accountId, t }; }
		});
		if (poolEmpty == null && series.length && state.every(st => st.v <= 0)) poolEmpty = t;
		const due = state.map((st, i) => st.next != null && st.next <= t ? i : -1).filter(i => i >= 0);
		if (due.length) {
			record(t);
			let gain = 0;
			for (const i of due) {
				gain += series[i].weight * (100 - state[i].v);
				state[i].v = 100; empty[i] = false;
				state[i].next = nextReset(series[i], state[i].next as number);
			}
			resets.push({ t, gain });
		}
		record(t);
	}
	return { line, accounts, resets, poolEmpty, firstEmpty };
}

/** The earliest reset after `now` and the pool percent it brings back, from a projection that runs past it. */
export function firstReset(projection: Projection, now: number) {
	return projection.resets.find(r => r.t > now) ?? null;
}

/** Recorded values of one account's share of the pool. */
export function accountLine(series: Series[], accountId: string, from: number, to: number) {
	return recordedLine(series.filter(s => s.accountId === accountId), from, to);
}

/** A line's value at `t`, linear between samples; null outside it. */
export function sampleAt(samples: Sample[], t: number) {
	if (!samples.length || t < samples[0].t || t > samples[samples.length - 1].t) return null;
	let low = 0, high = samples.length - 1;
	while (low < high) { const mid = (low + high + 1) >> 1; if (samples[mid].t <= t) low = mid; else high = mid - 1; }
	const a = samples[low], b = samples[low + 1];
	return !b || b.t === a.t ? a.v : a.v + (b.v - a.v) * (t - a.t) / (b.t - a.t);
}
