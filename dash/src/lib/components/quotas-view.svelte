<script lang="ts">
	import QuotaForecast from './quota-forecast.svelte';
	import Search from '@lucide/svelte/icons/search';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import ProviderMark from './provider-mark.svelte';
	import QuotaMeter from './quota-meter.svelte';
	import PrivateEmail from './private-email.svelte';
	import AccountResets from './account-resets.svelte';
	import EmptyState from './empty-state.svelte';
	import { providerName, formatDate, resetLabel, creditValue, usageLabel, privateLabel, accountStatus, accountWarning, type Account, type Quota, type Settings, type Provider } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { accounts, quotas, forecasts = [], providers = [], busy, resetUpdated, settings, manageAccounts }: { accounts: Account[]; quotas: Quota[]; forecasts?: import('$lib/management').WindowForecast[]; providers?: Provider[]; busy: string; resetUpdated: () => Promise<void>; settings: Settings | null; manageAccounts: () => void } = $props();
	let provider = $state('all');
	let search = $state('');
	let sort = $state('name');
	const order = ['claude', 'codex', 'opencode_go', 'xai'];
	// Providers that report no quota (custom endpoints, Qwen) have nothing to show here.
	let unreported = $derived(new Set(providers.filter(p => !p.quota).map(p => p.id)));
	let providerIds = $derived([...new Set(accounts.map(a => a.provider).filter(id => !unreported.has(id)))].sort((a, b) => (order.indexOf(a) < 0 ? 99 : order.indexOf(a)) - (order.indexOf(b) < 0 ? 99 : order.indexOf(b)) || providerName(a).localeCompare(providerName(b))));
	let filtered = $derived(accounts.filter(a => (provider === 'all' || !providerIds.includes(provider) || a.provider === provider) && `${a.name} ${a.email ?? ''}`.toLowerCase().includes(search.toLowerCase())).sort((a,b) => sort === 'remaining' ? (remaining(a.id) ?? 101) - (remaining(b.id) ?? 101) : a.name.localeCompare(b.name)));
	let groups = $derived(providerIds.map(id => ({ id, accounts: filtered.filter(a => a.provider === id) })).filter(group => group.accounts.length));
	function remaining(id: string) { const values = quotas.find(q => q.account_id === id)?.windows.map(w => w.remaining_percent).filter((v): v is number => v != null) ?? []; return values.length ? Math.min(...values) : null; }
</script>

<div class="page-heading"><h1>{t('nav.quotas')}</h1></div>

{#if providerIds.length > 1}
	<div class="provider-tabs" aria-label={t('quotas.filter')}>
		<button class:active={provider === 'all'} onclick={() => provider = 'all'} aria-pressed={provider === 'all'}>{t('quotas.all')} <span class="count-badge">{accounts.filter(a => providerIds.includes(a.provider)).length}</span></button>
		{#each providerIds as id (id)}<button class:active={provider === id} onclick={() => provider = id} aria-pressed={provider === id}><ProviderMark provider={id} size={22}/>{providerName(id)}<span class="count-badge">{accounts.filter(a => a.provider === id).length}</span></button>{/each}
	</div>
{/if}

<div class="section-toolbar quota-toolbar"><label class="search-input"><Search size={18}/><input type="search" placeholder={t('quotas.search')} aria-label={t('common.searchAccount')} bind:value={search}/></label><select aria-label={t('quotas.sort')} bind:value={sort}><option value="name">{t('quotas.byName')}</option><option value="remaining">{t('quotas.byRemaining')}</option></select></div>

{#if !accounts.length}<section class="ledger"><EmptyState title={t('common.noAccountConnected')} description={t('quotas.emptyDescription')} action={t('nav.accounts')} onclick={manageAccounts}/></section>
{:else if !filtered.length}<section class="ledger"><EmptyState title={t('common.noMatchingAccount')} description={t('common.tryOtherName')}/></section>
{:else}
	<div class="provider-groups">
		{#each groups as group (group.id)}
			<section class="provider-group" aria-label={t('quotas.group', { provider: providerName(group.id) })}>
				<header class="provider-heading"><ProviderMark provider={group.id} size={25}/><h2>{providerName(group.id)}</h2><span>{t(group.accounts.length === 1 ? 'common.accountCount.one' : 'common.accountCount.other', { count: group.accounts.length })}</span></header>
				{#if settings?.show_provider_pace && accounts.some(a => a.provider === group.id && a.enabled)}<QuotaForecast accounts={accounts.filter(a => a.provider === group.id)} {quotas} {forecasts} provider={group.id}/>{/if}
				<div class="ledger">
					{#each group.accounts as account (account.id)}
						{@const quota = quotas.find(q => q.account_id === account.id)}
						{@const subscription = quota?.subscription ?? account.subscription}
						<article class="quota-row" class:disabled-row={!account.enabled}>
							<div class="account-identity"><h3 class="account-title">{privateLabel(account.name)}</h3>{#if subscription?.label || account.plan}<p class="account-plan">{subscription?.label ?? account.plan}</p>{/if}{#if account.email}<PrivateEmail email={account.email}/>{/if}<AccountResets {account} {busy} updated={resetUpdated}/></div>
							<div class="quota-details">
								{#if quota?.verification_required}<p class="verification-needed">{t('quotas.googleVerify')} <a href="#accounts">{t('nav.accounts')}</a></p>{/if}
								<div class="quota-windows">{#if quota?.windows.length}{#each quota.windows as window (window.id)}<div class="quota-window"><QuotaMeter value={window.remaining_percent} label={usageLabel(window.label)}/><p class="reset-date" title={window.reset_at ? t('quotas.resetAt', { date: formatDate(window.reset_at) }) : undefined}><RefreshCw size={16}/>{resetLabel(window.reset_at)}</p></div>{/each}{:else if !quota?.verification_required}<p class="quota-unavailable">{quota?.error ? privateLabel(quota.error) : t('quotas.notReported')}</p>{/if}</div>
								{#if quota?.credits?.length}<div class="credit-windows">{#each quota.credits as credit (credit.id)}<div class="credit-window"><div><span>{usageLabel(credit.label)}</span><strong>{credit.enabled === false ? t('common.disabled') : credit.unlimited ? t('quotas.unlimited') : creditValue(credit.remaining, credit.unit)}{#if credit.enabled !== false && credit.remaining != null}<span class="muted">{` ${t('quotas.remaining')}`}</span>{/if}</strong></div>{#if credit.used != null}<p>{credit.limit != null ? t('quotas.usedOf', { used: creditValue(credit.used, credit.unit), limit: creditValue(credit.limit, credit.unit) }) : t('quotas.used', { used: creditValue(credit.used, credit.unit) })}</p>{/if}{#if credit.reset_at}<p class="reset-date"><RefreshCw size={16}/>{resetLabel(credit.reset_at)}</p>{/if}</div>{/each}</div>{/if}
							</div>
							<div class="account-state"><span class="state-badge" class:warning={!!quota?.error || accountWarning(account) || (account.enabled && !!account.reserve_reached)} class:inactive={!account.enabled}><span class="status-pip"></span>{!account.enabled ? t('common.disabled') : quota?.error ? t('status.check') : account.reserve_reached && ['ready', 'active'].includes(account.status) ? t('status.reserveReached') : accountStatus(account)}</span></div>
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
	@media(max-width:650px) { .quota-toolbar { flex-wrap: nowrap; margin: 16px 0 24px; gap: 10px; }.quota-toolbar>select { max-width: 140px; padding: 0 8px; }.quota-toolbar>.search-input { min-width: 0; width: auto; }.provider-groups { gap: 24px; }.provider-heading h2 { font-size: 22px; }.quota-row { grid-template-columns: 1fr; gap: 18px; padding: 18px; }.quota-windows { grid-template-columns: 1fr; gap: 20px; }.account-state { grid-column: auto; justify-content: flex-start; margin-top: 0; }.credit-window>div { flex-wrap: wrap; } }
</style>
