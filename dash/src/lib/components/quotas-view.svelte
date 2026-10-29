<script lang="ts">
	import QuotaForecast from './quota-forecast.svelte';
	import Search from '@lucide/svelte/icons/search';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import Plus from '@lucide/svelte/icons/plus';
	import ExternalLink from '@lucide/svelte/icons/external-link';
	import SlidersHorizontal from '@lucide/svelte/icons/sliders-horizontal';
	import Star from '@lucide/svelte/icons/star';
	import ProviderMark from './provider-mark.svelte';
	import QuotaMeter from './quota-meter.svelte';
	import AccountName from './account-name.svelte';
	import PrivateEmail from './private-email.svelte';
	import AccountResets from './account-resets.svelte';
	import EmptyState from './empty-state.svelte';
	import { providerName, formatDate, resetLabel, creditValue, usageLabel, privateLabel, accountStatus, accountWarning, type Account, type Quota, type QuotaWindow, type Settings, type QuotaDisplay, type QuotaWindowChoice, type Provider } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { accounts, quotas, forecasts = [], providers = [], busy, refresh, connect, rename, resetUpdated, settings, setQuotaDisplay, setQuotaWindows }: { accounts: Account[]; quotas: Quota[]; forecasts?: import('$lib/management').WindowForecast[]; providers?: Provider[]; busy: string; refresh: (id?: string) => void; connect: () => void; rename: (account: Account, name: string) => Promise<void>; resetUpdated: () => Promise<void>; settings: Settings | null; setQuotaDisplay: (provider: string, display: QuotaDisplay) => Promise<void>; setQuotaWindows: (provider: string, choice: QuotaWindowChoice) => Promise<void> } = $props();
	let provider = $state('all');
	let search = $state('');
	let sort = $state('name');
	let windowsOpen = $state<string | null>(null);
	const order = ['claude', 'codex', 'opencode_go', 'xai'];
	// Providers that report no quota (custom endpoints, Qwen) have nothing to show here.
	let unreported = $derived(new Set(providers.filter(p => !p.quota).map(p => p.id)));
	let providerIds = $derived([...new Set(accounts.map(a => a.provider).filter(id => !unreported.has(id)))].sort((a, b) => (order.indexOf(a) < 0 ? 99 : order.indexOf(a)) - (order.indexOf(b) < 0 ? 99 : order.indexOf(b)) || providerName(a).localeCompare(providerName(b))));
	let filtered = $derived(accounts.filter(a => (provider === 'all' || !providerIds.includes(provider) || a.provider === provider) && `${a.name} ${a.email ?? ''}`.toLowerCase().includes(search.toLowerCase())).sort((a,b) => sort === 'remaining' ? (remaining(a.id) ?? 101) - (remaining(b.id) ?? 101) : a.name.localeCompare(b.name)));
	let groups = $derived(providerIds.map(id => ({ id, accounts: filtered.filter(a => a.provider === id) })).filter(group => group.accounts.length));
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
	function remaining(id: string) { const values = quotas.find(q => q.account_id === id)?.windows.map(w => w.remaining_percent).filter((v): v is number => v != null) ?? []; return values.length ? Math.min(...values) : null; }
</script>

<div class="page-heading">
	<h1>{t('nav.quotas')}</h1>
	<div class="heading-actions"><button class="button" disabled={!!busy || !accounts.length} onclick={() => refresh()}><RefreshCw size={18} class={busy === 'quotas' ? 'spinning' : ''}/>{t('quotas.refresh')}</button><button class="button primary" onclick={connect}><Plus size={20}/>{t('common.connectAccount')}</button></div>
</div>

{#if providerIds.length > 1}
	<div class="provider-tabs" aria-label={t('quotas.filter')}>
		<button class:active={provider === 'all'} onclick={() => provider = 'all'} aria-pressed={provider === 'all'}>{t('quotas.all')} <span class="count-badge">{accounts.filter(a => providerIds.includes(a.provider)).length}</span></button>
		{#each providerIds as id (id)}<button class:active={provider === id} onclick={() => provider = id} aria-pressed={provider === id}><ProviderMark provider={id} size={22}/>{providerName(id)}<span class="count-badge">{accounts.filter(a => a.provider === id).length}</span></button>{/each}
	</div>
{/if}

{#if accounts.some(a => a.enabled && providerIds.includes(a.provider))}<QuotaForecast accounts={accounts.filter(a => providerIds.includes(a.provider))} {quotas} {forecasts} global/>{/if}

<div class="section-toolbar quota-toolbar"><label class="search-input"><Search size={18}/><input type="search" placeholder={t('quotas.search')} aria-label={t('common.searchAccount')} bind:value={search}/></label><select aria-label={t('quotas.sort')} bind:value={sort}><option value="name">{t('quotas.byName')}</option><option value="remaining">{t('quotas.byRemaining')}</option></select></div>

{#if !accounts.length}<section class="ledger"><EmptyState title={t('common.noAccountConnected')} description={t('quotas.emptyDescription')} action={t('common.connectAccount')} onclick={connect}/></section>
{:else if !filtered.length}<section class="ledger"><EmptyState title={t('common.noMatchingAccount')} description={t('common.tryOtherName')}/></section>
{:else}
	<div class="provider-groups">
		{#each groups as group (group.id)}
			<section class="provider-group" aria-label={t('quotas.group', { provider: providerName(group.id) })}>
				<header class="provider-heading"><ProviderMark provider={group.id} size={29}/><h2>{providerName(group.id)}</h2><span>{t(group.accounts.length === 1 ? 'common.accountCount.one' : 'common.accountCount.other', { count: group.accounts.length })}</span>{#if settings}<label class="display-choice" title={t('quotas.displayTitle', { provider: providerName(group.id) })}><span>{t('quotas.displayLabel')}</span><select value={display(group.id)} disabled={!!busy} onchange={(event) => chooseDisplay(group.id, event.currentTarget)}><option value="accounts">{t('quotas.perAccount')}</option><option value="average">{t('quotas.average')}</option></select></label><button class="button small windows-toggle" aria-expanded={windowsOpen === group.id} aria-controls="windows-{group.id}" title={t('quotas.windowsTitle', { provider: providerName(group.id) })} onclick={() => windowsOpen = windowsOpen === group.id ? null : group.id}><SlidersHorizontal size={16}/>{t('quotas.windows')}</button>{/if}</header>
					{#if settings && windowsOpen === group.id}
						{@const current = choice(group.id)}
						<div class="window-choices" id="windows-{group.id}" aria-label={t('quotas.windowsLabel', { provider: providerName(group.id) })}>
							{#each windowsOf(group.id) as window (window.id)}
								{@const primary = current.primary === window.id}
								{@const shown = primary || !current.hidden.includes(window.id)}
								<div class="window-choice" class:hidden-window={!shown}>
									<span class="window-name">{usageLabel(window.label)}</span>
									<button class="primary-choice" class:on={primary} aria-pressed={primary} disabled={!!busy} onclick={() => choosePrimary(group.id, primary ? null : window.id)}><Star size={16}/>{t(primary ? 'quotas.primary' : 'quotas.makePrimary')}</button>
									<button class="switch" class:on={shown} role="switch" aria-checked={shown} aria-label={t('quotas.showWindow', { label: usageLabel(window.label) })} disabled={!!busy || primary} onclick={() => toggleShown(group.id, window.id)}><span></span></button>
								</div>
							{:else}<p class="quota-unavailable">{t('quotas.noWindow')}</p>{/each}
							{#if !current.primary && windowsOf(group.id).length}<p class="window-default">{t('quotas.defaultWindow')}</p>{/if}
						</div>
					{/if}
				<QuotaForecast accounts={accounts.filter(a => a.provider === group.id)} {quotas} {forecasts}/>
				<div class="ledger">
					{#each group.accounts as account (account.id)}
						{@const quota = quotas.find(q => q.account_id === account.id)}
						{@const subscription = quota?.subscription ?? account.subscription}
						<article class="quota-row" class:disabled-row={!account.enabled}>
							<div class="account-identity"><AccountName {account} {rename} {busy}/>{#if subscription?.label || account.plan}<p class="account-plan">{subscription?.label ?? account.plan}</p>{/if}{#if account.email}<PrivateEmail email={account.email}/>{/if}<AccountResets {account} {busy} updated={resetUpdated}/></div>
							<div class="quota-details">
								{#if quota?.verification_required}<div class="verification-needed"><p>{t('quotas.googleVerify')}</p>{#if quota.verification_available}<a class="button" href={`/admin/api/accounts/${encodeURIComponent(account.id)}/verification`} target="_blank" rel="noopener noreferrer" data-sveltekit-reload>{t('common.verifyGoogle')}<ExternalLink size={16}/></a>{:else}<p>{t('common.openAntigravity')}</p>{/if}</div>{/if}
								<div class="quota-windows">{#if quota?.windows.length}{#each quota.windows as window (window.id)}<div class="quota-window"><QuotaMeter value={window.remaining_percent} label={usageLabel(window.label)}/><p class="reset-date" title={window.reset_at ? t('quotas.resetAt', { date: formatDate(window.reset_at) }) : undefined}><RefreshCw size={16}/>{resetLabel(window.reset_at)}</p></div>{/each}{:else if !quota?.verification_required}<p class="quota-unavailable">{quota?.error ? privateLabel(quota.error) : t('quotas.notReported')}</p>{/if}</div>
								{#if quota?.credits?.length}<div class="credit-windows">{#each quota.credits as credit (credit.id)}<div class="credit-window"><div><span>{usageLabel(credit.label)}</span><strong>{credit.enabled === false ? t('common.disabled') : credit.unlimited ? t('quotas.unlimited') : creditValue(credit.remaining, credit.unit)}{#if credit.enabled !== false && credit.remaining != null}<span class="muted">{` ${t('quotas.remaining')}`}</span>{/if}</strong></div>{#if credit.used != null}<p>{credit.limit != null ? t('quotas.usedOf', { used: creditValue(credit.used, credit.unit), limit: creditValue(credit.limit, credit.unit) }) : t('quotas.used', { used: creditValue(credit.used, credit.unit) })}</p>{/if}{#if credit.reset_at}<p class="reset-date"><RefreshCw size={16}/>{resetLabel(credit.reset_at)}</p>{/if}</div>{/each}</div>{/if}
							</div>
							<div class="account-state"><span class="state-badge" class:warning={!!quota?.error || accountWarning(account) || (account.enabled && !!account.reserve_reached)} class:inactive={!account.enabled}><span class="status-pip"></span>{!account.enabled ? t('common.disabled') : quota?.error ? t('status.check') : account.reserve_reached && ['ready', 'active'].includes(account.status) ? t('status.reserveReached') : accountStatus(account)}</span><button class="icon-button" disabled={!!busy} onclick={() => refresh(account.id)} title={t('quotas.refreshAccount')} aria-label={t('quotas.refreshNamed', { name: privateLabel(account.name) })}><RefreshCw size={19}/></button></div>
						</article>
					{/each}
				</div>
			</section>
		{/each}
	</div>
{/if}

<style>
	.quota-toolbar { justify-content: flex-end; margin: 20px 0 26px; }
	.quota-toolbar>select { background: var(--surface); font-size: 15px; }
	.provider-groups { display: grid; gap: 30px; }
	.provider-heading { display: flex; align-items: center; gap: 12px; margin: 0 0 14px; }
	.provider-heading h2 { font-family: var(--font-display); font-size: 23px; font-weight: 600; }
	.provider-heading>span { margin-left: auto; font-size: 15px; color: var(--muted); }
	.display-choice { display: flex; align-items: center; gap: 10px; margin-left: 10px; font-size: 15px; color: var(--muted); }
	.display-choice select { background: var(--surface); font-size: 15px; }
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
	.quota-row { display: grid; grid-template-columns: minmax(220px,.8fr) minmax(320px,1.8fr) auto; gap: 26px; padding: 25px 24px; border-bottom: 1px solid var(--border); align-items: center; }
	.quota-row:last-child { border-bottom: 0; }
	.account-identity { min-width: 0; }
	.account-plan { color: var(--muted); font-size: 15px; margin: 4px 0; }
	.quota-details { min-width: 0; }
	.verification-needed { display: grid; justify-items: start; gap: 12px; margin-bottom: 16px; }
	.verification-needed p { margin: 0; color: var(--muted); font-size: 16px; line-height: 1.5; }
	.quota-windows { display: grid; grid-template-columns: repeat(2,minmax(130px,1fr)); gap: 23px 28px; }
	.reset-date { display: flex; align-items: center; gap: 7px; font-size: 15px; color: var(--muted); margin: 11px 0 0; line-height: 1.4; }
	.reset-date :global(svg) { flex-shrink: 0; color: var(--faint); }
	.quota-unavailable { grid-column: 1/-1; font-size: 16px; color: var(--muted); margin: 0; }
	.account-state { display: flex; align-items: center; justify-content: flex-end; gap: 8px; }
	.disabled-row { opacity: .65; }
	.credit-windows { display: grid; gap: 18px; margin-top: 23px; padding-top: 18px; border-top: 1px solid var(--border); }
	.credit-window>div { display: flex; justify-content: space-between; gap: 12px; font-size: 15px; }
	.credit-window strong { font-weight: 600; }
	.credit-window strong>span { font-weight: 400; }
	.credit-window>p { margin: 8px 0 0; color: var(--muted); font-size: 15px; }
	@media(max-width:1250px) { .quota-row { grid-template-columns: minmax(190px,.7fr) minmax(260px,1.3fr); gap: 22px; }.account-state { grid-column: 1/-1; margin-top: -8px; } }
	@media(max-width:650px) { .quota-toolbar { flex-wrap: nowrap; margin: 18px 0 25px; gap: 10px; }.quota-toolbar>select { max-width: 140px; padding: 0 8px; }.quota-toolbar>.search-input { min-width: 0; width: auto; }.provider-groups { gap: 28px; }.provider-heading { flex-wrap: wrap; }.provider-heading h2 { font-size: 22px; }.display-choice { flex-basis: 100%; justify-content: space-between; margin-left: 0; }.display-choice select { max-width: 200px; padding: 0 8px; }.windows-toggle { margin-left: auto; }.window-choices { padding: 6px 14px; }.window-choice { gap: 10px; }.primary-choice { padding: 0 8px; }.quota-row { grid-template-columns: 1fr; gap: 22px; padding: 20px 18px; }.quota-windows { grid-template-columns: 1fr; gap: 23px; }.account-state { grid-column: auto; justify-content: space-between; margin-top: 0; }.credit-window>div { flex-wrap: wrap; } }
</style>
