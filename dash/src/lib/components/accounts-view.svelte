<script lang="ts">
	import Plus from '@lucide/svelte/icons/plus';
	import Upload from '@lucide/svelte/icons/upload';
	import Trash2 from '@lucide/svelte/icons/trash-2';
	import Search from '@lucide/svelte/icons/search';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import ExternalLink from '@lucide/svelte/icons/external-link';
	import ProviderMark from './provider-mark.svelte';
	import ProviderQuotaSettings from './provider-quota-settings.svelte';
	import AccountName from './account-name.svelte';
	import PrivateEmail from './private-email.svelte';
	import EmptyState from './empty-state.svelte';
	import AccountReserve from './account-reserve.svelte';
	import EndpointEditor from './endpoint-editor.svelte';
	import { providerName, formatDate, privateLabel, accountStatus, accountWarning, type Account, type EndpointPatch, type Quota, type ReserveWindow, type Settings, type Provider, type QuotaDisplay, type QuotaWindowChoice } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { accounts, quotas, settings, providers, busy, connect, importFiles, toggle, remove, rename, setReserve, updateEndpoint, refresh, setQuotaDisplay, setQuotaWindows }: { accounts: Account[]; quotas: Quota[]; settings: Settings | null; providers: Provider[]; busy: string; connect: () => void; importFiles: (files: FileList) => void; toggle: (account: Account) => void; remove: (account: Account) => void; rename: (account: Account, name: string) => Promise<void>; setReserve: (account: Account, reserve: number | null, windows: ReserveWindow[]) => Promise<boolean>; updateEndpoint: (account: Account, patch: EndpointPatch) => Promise<boolean>; refresh: (id?: string) => void; setQuotaDisplay: (provider: string, display: QuotaDisplay) => Promise<void>; setQuotaWindows: (provider: string, choice: QuotaWindowChoice) => Promise<void> } = $props();
	let search = $state('');
	let provider = $state('all');
	let confirmation = $state<string | null>(null);
	let dragging = $state(false);
	let fileInput: HTMLInputElement;
	let providerIds = $derived([...new Set(accounts.map(a => a.provider))]);
	let filtered = $derived(accounts.filter(a => (provider === 'all' || !providerIds.includes(provider) || a.provider === provider) && `${a.name} ${a.provider} ${a.route ?? ''} ${a.email ?? ''}`.toLowerCase().includes(search.toLowerCase())));
	let groups = $derived(providerIds.map(id => ({ id, accounts: filtered.filter(a => a.provider === id) })).filter(group => group.accounts.length));
</script>

<header class="account-toolbar">
	<h1>{t('accounts.title')} <span class="count-badge">{accounts.length}</span></h1>
	<div class="account-filters">
		<label class="search-input"><Search size={17}/><input type="search" placeholder={t('common.searchAccount')} aria-label={t('common.searchAccount')} bind:value={search}/></label>
		<select aria-label={t('quotas.filter')} bind:value={provider}><option value="all">{t('quotas.all')}</option>{#each providerIds as id (id)}<option value={id}>{providerName(id)}</option>{/each}</select>
	</div>
	<div class="account-actions">
		<button class="button refresh-button" disabled={!!busy || !accounts.length} onclick={() => refresh()} title={t('quotas.refresh')} aria-label={t('quotas.refresh')}><RefreshCw size={17} class={busy === 'quotas' ? 'spinning' : ''}/></button>
		<button class="button import-button" disabled={!!busy} onclick={() => fileInput.click()} title={t('accounts.importTitle')} aria-label={t('accounts.importTitle')}><Upload size={17}/><span>{t('common.import')}</span></button>
		<button class="button primary connect-button" onclick={connect} title={t('common.connectAccount')} aria-label={t('common.connectAccount')}><Plus size={17}/><span>{t('accounts.connect')}</span></button>
	</div>
</header>
<input hidden type="file" accept=".json,application/json" multiple bind:this={fileInput} onchange={(event) => { if (event.currentTarget.files?.length) importFiles(event.currentTarget.files); event.currentTarget.value = ''; }}/>
<div class="account-groups" class:dragging role="region" aria-label={t('accounts.dropLabel')} ondragover={(event) => { event.preventDefault(); dragging = true; }} ondragleave={(event) => { if (!event.currentTarget.contains(event.relatedTarget as Node)) dragging = false; }} ondrop={(event) => { event.preventDefault(); dragging = false; if (event.dataTransfer?.files.length && !busy) importFiles(event.dataTransfer.files); }}>
	{#if !accounts.length}<section class="ledger"><EmptyState title={t('common.noAccountConnected')} description={t('accounts.emptyDescription')} action={t('common.connectAccount')} onclick={connect}/></section>
	{:else if !filtered.length}<section class="ledger"><EmptyState title={t('common.noMatchingAccount')} description={t('common.tryOtherName')}/></section>
	{:else}
		{#each groups as group (group.id)}
			<section class="account-group" aria-label={providerName(group.id)}>
				<header class="group-heading"><div class="group-identity"><ProviderMark provider={group.id} size={25}/><h2>{providerName(group.id)}</h2><span class="count-badge">{group.accounts.length}</span></div>{#if !providers.some(p => p.id === group.id && !p.quota)}<ProviderQuotaSettings provider={group.id} {accounts} {quotas} {settings} {busy} {setQuotaDisplay} {setQuotaWindows}/>{/if}</header>
				<div class="ledger"><table><thead><tr><th>{t('accounts.colAccount')}</th><th>{t('accounts.colStatus')}</th><th>{t('accounts.colReserve')}</th><th class="actions-heading">{t('accounts.colActions')}</th></tr></thead><tbody>
					{#each group.accounts as account (account.id)}
						{@const quota = quotas.find(q => q.account_id === account.id)}
						<tr class:disabled-row={!account.enabled}>
							<td class="identity-cell"><AccountName {account} {rename} {busy}/><div class="account-meta">{#if quota?.subscription?.label || account.subscription?.label || account.plan}<span>{quota?.subscription?.label ?? account.subscription?.label ?? account.plan}</span>{/if}{#if account.email}<PrivateEmail email={account.email}/>{/if}</div><p class="verified-at">{t('accounts.verifiedPrefix')}{quota?.updated_at ? formatDate(quota.updated_at) : t('accounts.notVerified')}</p>{#if account.provider === 'openai_compatible'}<EndpointEditor {account} {busy} save={updateEndpoint}/>{/if}{#if account.last_error || quota?.error}<p class="inline-error">{privateLabel(account.last_error || quota?.error || '')}</p>{/if}</td>
							<td class="status-cell"><span class="state-badge" class:inactive={!account.enabled} class:warning={accountWarning(account) || !!quota?.error || !!quota?.verification_required}><span class="status-pip"></span>{accountStatus(account)}</span>{#if quota?.verification_required}{#if quota.verification_available}<a class="verify-link" href={`/admin/api/accounts/${encodeURIComponent(account.id)}/verification`} target="_blank" rel="noopener noreferrer" data-sveltekit-reload>{t('common.verifyGoogle')}<ExternalLink size={14}/></a>{:else}<p class="inline-error">{t('common.openAntigravity')}</p>{/if}{/if}</td>
							<td class="reserve-cell"><AccountReserve {account} {quota} {busy} save={setReserve}/></td>
							<td class="actions-cell"><div class="row-actions"><button class="icon-button" disabled={!!busy} onclick={() => refresh(account.id)} title={t('quotas.refreshAccount')} aria-label={t('quotas.refreshNamed', { name: privateLabel(account.name) })}><RefreshCw size={17}/></button><button class="switch" role="switch" aria-checked={account.enabled} aria-label={t('common.enable', { name: privateLabel(account.name) })} class:on={account.enabled} disabled={!!busy} onclick={() => toggle(account)}><span></span></button><button class="icon-button" aria-label={t('accounts.delete', { name: privateLabel(account.name) })} title={t('accounts.deleteTitle')} disabled={!!busy} onclick={() => confirmation = account.id}><Trash2 size={17}/></button></div>{#if confirmation === account.id}<div class="delete-confirmation"><p>{t('accounts.deleteConfirm')}</p><button class="button danger small" disabled={!!busy} onclick={() => { remove(account); confirmation = null; }}>{t('accounts.deleteButton')}</button><button class="button small" onclick={() => confirmation = null}>{t('common.cancel')}</button></div>{/if}</td>
						</tr>
					{/each}
				</tbody></table></div>
			</section>
		{/each}
	{/if}
</div>

<style>
	.account-toolbar { display: flex; align-items: center; gap: 24px; margin-bottom: 28px; }
	h1 { display: flex; gap: 10px; align-items: center; flex-shrink: 0; margin: 0; } h1 .count-badge { font-size: 15px; }
	.account-filters { display: flex; align-items: center; gap: 8px; flex: 1; min-width: 0; margin-left: auto; }
	.account-filters .search-input { width: auto; min-width: 0; flex: 1; }
	.account-filters select { width: 150px; background: var(--surface); }
	.account-actions { display: flex; align-items: center; gap: 8px; flex-shrink: 0; }
	.account-toolbar .button, .account-filters select, .account-filters .search-input { height: 40px; min-height: 40px; border-radius: 10px; font-size: 14px; }
	.account-toolbar .button { gap: 8px; padding: 0 12px; }
	.account-toolbar .refresh-button { width: 40px; padding: 0; }
	.account-filters input { font-size: 14px; }
	.account-groups { display: grid; gap: 24px; border-radius: 12px; min-height: 120px; transition: outline-color .15s; }.account-groups.dragging { outline: 2px dashed var(--blue); outline-offset: 8px; }
	.group-heading { display: flex; align-items: center; justify-content: space-between; gap: 14px; flex-wrap: wrap; margin-bottom: 12px; }.group-identity { display: flex; gap: 10px; align-items: center; }.group-identity h2 { font-family: var(--font-display); font-size: 21px; }
	table { table-layout: fixed; }th, td { padding: 12px 16px; }th { font-size: 13px; }th:first-child { width: 37%; }th:nth-child(2) { width: 19%; }th:nth-child(3) { width: 25%; }.actions-heading { text-align: right; }td { vertical-align: top; }
	.identity-cell { min-width: 0; }.account-meta { display: flex; flex-wrap: wrap; gap: 2px 12px; align-items: center; font-size: 14px; color: var(--muted); }.account-meta :global(.private-email) { margin: 0; }.verified-at { color: var(--faint); font-size: 12px; margin: 4px 0 0; }.disabled-row { opacity: .65; }
	.reserve-cell :global(.reserve), .reserve-cell :global(.reserve-editor) { margin-top: 0; }.row-actions { display: flex; gap: 9px; align-items: center; justify-content: flex-end; }.row-actions .icon-button { min-width: 30px; width: 30px; min-height: 32px; height: 32px; }.inline-error { overflow-wrap: anywhere; font-size: 13px; color: var(--red); margin: 5px 0 0; }.verify-link { display: flex; gap: 6px; align-items: center; color: var(--blue); font-size: 14px; margin-top: 8px; }
	.delete-confirmation { font-size: 14px; margin-top: 10px; }.delete-confirmation p { margin: 0 0 8px; }.delete-confirmation .button { margin: 4px 0 0; }
	@media(max-width:1150px) { .account-toolbar { display: grid; grid-template-columns: minmax(0,1fr) auto; gap: 16px; }.account-filters { grid-column: 1/-1; grid-row: 2; width: 100%; }.account-actions { grid-column: 2; grid-row: 1; }th:first-child { width: 32%; }th:nth-child(3) { width: 27%; }th,td { padding: 12px; } }
	@media(max-width:850px) { table,tbody { display: block; }thead { display: none; }tr { display: grid; grid-template-columns: minmax(0,1fr) auto; gap: 12px; padding: 16px; border-bottom: 1px solid var(--border); }tr:last-child { border: 0; }td { padding: 0; border: 0; }.identity-cell,.reserve-cell { grid-column: 1; }.status-cell { grid-column: 2; grid-row: 1; }.actions-cell { grid-column: 2; grid-row: 2; align-self: end; }.account-meta { display: grid; }.group-heading { gap: 10px; } }
	@media(max-width:500px) { .account-toolbar { gap: 16px 10px; margin-bottom: 24px; }.account-filters select { width: 120px; padding: 0 8px; }.status-cell { grid-column: 1; grid-row: 2; }.reserve-cell { grid-row: 3; grid-column: 1/-1; }.actions-cell { grid-row: 2; }.verified-at { font-size: 12px; } }
	@media(max-width:500px) { .import-button span { display: none; }.account-toolbar .import-button { width: 40px; padding: 0; } }
	@media(max-width:360px) { .connect-button span { display: none; }.account-toolbar .connect-button { width: 40px; padding: 0; } }
</style>
