<script lang="ts">
	import QuotaPace from './quota-pace.svelte';
	import Search from '@lucide/svelte/icons/search';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import ProviderMark from './provider-mark.svelte';
	import QuotaMeter from './quota-meter.svelte';
	import PrivateEmail from './private-email.svelte';
	import AccountResets from './account-resets.svelte';
	import EmptyState from './empty-state.svelte';
	import { providerName, formatDate, resetLabel, creditValue, usageLabel, privateLabel, accountStatus, accountWarning, type Account, type Quota, type Settings, type Provider } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { accounts, quotas, providers = [], busy, refresh, resetUpdated, settings, manageAccounts }: { accounts: Account[]; quotas: Quota[]; providers?: Provider[]; busy: string; refresh: (id?: string) => void; resetUpdated: () => Promise<void>; settings: Settings | null; manageAccounts: () => void } = $props();
	const TOOLBAR_GAP = 20;
	let provider = $state('all');
	let search = $state('');
	let sort = $state('name');
	const order = ['claude', 'codex', 'opencode_go', 'xai'];
	// Providers that report no quota (custom endpoints, Qwen) have nothing to show here.
	let unreported = $derived(new Set(providers.filter(p => !p.quota).map(p => p.id)));
	let providerIds = $derived([...new Set(accounts.map(a => a.provider).filter(id => !unreported.has(id)))].sort((a, b) => (order.indexOf(a) < 0 ? 99 : order.indexOf(a)) - (order.indexOf(b) < 0 ? 99 : order.indexOf(b)) || providerName(a).localeCompare(providerName(b))));
	let filtered = $derived(accounts.filter(a => (provider === 'all' || !providerIds.includes(provider) || a.provider === provider) && `${a.name} ${a.email ?? ''}`.toLowerCase().includes(search.toLowerCase())).sort((a,b) => sort === 'remaining' ? (remaining(a.id) ?? 101) - (remaining(b.id) ?? 101) : a.name.localeCompare(b.name)));
	let groups = $derived(providerIds.map(id => ({ id, accounts: filtered.filter(a => a.provider === id) })).filter(group => group.accounts.length));
	// A disabled or empty paid balance (Claude extra usage off) is noise on a display-only page.
	const shownCredits = (quota?: Quota) => quota?.credits?.filter(c => c.enabled !== false && (c.unlimited || c.remaining != null || !!c.used)) ?? [];
	// Tabs go to their own row only when the line cannot hold them (many providers, or an open search), so none is ever cut.
	let toolbarWidth = $state(0), titleWidth = $state(0), filtersWidth = $state(0), tabs = $state<HTMLElement>(), filters = $state<HTMLElement>();
	// Tab sizes change without the toolbar resizing (Figtree loading late, a provider added, the language switched): observe each tab.
	let tabsVersion = $state(0);
	$effect(() => { void providerIds; if (!tabs) return; const observer = new ResizeObserver(() => tabsVersion++); for (const tab of tabs.children) observer.observe(tab); return () => observer.disconnect(); });
	// The width the tabs need on one line: stacked tabs may wrap, so their offsets no longer give it.
	let tabsWidth = $derived.by(() => { void tabsVersion; if (!tabs?.children.length) return 0; const items = [...tabs.children] as HTMLElement[]; return items.reduce((sum, tab) => sum + tab.offsetWidth, 0) + (items.length - 1) * (parseFloat(getComputedStyle(tabs).columnGap) || 0); });
	// Leaving the search collapses its phone row; during a press that would move the tab under the finger, so wait for the press to end.
	let searchFocused = $state(false), searchLabel = $state<HTMLElement>(), pressing = false;
	const settleSearch = () => searchFocused = !!searchLabel?.contains(document.activeElement);
	const pressEnd = () => { pressing = false; setTimeout(settleSearch); };
	let stacked = $derived(toolbarWidth > 0 && titleWidth + tabsWidth + filtersWidth + 2 * TOOLBAR_GAP > toolbarWidth);
	// The filters' own minimum (collapsed search, sort, refresh): beside the title they overflow it on the narrowest phones, so they then take their own row.
	let filtersNeed = $derived.by(() => { void toolbarWidth; if (!filters) return 0; const items = [...filters.children] as HTMLElement[]; return items.reduce((sum, item) => sum + (item === searchLabel ? 40 : item.offsetWidth), 0) + (items.length - 1) * (parseFloat(getComputedStyle(filters).columnGap) || 0); });
	let filtersRow = $derived(stacked && (searchFocused || !!search || titleWidth + filtersNeed + 16 > toolbarWidth));
	function remaining(id: string) { const values = quotas.find(q => q.account_id === id)?.windows.map(w => w.remaining_percent).filter((v): v is number => v != null) ?? []; return values.length ? Math.min(...values) : null; }
</script>

<svelte:window onpointerdown={() => pressing = true} onpointerup={pressEnd} onpointercancel={pressEnd}/>

<header class="quota-toolbar" class:stacked class:filters-row={filtersRow} bind:clientWidth={toolbarWidth}>
	<h1 bind:clientWidth={titleWidth}>{t('nav.quotas')}</h1>
	{#if providerIds.length > 1}
		<div class="provider-tabs" aria-label={t('quotas.filter')} bind:this={tabs}>
			<button class:active={provider === 'all'} onclick={() => provider = 'all'} aria-pressed={provider === 'all'}>{t('quotas.all')}</button>
			{#each providerIds as id (id)}<button class:active={provider === id} onclick={() => provider = id} aria-pressed={provider === id}><ProviderMark provider={id} size={20}/>{providerName(id)}</button>{/each}
		</div>
	{/if}
	<div class="quota-filters" bind:clientWidth={filtersWidth} bind:this={filters}><label class="search-input" class:filled={!!search} bind:this={searchLabel} onfocusin={() => searchFocused = true} onfocusout={() => { if (!pressing) searchFocused = false; }}><Search size={17}/><input type="search" placeholder={t('quotas.search')} aria-label={t('common.searchAccount')} bind:value={search}/></label><select aria-label={t('quotas.sort')} bind:value={sort}><option value="name">{t('quotas.byName')}</option><option value="remaining">{t('quotas.byRemaining')}</option></select><button class="button refresh-button" disabled={!!busy || !accounts.length} onclick={() => refresh()} title={t('quotas.refresh')} aria-label={t('quotas.refresh')}><RefreshCw size={17} class={busy === 'quotas' ? 'spinning' : ''}/></button></div>
</header>

{#if !accounts.length}<section class="ledger"><EmptyState title={t('common.noAccountConnected')} description={t('quotas.emptyDescription')} action={t('nav.accounts')} onclick={manageAccounts}/></section>
{:else if !filtered.length}<section class="ledger"><EmptyState title={t('common.noMatchingAccount')} description={t('common.tryOtherName')}/></section>
{:else}
	<div class="provider-groups">
		{#each groups as group (group.id)}
			<section class="provider-group" aria-label={t('quotas.group', { provider: providerName(group.id) })}>
				<header class="provider-heading"><ProviderMark provider={group.id} size={25}/><h2>{providerName(group.id)}</h2><span>{t(group.accounts.length === 1 ? 'common.accountCount.one' : 'common.accountCount.other', { count: group.accounts.length })}</span></header>
				{#if settings?.show_provider_pace && accounts.some(a => a.provider === group.id && a.enabled)}<QuotaPace accounts={accounts.filter(a => a.provider === group.id)} {quotas} provider={group.id}/>{/if}
				<div class="ledger">
					{#each group.accounts as account (account.id)}
						{@const quota = quotas.find(q => q.account_id === account.id)}
						{@const subscription = quota?.subscription ?? account.subscription}
						{@const credits = shownCredits(quota)}
						<article class="quota-row" class:disabled-row={!account.enabled}>
							<div class="account-identity"><h3 class="account-title">{privateLabel(account.name)}</h3>{#if subscription?.label || account.plan}<p class="account-plan">{subscription?.label ?? account.plan}</p>{/if}{#if account.email}<PrivateEmail email={account.email}/>{/if}<AccountResets {account} {busy} updated={resetUpdated}/></div>
							<div class="quota-details">
								{#if quota?.verification_required}<p class="verification-needed">{t('quotas.googleVerify')} <a href="#accounts">{t('nav.accounts')}</a></p>{/if}
								<div class="quota-windows">{#if quota?.windows.length}{#each quota.windows as window (window.id)}<div class="quota-window"><QuotaMeter value={window.remaining_percent} label={usageLabel(window.label)}/><p class="reset-date" title={window.reset_at ? t('quotas.resetAt', { date: formatDate(window.reset_at) }) : undefined}><RefreshCw size={16}/>{resetLabel(window.reset_at)}</p></div>{/each}{:else if !quota?.verification_required}<p class="quota-unavailable">{quota?.error ? privateLabel(quota.error) : t('quotas.notReported')}</p>{/if}</div>
								{#if credits.length}<div class="credit-windows">{#each credits as credit (credit.id)}<div class="credit-window"><div><span>{usageLabel(credit.label)}</span><strong>{credit.enabled === false ? t('common.disabled') : credit.unlimited ? t('quotas.unlimited') : creditValue(credit.remaining, credit.unit)}{#if credit.enabled !== false && credit.remaining != null}<span class="muted">{` ${t('quotas.remaining')}`}</span>{/if}</strong></div>{#if credit.used != null}<p>{credit.limit != null ? t('quotas.usedOf', { used: creditValue(credit.used, credit.unit), limit: creditValue(credit.limit, credit.unit) }) : t('quotas.used', { used: creditValue(credit.used, credit.unit) })}</p>{/if}{#if credit.reset_at}<p class="reset-date"><RefreshCw size={16}/>{resetLabel(credit.reset_at)}</p>{/if}</div>{/each}</div>{/if}
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
	.quota-toolbar { display: flex; align-items: center; gap: 20px; margin-bottom: 24px; }
	.quota-toolbar h1 { flex-shrink: 0; margin: 0; }
	.quota-toolbar .provider-tabs { flex: 0 1 auto; min-width: 0; padding: 0; border: 0; }
	.quota-toolbar .provider-tabs>button { padding: 0 10px; height: 40px; font-size: 14px; }
	.quota-toolbar.stacked { display: grid; grid-template-columns: auto minmax(0,1fr); gap: 14px 16px; }
	.quota-toolbar.stacked .provider-tabs { grid-column: 1/-1; grid-row: 2; }
	.quota-toolbar.stacked .quota-filters { max-width: 100%; }
	/* Desktop stacked tabs wrap rather than hide behind a scrollbar a mouse cannot reach; phones keep the swipe strip. */
	@media(min-width:781px) { .quota-toolbar.stacked .provider-tabs { flex-wrap: wrap; overflow: visible; } }
	/* A phone gives an open search its own row instead of squeezing it beside the title and sort. */
	@media(max-width:780px) { .quota-toolbar.filters-row .quota-filters { grid-column: 1/-1; grid-row: 2; margin-left: 0; width: 100%; }.quota-toolbar.filters-row .search-input { flex-grow: 1; }.quota-toolbar.filters-row .provider-tabs { grid-row: 3; } }
	.quota-filters { display: flex; align-items: center; justify-content: flex-end; gap: 8px; flex-shrink: 0; min-width: 0; margin-left: auto; }
	/* Search stays an icon until it is used, so the provider tabs keep the line. */
	/* A flex basis rather than a width lets an open search shrink inside a narrow (phone) toolbar. */
	.quota-filters .search-input { flex: 0 1 40px; width: auto; min-width: 40px; padding: 0 11px; cursor: text; transition: flex-basis .2s; }
	.quota-filters .search-input:focus-within, .quota-filters .search-input.filled { flex-basis: 220px; }
	.quota-filters .search-input input { min-width: 0; }
	.quota-filters .search-input :global(svg) { flex-shrink: 0; }
	.quota-filters .refresh-button { flex-shrink: 0; width: 40px; min-height: 40px; height: 40px; padding: 0; }
	.quota-filters select { flex-shrink: 0; width: 140px; background: var(--surface); }
	.quota-filters select, .quota-filters .search-input { height: 40px; min-height: 40px; border-radius: 10px; font-size: 14px; }
	.quota-filters input { font-size: 14px; }
	.provider-groups { display: grid; gap: 30px; }
	.provider-heading { display: flex; align-items: center; gap: 12px; margin: 0 0 14px; }
	.provider-heading h2 { font-family: var(--font-display); font-size: 23px; font-weight: 600; }
	.provider-heading>span { margin-left: auto; font-size: 15px; color: var(--muted); }
	.quota-row { display: grid; grid-template-columns: minmax(220px,.8fr) minmax(320px,1.8fr) auto; gap: 26px; padding: 25px 24px; border-bottom: 1px solid var(--border); align-items: center; }
	.quota-row:last-child { border-bottom: 0; }
	.account-identity { min-width: 0; }
	.account-plan { color: var(--muted); font-size: 15px; margin: 4px 0; }
	.quota-details { min-width: 0; }
	.verification-needed { display: grid; justify-items: start; gap: 12px; margin-bottom: 16px; }
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
	.account-title { font-size: 17px; font-weight: 600; line-height: 1.45; overflow-wrap: anywhere; }
	@media(max-width:650px) { .quota-filters { gap: 6px; }.quota-filters select { width: 120px; padding: 0 8px; }.provider-groups { gap: 24px; }.provider-heading h2 { font-size: 22px; }.quota-row { grid-template-columns: 1fr; gap: 18px; padding: 18px; }.quota-windows { grid-template-columns: 1fr; gap: 20px; }.account-state { grid-column: auto; justify-content: flex-start; margin-top: 0; }.credit-window>div { flex-wrap: wrap; } }
</style>
