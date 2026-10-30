<script lang="ts">
	import { t } from '$lib/i18n.svelte';
	let { value, label, compact = false }: { value: number | null; label?: string; compact?: boolean } = $props();
	let text = $derived(label ?? t('meter.left'));
	let percent = $derived(value == null ? null : Math.min(100, Math.max(0, value)));
</script>

<div class="quota-meter" class:compact>
	<div class="meter-label"><span>{text}</span><span class="tabular">{percent == null ? t('meter.unavailable') : t('common.percent', { value: Math.round(percent) })}</span></div>
	<div class="meter-track" role="meter" aria-label={text} aria-valuenow={percent ?? undefined} aria-valuemin="0" aria-valuemax="100" aria-valuetext={percent == null ? t('meter.unavailable') : t('meter.valueLeft', { value: Math.round(percent) })}>
		{#if percent != null}<div class="meter-fill" class:low={percent < 25} class:medium={percent >= 25 && percent < 60} style:width="{percent}%"></div>{/if}
	</div>
</div>

<style>
	.meter-label { display: flex; justify-content: space-between; gap: 14px; font-size: 15px; color: var(--muted); margin-bottom: 9px; }
	.meter-label span:last-child { color: var(--text); }
	.meter-label span:last-child { white-space: nowrap; }
	.meter-track { height: 7px; background: #2a3a55; border-radius: 4px; overflow: hidden; }
	.meter-fill { height: 100%; background: var(--green); border-radius: inherit; }
	.meter-fill.medium { background: var(--yellow); }
	.meter-fill.low { background: var(--red); }
	.compact .meter-label { display: none; }
	.compact .meter-track { height: 5px; }
</style>
