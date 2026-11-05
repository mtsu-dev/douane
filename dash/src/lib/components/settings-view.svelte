<script lang="ts">
	import { untrack } from 'svelte';
	import Save from '@lucide/svelte/icons/save';
	import GitBranch from '@lucide/svelte/icons/git-branch';
	import ProviderMark from './provider-mark.svelte';
	import LanguageSetting from './language-setting.svelte';
	import BackupSetting from './backup-setting.svelte';
	import { providerName, type Settings, type Provider, type Strategy, type Account, type Quota } from '$lib/management';
	import { rawWeight } from '$lib/pool';
	import { t } from '$lib/i18n.svelte';
	let { settings, providers, accounts = [], quotas = [], busy, save, restore }: { settings: Settings; providers: Provider[]; accounts?: Account[]; quotas?: Quota[]; busy: string; save: (settings: Settings) => Promise<boolean>; restore: (bundle: unknown, settings: boolean) => Promise<boolean> } = $props();
	let draft = $state<Settings>(untrack(() => ({ share_release: 'last-seventh', ...settings, provider_strategies: { ...settings.provider_strategies }, plan_weights: Object.fromEntries(Object.entries(settings.plan_weights ?? {}).map(([provider, plans]) => [provider, { ...plans }])) })));
	// Every provider and plan among the accounts, with the weight it gets when left empty.
	let plans = $derived.by(() => {
		const found = new Map<string, { provider: string; id: string; label: string; fallback: number }>();
		for (const account of accounts) {
			const subscription = quotas.find(q => q.account_id === account.id)?.subscription ?? account.subscription;
			if (!subscription?.id) continue;
			const key = `${account.provider}/${subscription.id}`;
			if (!found.has(key)) found.set(key, { provider: account.provider, id: subscription.id, label: subscription.label || subscription.id, fallback: rawWeight(account.provider, subscription) });
		}
		return [...found.values()].sort((a, b) => providerName(a.provider).localeCompare(providerName(b.provider)) || a.fallback - b.fallback);
	});
	/** An empty or invalid weight removes the key, so the plan multiplier applies again. */
	function setWeight(provider: string, id: string, value: string) {
		const all = { ...draft.plan_weights }, own = { ...all[provider] };
		const weight = Number(value);
		if (!value.trim() || !(weight > 0)) delete own[id]; else own[id] = Math.min(1000, weight);
		if (Object.keys(own).length) all[provider] = own; else delete all[provider];
		draft.plan_weights = all;
	}
	const strategies: Strategy[] = ['round-robin', 'fill-first', 'reset-first', 'smart'];
	// A routing choice saves on click with every visible edit; a refused save
	// restores the routing in force, so a highlighted strategy is never a draft.
	async function route(change: Partial<Settings>) {
		const inForce = { strategy: draft.strategy, provider_strategies: draft.provider_strategies };
		draft = { ...draft, ...change };
		if (!(await save(draft))) draft = { ...draft, ...inForce };
	}
	function override(provider: string, strategy: string) { const next = { ...draft.provider_strategies }; if (strategy) next[provider] = strategy as Strategy; else delete next[provider]; route({ provider_strategies: next }); }
	async function setPace(show: boolean) {
		const previous = draft.show_provider_pace ?? false;
		draft.show_provider_pace = show;
		if (!(await save(draft))) draft.show_provider_pace = previous;
	}
</script>

<div class="page-heading"><h1>{t('nav.settings')}</h1></div>
<p class="session-note"><GitBranch size={18}/><span>{t('settings.sessionNote')}</span></p>
<form class="settings-form" onsubmit={(event) => { event.preventDefault(); save(draft); }}>
	<section class="settings-section"><div><h2>{t('settings.quotaView')}</h2><p>{t('settings.quotaViewDescription')}</p></div><div class="pace-setting"><div><span id="pace-label">{t('settings.providerPace')}</span><p id="pace-description">{t('settings.providerPaceDescription')}</p></div><button type="button" class="switch" class:on={draft.show_provider_pace} role="switch" aria-labelledby="pace-label" aria-describedby="pace-description" aria-checked={draft.show_provider_pace ?? false} disabled={!!busy} onclick={() => setPace(!draft.show_provider_pace)}><span></span></button></div></section>
	<section class="settings-section"><div><h2>{t('settings.planWeights')}</h2><p>{t('settings.planWeightsDescription')}</p></div><div class="settings-fields provider-fields">{#each plans as plan (`${plan.provider}/${plan.id}`)}<label><span class="provider-label"><ProviderMark provider={plan.provider} size={22}/>{plan.label}</span><input type="number" min="0" max="1000" step="any" inputmode="decimal" placeholder={String(plan.fallback)} value={draft.plan_weights?.[plan.provider]?.[plan.id] ?? ''} aria-label={t('settings.planWeight', { plan: plan.label })} onchange={(event) => setWeight(plan.provider, plan.id, event.currentTarget.value)}/></label>{/each}{#if !plans.length}<p class="muted small">{t('settings.noPlans')}</p>{/if}</div></section>
	<section class="settings-section"><div><h2>{t('settings.routing')}</h2><p>{t('settings.routingDescription')}</p></div><div class="strategy-options">{#each strategies as strategy (strategy)}<label class="strategy-option" class:chosen={draft.strategy === strategy}><input type="radio" name="strategy" value={strategy} checked={draft.strategy === strategy} disabled={!!busy} onchange={() => route({ strategy })}/><span>{t(`strategy.${strategy}.label`)}<small>{t(`strategy.${strategy}.desc`)}</small></span></label>{/each}</div></section>
	<section class="settings-section"><div><h2>{t('settings.perProvider')}</h2><p>{t('settings.perProviderDescription')}</p></div><div class="settings-fields provider-fields">{#each providers as provider (provider.id)}<label><span class="provider-label"><ProviderMark provider={provider.id} size={22}/>{providerName(provider.id, provider.name)}</span><select value={draft.provider_strategies?.[provider.id] ?? ''} disabled={!!busy} onchange={(event) => override(provider.id,event.currentTarget.value)}><option value="">{t('settings.defaultRule')}</option>{#each strategies as strategy (strategy)}<option value={strategy}>{t(`strategy.${strategy}.label`)}</option>{/each}</select></label>{/each}{#if !providers.length}<p class="muted small">{t('settings.noProviders')}</p>{/if}</div></section>
	<section class="settings-section"><div><h2>{t('settings.refresh')}</h2><p>{t('settings.refreshDescription')}</p></div><div class="settings-fields"><label>{t('settings.refreshEvery')}<span class="unit-field"><input type="number" min="30" max="86400" step="1" bind:value={draft.quota_refresh_seconds} required/><span>{t('settings.seconds')}</span></span></label><label>{t('settings.timeout')}<span class="unit-field"><input type="number" min="10" max="1800" step="1" bind:value={draft.request_timeout_seconds} required/><span>{t('settings.seconds')}</span></span></label><label>{t('settings.defaultProvider')}<select bind:value={draft.default_provider}><option value={null}>{t('settings.autoProvider')}</option>{#each providers as provider (provider.id)}<option value={provider.id}>{providerName(provider.id, provider.name)}</option>{/each}</select></label></div></section>
	<section class="settings-section"><div><h2>{t('settings.weekEnd')}</h2><p>{t('settings.weekEndDescription')}</p></div><div class="settings-fields"><label>{t('settings.freeBefore')}<span class="unit-field"><input type="number" min="0" max="168" step="1" bind:value={draft.free_before_reset_hours} required/><span>{t('settings.hours')}</span></span></label><label>{t('settings.shareRelease')}<select bind:value={draft.share_release}><option value="last-seventh">{t('settings.shareRelease.lastSeventh')}</option><option value="before-reset">{t('settings.shareRelease.beforeReset')}</option></select></label><p class="muted small">{t('settings.shareReleaseDescription')}</p></div></section>
	<div class="settings-footer"><button class="button primary" type="submit" disabled={!!busy}><Save size={14}/>{busy === 'settings' ? t('settings.saving') : t('settings.save')}</button></div>
</form>
<BackupSetting {busy} {restore}/>
<LanguageSetting/>

<style>
	.pace-setting { display: flex; align-items: center; gap: 24px; justify-content: space-between; border: 1px solid var(--border); border-radius: 12px; padding: 18px; background: var(--surface); }.pace-setting>div>span { display: block; font-size: 16px; margin-bottom: 6px; }
	.session-note { display: flex; align-items: flex-start; gap: 12px; font-size: 15px; color: var(--muted); line-height: 1.65; margin: 24px 0 0; padding: 16px 18px; border: 1px solid var(--border); border-radius: 12px; background: var(--surface); max-width: 1060px; }
	.session-note :global(svg) { flex-shrink: 0; color: var(--blue); margin-top: 2px; }
	.settings-form { margin-top: 32px; max-width: 1060px; }.settings-section { display: grid; grid-template-columns: 245px 1fr; gap: 34px; border-bottom: 1px solid var(--border); padding: 30px 0; }.settings-section:first-child { padding-top: 0; }.settings-section h2 { font-size: 16px; margin: 0 0 9px; }.settings-section p { color: var(--muted); font-size: 15px; line-height: 1.65; margin: 0; }.strategy-options { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 12px; }.strategy-option { display: flex; gap: 12px; align-items: flex-start; min-height: 126px; padding: 18px 16px; border: 1px solid var(--border); border-radius: 12px; background: var(--surface); cursor: pointer; }.strategy-option:hover { border-color: #3c5279; }.strategy-option.chosen { border-color: var(--blue); background: #5478f613; box-shadow: inset 0 0 0 1px #7899ff22; }.strategy-option input { flex-shrink: 0; width: auto; height: auto; margin: 3px 0 0; accent-color: var(--primary); }.strategy-option span { font-size: 15px; font-weight: 550; line-height: 1.3; }.strategy-option small { display: block; font-size: 15px; font-weight: 400; color: var(--muted); line-height: 1.55; margin-top: 8px; }.settings-fields { display: grid; gap: 20px; }.settings-fields>label { display: grid; grid-template-columns: 1fr 240px; gap: 18px; align-items: center; font-size: 15px; }.provider-fields { gap: 0; border: 1px solid var(--border); border-radius: 12px; background: var(--surface); overflow: hidden; }.provider-fields>label { padding: 13px 16px; border-bottom: 1px solid var(--border); }.provider-fields>label:last-child { border-bottom: 0; }.provider-label { display: flex; align-items: center; gap: 11px; font-size: 15px; }.provider-fields select, .provider-fields input { font-size: 15px; }.unit-field { display: flex; align-items: center; gap: 10px; color: var(--muted); font-size: 15px; }.unit-field input { width: 115px; }.settings-footer { margin-top: 25px; display: flex; justify-content: flex-end; }
	@media(max-width:1150px) { .settings-section { grid-template-columns: 1fr; gap: 22px; }.settings-section>div:first-child { max-width: 600px; } }
	@media(max-width:650px) { .strategy-options { grid-template-columns: 1fr; }.strategy-option { min-height: 0; padding: 16px; }.strategy-option small { font-size: 15px; }.settings-fields>label { grid-template-columns: 1fr; gap: 10px; }.provider-fields>label { grid-template-columns: 1fr 170px; gap: 10px; padding: 13px 12px; }.provider-label { font-size: 15px; gap: 8px; }.provider-fields select { width: 100%; padding: 0 7px; }.settings-footer .button { width: 100%; }.session-note { padding: 15px; } }
</style>
