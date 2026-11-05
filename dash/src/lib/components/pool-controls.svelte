<script lang="ts">
	import ChevronLeft from '@lucide/svelte/icons/chevron-left';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import Users from '@lucide/svelte/icons/users';
	import type { FutureUsage, PoolMode } from './quota-pool.svelte';
	import { t } from '$lib/i18n.svelte';
	let { mode = $bindable(), burnHours = $bindable(), future = $bindable(), showAccounts = $bindable(), range, today, shift, error }: {
		mode: PoolMode; burnHours: number; future: FutureUsage; showAccounts: boolean;
		range: string; today: boolean; shift: (weeks: number | null) => void; error: string;
	} = $props();
	const modes: PoolMode[] = ['remaining', 'drain'];
	const windows = [1, 3, 6];
</script>

<div class="pool-controls" role="group" aria-label={t('pool.controls')}>
	<div class="segmented" role="group" aria-label={t('pool.mode')}>
		{#each modes as id (id)}<button type="button" class:active={mode === id} aria-pressed={mode === id} onclick={() => mode = id}>{t(id === 'remaining' ? 'pool.remaining' : 'pool.drain')}</button>{/each}
	</div>
	<div class="segmented labelled" role="group" aria-label={t('pool.burnRate')}>
		<span>{t('pool.burnRate')}</span>
		{#each windows as hours (hours)}<button type="button" class:active={burnHours === hours} aria-pressed={burnHours === hours} onclick={() => burnHours = hours}>{hours}h</button>{/each}
	</div>
	<label class="future"><span>{t('pool.future')}</span><select bind:value={future} aria-label={t('pool.future')}><option value="same">{t('pool.future.same')}</option><option value="day">{t('pool.future.day')}</option><option value="week">{t('pool.future.week')}</option></select></label>
	<button type="button" class="toggle" class:active={showAccounts} aria-pressed={showAccounts} onclick={() => showAccounts = !showAccounts}><Users size={16}/>{t('pool.accounts')}</button>
	<div class="range">
		<span class="range-label tabular">{range}</span>
		<button type="button" class="step" aria-label={t('pool.previous')} title={t('pool.previous')} onclick={() => shift(-1)}><ChevronLeft size={18}/></button>
		<button type="button" class="today" class:active={today} aria-pressed={today} onclick={() => shift(null)}>{t('pool.today')}</button>
		<button type="button" class="step" aria-label={t('pool.next')} title={t('pool.next')} onclick={() => shift(1)}><ChevronRight size={18}/></button>
	</div>
	{#if error}<p class="history-error" role="status">{error}</p>{/if}
</div>

<style>
	.pool-controls { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; margin: -6px 0 22px; font-size: 14px; }
	.segmented, .future select, .toggle, .range button { height: 40px; border: 1px solid var(--border); border-radius: 10px; background: var(--surface); color: var(--muted); font-size: 14px; }
	.segmented { display: inline-flex; align-items: center; gap: 2px; padding: 3px; }
	.segmented button { height: 32px; padding: 0 12px; border: 1px solid transparent; border-radius: 7px; background: transparent; color: var(--muted); font-size: 14px; font-weight: 500; white-space: nowrap; transition: background .18s, color .18s, border-color .18s; }
	.segmented button:hover { color: var(--text); background: var(--surface-hover); }
	.segmented button.active, .toggle.active, .today.active { border-color: #7899ff2b; background: #5478f61c; color: #c7d5ff; }
	.segmented.labelled>span { padding: 0 6px 0 9px; color: var(--faint); white-space: nowrap; }
	.segmented.labelled button { padding: 0 10px; font-variant-numeric: tabular-nums; }
	.future { display: inline-flex; align-items: center; gap: 9px; color: var(--faint); white-space: nowrap; }
	.future select { width: auto; min-height: 40px; padding: 0 10px; color: var(--text); }
	.toggle { display: inline-flex; align-items: center; gap: 8px; padding: 0 13px; font-weight: 500; transition: background .18s, color .18s, border-color .18s; }
	.toggle:hover, .range button:hover { background: var(--surface-hover); color: var(--text); }
	.range { display: inline-flex; align-items: center; gap: 6px; margin-left: auto; }
	.range-label { margin-right: 6px; color: var(--muted); white-space: nowrap; }
	.range .step { display: inline-flex; align-items: center; justify-content: center; width: 40px; padding: 0; }
	.range .today { padding: 0 14px; font-weight: 500; color: var(--text); }
	.history-error { flex-basis: 100%; margin: 0; color: var(--yellow); font-size: 14px; }
	@media(max-width:780px) {
		.pool-controls { gap: 8px; }
		.toggle { order: 1; }
		.labelled, .future { order: 2; }
		.future>span { display: none; }
		.range { order: 3; margin-left: 0; width: 100%; }
		.history-error { order: 4; }
		.range-label { margin-right: auto; }
	}
</style>
