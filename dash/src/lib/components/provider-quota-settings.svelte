<script lang="ts">
	import SlidersHorizontal from '@lucide/svelte/icons/sliders-horizontal';
	import Star from '@lucide/svelte/icons/star';
	import { providerName, usageLabel, type Account, type Quota, type QuotaWindow, type Settings, type QuotaDisplay, type QuotaWindowChoice } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { provider, accounts, quotas, settings, busy, setQuotaDisplay, setQuotaWindows }: { provider: string; accounts: Account[]; quotas: Quota[]; settings: Settings | null; busy: string; setQuotaDisplay: (provider: string, display: QuotaDisplay) => Promise<void>; setQuotaWindows: (provider: string, choice: QuotaWindowChoice) => Promise<void> } = $props();
	let windowsOpen = $state<string | null>(null);
	function display(id: string): QuotaDisplay { return settings?.quota_display?.[id] ?? 'accounts'; }
	async function chooseDisplay(id: string, select: HTMLSelectElement) { await setQuotaDisplay(id, select.value as QuotaDisplay); select.value = display(id); }
	/** Every window this provider's accounts reported, in first-seen order. */
	function windowsOf(id: string): QuotaWindow[] {
		const ids = new Set(accounts.filter(a => a.provider === id).map(a => a.id));
		const seen = new Map<string, QuotaWindow>();
		for (const quota of quotas) if (ids.has(quota.account_id)) for (const window of quota.windows) if (!seen.has(window.id)) seen.set(window.id, window);
		return [...seen.values()];
	}
	function choice(id: string) { const saved = settings?.quota_windows?.[id]; const primary = saved?.primary ?? null; return { hidden: (saved?.hidden ?? []).filter(w => w !== primary), primary }; }
	function toggleShown(id: string, window: string) { const current = choice(id); const hidden = current.hidden.includes(window) ? current.hidden.filter(w => w !== window) : [...current.hidden, window]; return setQuotaWindows(id, { ...current, hidden }); }
	function choosePrimary(id: string, window: string | null) { const current = choice(id); return setQuotaWindows(id, { hidden: current.hidden.filter(w => w !== window), primary: window }); }
</script>

<div class="client-quota-settings">{#if settings}<label class="display-choice" title={t('quotas.displayTitle', { provider: providerName(provider) })}><span>{t('quotas.displayLabel')}</span><select value={display(provider)} disabled={!!busy} onchange={(event) => chooseDisplay(provider, event.currentTarget)}><option value="accounts">{t('quotas.perAccount')}</option><option value="average">{t('quotas.average')}</option></select></label><button class="button small windows-toggle" aria-expanded={windowsOpen === provider} aria-controls="windows-{provider}" title={t('quotas.windowsTitle', { provider: providerName(provider) })} onclick={() => windowsOpen = windowsOpen === provider ? null : provider}><SlidersHorizontal size={16}/>{t('quotas.windows')}</button>{/if}</div>
					{#if settings && windowsOpen === provider}
						{@const current = choice(provider)}
						<div class="window-choices" id="windows-{provider}" aria-label={t('quotas.windowsLabel', { provider: providerName(provider) })}>
							{#each windowsOf(provider) as window (window.id)}
								{@const primary = current.primary === window.id}
								{@const shown = primary || !current.hidden.includes(window.id)}
								<div class="window-choice" class:hidden-window={!shown}>
									<span class="window-name">{usageLabel(window.label)}</span>
									<button class="primary-choice" class:on={primary} aria-pressed={primary} disabled={!!busy} onclick={() => choosePrimary(provider, primary ? null : window.id)}><Star size={16}/>{t(primary ? 'quotas.primary' : 'quotas.makePrimary')}</button>
									<button class="switch" class:on={shown} role="switch" aria-checked={shown} aria-label={t('quotas.showWindow', { label: usageLabel(window.label) })} disabled={!!busy || primary} onclick={() => toggleShown(provider, window.id)}><span></span></button>
								</div>
							{:else}<p class="quota-unavailable">{t('quotas.noWindow')}</p>{/each}
							{#if !current.primary && windowsOf(provider).length}<p class="window-default">{t('quotas.defaultWindow')}</p>{/if}
						</div>
					{/if}

<style>
	.display-choice { display: flex; align-items: center; gap: 10px; font-size: 14px; color: var(--muted); }
	.display-choice select { background: var(--surface); font-size: 14px; max-width: 210px; height: 40px; border-radius: 10px; }
	.windows-toggle { height: 40px; padding: 0 12px; font-size: 14px; }
	.windows-toggle[aria-expanded='true'] { border-color: var(--primary); color: var(--text); }
	.window-choices { display: grid; gap: 4px; margin: 0 0 14px; padding: 10px 16px; border: 1px solid var(--border); border-radius: 12px; background: var(--surface); }
	.window-choice { display: flex; align-items: center; gap: 14px; min-height: 48px; }
	.window-choice+.window-choice { border-top: 1px solid var(--border); }
	.window-name { flex: 1; min-width: 0; font-size: 16px; }
	.hidden-window .window-name { color: var(--muted); }
	.primary-choice { display: inline-flex; align-items: center; gap: 7px; min-height: 36px; padding: 0 11px; border: 1px solid transparent; border-radius: 9px; background: transparent; color: var(--muted); font-size: 15px; }
	.primary-choice:hover:not(:disabled) { border-color: var(--border); color: var(--text); }
	.primary-choice.on { border-color: var(--primary); color: var(--text); }
	.primary-choice.on :global(svg) { color: var(--primary); fill: var(--primary); }
	.window-default { margin: 6px 0 4px; font-size: 15px; color: var(--muted); }

	.client-quota-settings { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
	.window-choices { flex-basis: 100%; margin: 12px 0 0; }
	@media(max-width:650px) { .client-quota-settings { width: 100%; align-items: flex-end; }.display-choice { flex: 1; min-width: 0; flex-wrap: wrap; gap: 6px; }.display-choice span { font-size: 13px; }.display-choice select { width: 100%; max-width: none; }.window-choice { flex-wrap: wrap; gap: 8px; padding: 8px 0; }.window-name { flex-basis: 100%; }.window-choice .switch { margin-left: auto; } }
</style>
