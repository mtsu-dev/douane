<script module lang="ts">
	export type PoolMode = 'remaining' | 'drain';
	export type FutureUsage = 'same' | 'day' | 'week';
</script>

<script lang="ts">
	import Info from '@lucide/svelte/icons/info';
	import { Tween } from 'svelte/motion';
	import { cubicOut } from 'svelte/easing';
	import PoolChart, { type ChartLabel, type ChartLine, type HoverInfo } from './pool-chart.svelte';
	import { providerPace } from '$lib/pace';
	import { accountWeights, weightNotes, poolSeries, capacity, poolValue, recordedLine, recordedStart, drainLine, burnRate, project, firstReset, accountLine, sampleAt, DAY, HOUR, type Sample } from '$lib/pool';
	import { accountColors, day, duration, moment, percent, providerAccent, rate, weight } from '$lib/pool-format';
	import { privateLabel, providerName, type Account, type HistorySeries, type PlanWeights, type Quota } from '$lib/management';
	import { t } from '$lib/i18n.svelte';

	let { provider, accounts, quotas, history, now, start, end, mode, burnHours, future, showAccounts, planWeights }: {
		provider: string; accounts: Account[]; quotas: Quota[]; history: HistorySeries[]; now: number; start: number; end: number;
		mode: PoolMode; burnHours: number; future: FutureUsage; showAccounts: boolean; planWeights?: PlanWeights;
	} = $props();

	let accent = $derived(providerAccent(provider));
	let weights = $derived(accountWeights(accounts, quotas, planWeights));
	let notes = $derived(weightNotes(accounts, quotas, weights));
	let series = $derived(poolSeries(accounts, quotas, history, weights, now));
	let cap = $derived(capacity(series));
	let current = $derived(poolValue(series, now));
	let burn = $derived(burnRate(series, now, burnHours));
	let usage = $derived(future === 'same' ? burn : burnRate(series, now, future === 'day' ? 24 : 168));
	// Stats look ahead to the range end, or a week when the range is behind.
	let statsEnd = $derived(end > now ? end : now + 7 * DAY);
	let earliestReset = $derived(Math.min(...series.map(s => s.points[s.points.length - 1].reset ?? Infinity).filter(r => r > now)));
	let horizon = $derived(Math.min(now + 45 * DAY, Math.max(statsEnd, Number.isFinite(earliestReset) ? earliestReset + 1 : 0)));
	let projection = $derived(project(series, now, horizon, usage.rate, usage.rates));
	let pace = $derived(providerPace(accounts, quotas, weights));
	let paceState = $derived(pace.percent == null ? 'unknown' : pace.percent > 110 ? 'fast' : pace.percent < 90 ? 'slow' : 'balanced');
	let nextReset = $derived(firstReset(projection, now));
	let poolEmpty = $derived(projection.poolEmpty != null && projection.poolEmpty <= statsEnd ? projection.poolEmpty : null);
	let firstEmpty = $derived(projection.firstEmpty && projection.firstEmpty.t <= statsEnd ? projection.firstEmpty : null);
	const nameOf = (id: string) => privateLabel(accounts.find(a => a.id === id)?.name ?? id);
	let accountIds = $derived([...new Set(series.map(s => s.accountId))]);
	const colorOf = (id: string) => accountColors[accountIds.indexOf(id) % accountColors.length];

	// What the chart draws in each mode.
	let recorded = $derived.by<Sample[]>(() => {
		if (mode === 'remaining') return recordedLine(series, start, Math.min(end, now));
		return drainLine(series, start, Math.min(end, now)).flatMap(b => [{ t: b.t, v: b.v }, { t: Math.min(b.t + HOUR, now), v: b.v }]);
	});
	let projected = $derived.by<Sample[]>(() => {
		if (now >= end) return [];
		return mode === 'remaining' ? projection.line : [{ t: now, v: usage.rate }, { t: end, v: usage.rate }];
	});
	let top = $derived.by(() => {
		if (mode === 'remaining') return cap;
		const peak = Math.max(usage.rate, ...recorded.map(p => p.v)) * 1.2;
		const unit = peak > 20 ? 10 : peak > 5 ? 2 : peak > 2 ? 1 : 0.5;
		return Math.max(unit, Math.ceil(peak / unit) * unit);
	});
	let labels = $derived<ChartLabel[]>(mode !== 'remaining' ? [] : projection.resets.filter(r => r.gain >= 1).map(r => ({ t: r.t, v: sampleAt(projection.line, r.t) ?? 0, text: `+${percent(r.gain)}` })));
	let lines = $derived<ChartLine[]>(!showAccounts || mode !== 'remaining' ? [] : accountIds.map(id => ({ id, name: nameOf(id), color: colorOf(id), recorded: accountLine(series, id, start, Math.min(end, now)), projected: now < end ? projection.accounts.get(id) ?? [] : [] })));
	let firstRecorded = $derived(recordedStart(series));

	function hover(at: number): HoverInfo | null {
		if (mode === 'drain') {
			const value = at <= now ? sampleAt(recorded, at) : usage.rate;
			return value == null ? null : { level: value, value: rate(value), accounts: [] };
		}
		const before = at <= now;
		if (before && at < firstRecorded) return null;
		const value = before ? poolValue(series, at) : sampleAt(projection.line, at);
		if (value == null) return null;
		const items = showAccounts ? accountIds.map(id => {
			const own = before ? poolValue(series.filter(s => s.accountId === id), at) : sampleAt(projection.accounts.get(id) ?? [], at) ?? 0;
			return { id, name: nameOf(id), color: colorOf(id), value: percent(own) };
		}) : [];
		return { level: value, value: t('pool.left', { value: percent(value) }), accounts: items };
	}
	const tick = (value: number) => mode === 'remaining' ? percent(value) : rate(value);

	const shown = Tween.of(() => current, { duration: 700, easing: cubicOut });
	let methodOpen = $state(false);
</script>

<section class="pool" style:--accent={accent} aria-label={t('pool.chart', { provider: providerName(provider) })}>
	<div class="summary">
		<div class="total">
			<strong class="tabular">{percent(shown.current)}</strong>
			<span class="of">{t('pool.of', { capacity: percent(cap) })}</span>
			{#each notes as note (note.label)}<span class="note">{t('pool.weight', { plan: note.label, weight: weight(note.weight) })}</span>{/each}
		</div>
		<dl class="stats">
			<div class="stat pace" data-pace={paceState}>
				<dt>{t('pace.title')}<button class="info icon-button" aria-label={t('pace.about')} aria-expanded={methodOpen} aria-controls="pace-info-{provider}" onclick={() => methodOpen = !methodOpen}><Info size={14}/></button></dt>
				<dd><span class="tabular" role="meter" aria-label={t('pace.provider', { provider: providerName(provider) })} aria-valuenow={pace.percent ?? undefined} aria-valuemin="0" aria-valuemax="200" aria-valuetext={pace.percent == null ? t('pace.unknown') : t('pace.percent', { value: pace.percent })}>{pace.percent == null ? t('pace.unknown') : percent(pace.percent)}</span>{#if pace.percent != null}<small>{t(`pace.${paceState as 'fast' | 'slow' | 'balanced'}`)}</small>{/if}{#if pace.known < pace.total}<small class="coverage" title={t('pace.coverage', { known: pace.known, total: pace.total })}>{pace.known}/{pace.total}</small>{/if}</dd>
				{#if methodOpen}
					<div class="method" id="pace-info-{provider}">
						<p>{t('pace.method')}</p>
						<p>{t('pace.coverage', { known: pace.known, total: pace.total })}</p>
					</div>
				{/if}
			</div>
			<div class="stat"><dt>{t('pool.empty')}</dt><dd>{poolEmpty == null ? t('pool.emptyNot', { date: day(statsEnd - 1) }) : poolEmpty <= now ? t('pool.emptyNow') : moment(poolEmpty, now)}</dd></div>
			<div class="stat"><dt>{t('pool.firstEmpty')}</dt><dd>{firstEmpty ? t('pool.firstEmptyIn', { name: nameOf(firstEmpty.accountId), duration: duration(firstEmpty.t - now) }) : t('pool.noneBeforeReset')}</dd></div>
			<div class="stat"><dt>{t('pool.nextReset')}</dt><dd>{#if nextReset}<b class="tabular">+{percent(nextReset.gain)}</b> {moment(nextReset.t, now)}{:else}{t('pool.noReset')}{/if}</dd></div>
			<div class="stat"><dt>{burn.fallback ? t('pool.burnSince') : t('pool.burnLast', { hours: burnHours })}</dt><dd class="tabular">{rate(burn.rate)}</dd></div>
		</dl>
	</div>
	<PoolChart {start} {end} {now} max={top} {accent} {recorded} {projected} {labels} {lines} {hover} {tick} label={t('pool.chart', { provider: providerName(provider) })}/>
	<footer class="legend">
		<span class="legend-title">{t('pool.title')}</span>
		<span class="key"><i class="solid"></i>{t('pool.recorded')}</span>
		<span class="key"><i class="dashed"></i>{t('pool.projected')}</span>
		{#each lines as item (item.id)}<span class="key account"><i class="dot" style:background={item.color}></i>{item.name}</span>{/each}
	</footer>
</section>

<style>
	.pool { position: relative; min-width: 0; margin: 0 0 14px; padding: 22px 24px 16px; border: 1px solid var(--border); border-radius: 14px; background: radial-gradient(120% 90% at 0% 0%, color-mix(in srgb, var(--accent) 7%, transparent), transparent 55%), var(--surface); transition: border-color .25s, box-shadow .25s; }
	.pool:hover { border-color: color-mix(in srgb, var(--accent) 28%, var(--border)); box-shadow: 0 12px 34px #02061340; }
	.summary { display: flex; align-items: flex-end; justify-content: space-between; gap: 18px 36px; flex-wrap: wrap; margin-bottom: 10px; }
	.total { display: flex; align-items: baseline; flex-wrap: wrap; gap: 4px 12px; min-width: 0; }
	.total strong { font-family: var(--font-display); font-size: 50px; font-weight: 700; line-height: 1; letter-spacing: -.025em; color: var(--text); }
	.of { color: var(--muted); font-size: 17px; }
	.note { color: var(--faint); font-size: 15px; }
	.stats { display: flex; flex-wrap: wrap; gap: 14px 34px; margin: 0; }
	.stat { position: relative; min-width: 0; }
	.stat dt { display: flex; align-items: center; gap: 4px; height: 20px; color: var(--faint); font-size: 14px; white-space: nowrap; }
	.stat dd { margin: 3px 0 0; color: var(--text); font-size: 16px; font-weight: 600; white-space: nowrap; }
	.stat dd b { color: color-mix(in srgb, var(--accent) 70%, var(--text)); font-weight: 700; }
	.stat dd small { margin-left: 7px; font-size: 14px; font-weight: 500; }
	.pace { --pace-color: #81d9b2; }
	.pace[data-pace='fast'] { --pace-color: #ffa070; }
	.pace[data-pace='slow'] { --pace-color: #8fb0ff; }
	.pace[data-pace='unknown'] { --pace-color: var(--muted); }
	.pace dd, .pace dd small { color: var(--pace-color); }
	.pace dd .coverage { color: var(--muted); }
	.info { width: 22px; height: 22px; min-height: 22px; border-radius: 6px; }
	.method { position: absolute; z-index: 4; top: calc(100% + 8px); left: 0; width: 440px; max-width: calc(100vw - 64px); padding: 16px 18px; border: 1px solid var(--border); border-radius: 10px; background: var(--surface-raised); box-shadow: 0 10px 28px #0006; color: var(--muted); font-size: 14px; font-weight: 400; line-height: 1.6; white-space: normal; }
	.method p { margin: 0; }
	.method p+p { margin-top: 10px; }
	.legend { display: flex; align-items: center; flex-wrap: wrap; gap: 6px 22px; margin-top: 4px; color: var(--muted); font-size: 14px; }
	.legend-title { color: var(--faint); }
	.key { display: inline-flex; align-items: center; gap: 8px; }
	.key i { display: block; width: 18px; height: 0; border-top: 2px solid var(--accent); }
	.key i.dashed { border-top-style: dashed; }
	.key i.dot { width: 8px; height: 8px; border: 0; border-radius: 50%; }
	.key.account { font-size: 13px; }
	@media(max-width:650px) {
		.pool { padding: 16px 14px 12px; }
		.total strong { font-size: 40px; }
		.stats { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px 16px; width: 100%; }
		.stat dd { white-space: normal; font-size: 15px; }
		.stat dt { font-size: 13px; }
		.method { width: calc(100vw - 60px); }
	}
</style>
