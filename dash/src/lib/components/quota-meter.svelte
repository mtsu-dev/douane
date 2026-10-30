<script lang="ts">
	import { t } from '$lib/i18n.svelte';
	import { formatDate, formatDecimal, type WindowForecast } from '$lib/management';
	let { value, label, compact = false, forecast }: { value: number | null; label?: string; compact?: boolean; forecast?: WindowForecast | null } = $props();
	let text = $derived(label ?? t('meter.left'));
	let percent = $derived(value == null ? null : Math.min(100, Math.max(0, value)));
	let comparison = $derived(forecast !== undefined);
	let estimate = $derived(percent == null ? null : forecast);
	let projected = $derived(estimate == null ? null : Math.min(100, Math.max(0, estimate.remaining_at_reset)));
</script>

<div class="quota-meter" class:compact class:comparison>
	{#if comparison}<div class="window-label">{text}</div>{/if}
	<div class="meter-label"><span>{comparison ? t('forecast.current') : text}</span><span class="tabular">{percent == null ? t('meter.unavailable') : t('common.percent', { value: Math.round(percent) })}</span></div>
	<div class="meter-track" role="meter" aria-label={comparison ? `${text} / ${t('forecast.current')}` : text} aria-valuenow={percent ?? undefined} aria-valuemin="0" aria-valuemax="100" aria-valuetext={percent == null ? t('meter.unavailable') : t('meter.valueLeft', { value: Math.round(percent) })}>
		{#if percent != null}<div class="meter-fill" class:low={percent < 25} class:medium={percent >= 25 && percent < 60} style:width="{percent}%"></div>{/if}
	</div>
	{#if comparison}
		<div class="projection" class:exhausts={!!estimate?.exhausts_at} title={estimate ? t('forecast.rate', { rate: formatDecimal(estimate.rate_per_hour), hours: formatDecimal(estimate.observed_seconds / 3600) }) : t('forecast.unavailable')}>
			<div class="meter-label"><span>{t('forecast.atReset')}</span><span class="tabular">{projected == null ? t('forecast.pending') : t('forecast.projectedPercent', { value: Math.round(projected) })}</span></div>
			<div class="meter-track" class:unknown={projected == null} role="meter" aria-label={`${text} / ${t('forecast.projected')}`} aria-valuenow={projected ?? undefined} aria-valuemin="0" aria-valuemax="100" aria-valuetext={projected == null ? t('forecast.unavailable') : t('forecast.leftAtReset', { value: Math.round(projected) })}>
				{#if projected != null}<div class="meter-fill" style:width="{projected}%"></div>{/if}
			</div>
			{#if estimate?.exhausts_at}<p class="empty-at">{t('forecast.emptyAt', { date: formatDate(estimate.exhausts_at) })}</p>{/if}
		</div>
	{/if}
</div>

<style>
	.window-label { font-size: 15px; font-weight: 600; margin-bottom: 12px; }
	.meter-label { display: flex; justify-content: space-between; gap: 14px; font-size: 15px; color: var(--muted); margin-bottom: 9px; }
	.meter-label span:last-child { color: var(--text); }
	.meter-label span:last-child { white-space: nowrap; }
	.meter-track { height: 7px; background: #2a3a55; border-radius: 4px; overflow: hidden; }
	.meter-fill { height: 100%; background: var(--green); border-radius: inherit; }
	.meter-fill.medium { background: var(--yellow); }
	.meter-fill.low { background: var(--red); }
	.comparison .meter-label { margin-bottom: 6px; font-size: 14px; }
	.comparison .meter-track { height: 8px; }
	.projection { margin-top: 12px; color: #9bb9ff; }
	.projection .meter-label, .projection .meter-label span:last-child { color: inherit; }
	.projection .meter-fill { background: repeating-linear-gradient(125deg, #9bb9ff 0 4px, #688cdd 4px 7px); }
	.projection.exhausts { color: #ffba8b; }
	.exhausts .meter-track { outline: 1px solid #ffba8b70; }
	.exhausts .meter-fill { background: repeating-linear-gradient(125deg, #ffba8b 0 4px, #c27f55 4px 7px); }
	.unknown { background: repeating-linear-gradient(125deg, #2a3a55 0 4px, #1b2941 4px 7px); }
	.empty-at { margin: 8px 0 0; font-size: 13px; line-height: 1.4; }
	.compact .meter-label { display: none; }
	.compact .meter-track { height: 5px; }
</style>
