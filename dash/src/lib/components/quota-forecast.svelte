<script lang="ts">
	import ChartNoAxesCombined from '@lucide/svelte/icons/chart-no-axes-combined';
	import Info from '@lucide/svelte/icons/info';
	import { forecastSummary } from '$lib/forecast';
	import type { Account, Quota, WindowForecast } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { accounts, quotas, forecasts, global = false }: { accounts: Account[]; quotas: Quota[]; forecasts: WindowForecast[]; global?: boolean } = $props();
	let summary = $derived(forecastSummary(accounts, quotas, forecasts));
	let methodOpen = $state(false);
</script>

<section class="forecast" class:global aria-label={t(global ? 'forecast.global' : 'forecast.title')}>
	<div class="forecast-heading">
		<ChartNoAxesCombined size={global ? 22 : 18}/>
		<h3>{t(global ? 'forecast.global' : 'forecast.title')}</h3>
		<span class="pace" data-pace={summary.status}>{t(`forecast.${summary.status}`)}</span>
		<button class="info icon-button" aria-label={t('forecast.about')} aria-expanded={methodOpen} title={t('forecast.about')} onclick={() => methodOpen = !methodOpen}><Info size={17}/></button>
	</div>
	{#if methodOpen}<p class="method">{t('forecast.method')}</p>{/if}
	{#if summary.known || summary.missingAccounts}
		<div class="forecast-counts">
			{#if summary.known}
			{#if summary.counts.fast}<span data-pace="fast">{t('forecast.fastCount', { count: summary.counts.fast })}</span>{/if}
			{#if summary.counts.slow}<span data-pace="slow">{t('forecast.slowCount', { count: summary.counts.slow })}</span>{/if}
			{#if summary.counts.balanced}<span>{t('forecast.balancedCount', { count: summary.counts.balanced })}</span>{/if}
			<span class="coverage">{t('forecast.coverage', { known: summary.known, total: summary.total })}</span>
			{/if}
			{#if summary.missingAccounts}<span>{t('forecast.missingAccounts', { count: summary.missingAccounts })}</span>{/if}
		</div>
	{/if}
	{#if !summary.known}<p class="learning">{t('forecast.learning')}</p>{/if}
</section>

<style>
	.forecast { padding: 16px 20px; margin-bottom: 14px; border: 1px solid var(--border); border-radius: 12px; background: var(--surface); }
	.global { margin: 22px 0; padding: 22px 24px; background: linear-gradient(110deg, #5478f612, var(--surface)); }
	.forecast-heading { display: flex; align-items: center; gap: 10px; color: var(--muted); }
	h3 { margin: 0; color: var(--text); font-size: 16px; font-weight: 600; }
	.global h3 { font-family: var(--font-display); font-size: 21px; }
	.pace { margin-left: auto; font-size: 15px; font-weight: 600; }
	[data-pace='fast'], [data-pace='mixed'] { color: #ffba8b; }
	[data-pace='slow'] { color: #9bb9ff; }
	[data-pace='balanced'] { color: #81d9b2; }
	.info { width: 28px; height: 28px; min-height: 28px; }
	.forecast-counts { display: flex; flex-wrap: wrap; gap: 8px 20px; margin-top: 12px; font-size: 15px; color: var(--muted); }
	.coverage { margin-left: auto; color: var(--faint); }
	.learning { margin: 10px 0 0; color: var(--muted); font-size: 15px; }
	.method { margin: 12px 0; color: var(--muted); font-size: 15px; line-height: 1.6; }
	@media(max-width: 620px) {
		.forecast, .global { padding: 15px; }
		.forecast-heading { flex-wrap: wrap; gap: 8px; }
		.pace { margin-left: 0; }
		.info { margin-left: auto; }
		.coverage { margin-left: 0; width: 100%; }
	}
</style>
