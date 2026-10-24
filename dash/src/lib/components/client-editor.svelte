<script lang="ts">
	import { untrack } from 'svelte';
	import Info from '@lucide/svelte/icons/info';
	import Modal from './modal.svelte';
	import ProviderMark from './provider-mark.svelte';
	import { management, pools as poolsOf, privateLabel, formatDecimal, type Account, type Client, type ClientSaved } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { client, clients, accounts, close, saved }: { client: Client | null; clients: Client[]; accounts: Account[]; close: () => void; saved: (result: ClientSaved, created: boolean) => void } = $props();
	// The draft starts from the client as it was when the dialog opened; refreshes do not reset it.
	const initial = untrack(() => $state.snapshot(client));
	let name = $state(initial?.name ?? '');
	let all = $state(initial?.access.all ?? true);
	let chosenPools = $state<string[]>([...(initial?.access.pools ?? [])]);
	let chosenAccounts = $state<string[]>([...(initial?.access.accounts ?? [])]);
	let shares = $state<Record<string, string>>(Object.fromEntries(Object.entries(initial?.shares ?? {}).map(([pool, value]) => [pool, String(value)])));
	let accountShares = $state<Record<string, string>>(Object.fromEntries(Object.entries(initial?.account_shares ?? {}).map(([id, value]) => [id, String(value)])));
	let saving = $state(false);
	let failure = $state('');
	let pools = $derived(poolsOf(accounts));
	let reachable = $derived(pools.filter(pool => all || chosenPools.includes(pool.id) || pool.accounts.some(account => chosenAccounts.includes(account.id))));
	function allowed(account: Account) { return all || chosenPools.includes(account.route || account.provider) || chosenAccounts.includes(account.id); }
	function draftShare(value: string | undefined) { const number = Number((value ?? '').trim().replace(',', '.')); return Number.isFinite(number) && number > 0 ? number : 0; }
	function assigned(account: Account) {
		const pool = account.route || account.provider;
		const mine = draftShare(accountShares[account.id]?.trim() ? accountShares[account.id] : shares[pool]);
		return clients.reduce((sum, other) => {
			if (other.id === initial?.id || !(other.access.all || other.access.pools.includes(pool) || other.access.accounts.includes(account.id))) return sum;
			return sum + (other.account_shares?.[account.id] ?? other.shares[pool] ?? 0);
		}, mine);
	}
	function togglePool(id: string) { chosenPools = chosenPools.includes(id) ? chosenPools.filter(pool => pool !== id) : [...chosenPools, id]; }
	function toggleAccount(id: string) { chosenAccounts = chosenAccounts.includes(id) ? chosenAccounts.filter(account => account !== id) : [...chosenAccounts, id]; }
	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (saving) return;
		failure = '';
		const poolIds = all ? [] : pools.filter(pool => chosenPools.includes(pool.id)).map(pool => pool.id);
		const accountIds = all ? [] : accounts.filter(account => chosenAccounts.includes(account.id) && !poolIds.includes(account.route || account.provider)).map(account => account.id);
		if (!all && !poolIds.length && !accountIds.length) { failure = t('clients.pickAccess'); return; }
		function parse(draft: Record<string, string>): Record<string, number> | null {
			const next: Record<string, number> = {};
			for (const [id, input] of Object.entries(draft)) {
				const text = (input ?? '').trim().replace(',', '.');
				if (!text) continue;
				const value = Number(text);
				if (!/^\d+(\.\d+)?$/.test(text) || value <= 0 || value > 100) return null;
				next[id] = value;
			}
			return next;
		}
		const nextShares = parse(shares), nextAccountShares = parse(accountShares);
		if (!nextShares || !nextAccountShares) { failure = t('clients.shareInvalid'); return; }
		const patch = { name: name.trim(), access: { all, pools: poolIds, accounts: accountIds }, shares: nextShares, account_shares: nextAccountShares };
		saving = true;
		try { saved(initial ? await management.updateClient(initial.id, patch) : await management.createClient(patch), !initial); }
		catch (error) { failure = (error as Error).message; }
		finally { saving = false; }
	}
</script>

<Modal title={initial ? t('clients.editTitle', { name: initial.name }) : t('clients.new')} {close} locked={saving} wide>
	<form class="client-form" onsubmit={submit}>
		<label class="field"><span>{t('clients.name')}</span><input bind:value={name} required maxlength="64" placeholder={t('clients.namePlaceholder')} autocomplete="off"/></label>
		<section>
			<h3>{t('clients.access')}</h3>
			<div class="all-row"><div><strong>{t('clients.allAccounts')}</strong><span>{t('clients.allAccountsHint')}</span></div><button type="button" class={['switch', all && 'on']} role="switch" aria-checked={all} aria-label={t('clients.allAccounts')} onclick={() => all = !all}><span></span></button></div>
			{#if !all}
				<div class="pool-list">
					{#each pools as pool (pool.id)}
						{@const whole = chosenPools.includes(pool.id)}
						<div class="pool">
							<label class="pool-row" title={t('clients.wholePool', { pool: pool.name })}><input type="checkbox" checked={whole} onchange={() => togglePool(pool.id)}/><ProviderMark provider={pool.provider} size={20}/><strong>{pool.name}</strong></label>
							<div class="account-chips">{#each pool.accounts as account (account.id)}{@const on = whole || chosenAccounts.includes(account.id)}<button type="button" class={['chip', on && 'on']} aria-pressed={on} disabled={whole} onclick={() => toggleAccount(account.id)}>{privateLabel(account.name)}</button>{/each}</div>
						</div>
					{/each}
				</div>
			{/if}
		</section>
		<section>
			<h3>{t('clients.shares')}<span class="info" title={t('clients.sharesInfo')} aria-label={t('clients.sharesInfo')}><Info size={16}/></span></h3>
			{#if reachable.length}
				<div class="share-list">
					{#each reachable as pool (pool.id)}
						<div class="share-group">
							<div class="share-row pool-default"><span class="share-pool"><ProviderMark provider={pool.provider} size={22}/>{pool.name}</span><label class="share-input"><input bind:value={shares[pool.id]} inputmode="decimal" placeholder="0" aria-label={t('clients.shareOf', { pool: pool.name })}/>%</label><span class="share-common">{t('clients.providerDefault')}</span></div>
							{#each pool.accounts.filter(allowed) as account (account.id)}
								{@const total = assigned(account)}
								{@const competing = total > 100 + 1e-9}
								<div class="share-row account-share">
									<span class="share-account" title={privateLabel(account.name)}>{privateLabel(account.name)}</span>
									<label class="share-input"><input bind:value={accountShares[account.id]} inputmode="decimal" placeholder={draftShare(shares[pool.id]) ? String(draftShare(shares[pool.id])) : '0'} title={t('clients.accountInherit')} aria-label={t('clients.accountShareOf', { name: privateLabel(account.name) })}/>%</label>
									<span class={['share-common', competing && 'over']} title={competing ? t('clients.overbookedInfo') : t('clients.accountInherit')}>{competing ? t('clients.overbooked', { value: formatDecimal(total) }) : t('clients.common', { value: formatDecimal(Math.max(0, 100 - total)) })}</span>
								</div>
							{/each}
						</div>
					{/each}
				</div>
			{:else}<p class="muted small">{t('clients.noPool')}</p>{/if}
		</section>
		{#if failure}<p class="dialog-error" role="alert">{failure}</p>{/if}
		<div class="dialog-actions"><button type="button" class="button" disabled={saving} onclick={close}>{t('common.cancel')}</button><button class="button primary" disabled={saving || !name.trim()}>{initial ? t('common.save') : t('clients.create')}</button></div>
	</form>
</Modal>

<style>
	.client-form { display: grid; gap: 22px; }
	.client-form .dialog-error, .client-form :global(.dialog-actions) { margin-top: 0; }
	.field { display: grid; gap: 7px; font-size: 15px; color: var(--muted); }
	.field input { width: 100%; }
	h3 { display: flex; align-items: center; gap: 8px; margin: 0 0 10px; font-size: 16px; font-weight: 600; }
	.info { display: inline-flex; color: var(--faint); cursor: help; }
	.info:hover { color: var(--blue); }
	.all-row { display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 13px 15px; border: 1px solid var(--border); border-radius: 11px; background: var(--background); }
	.all-row strong { display: block; font-size: 15px; font-weight: 600; }
	.all-row span { font-size: 14px; color: var(--muted); }
	.pool-list { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; margin-top: 10px; }
	.pool { display: grid; align-content: start; gap: 8px; padding: 10px 12px 12px; border: 1px solid var(--border); border-radius: 11px; background: var(--background); }
	.pool:has(.pool-row input:checked) { border-color: #5478f670; }
	.pool-row { display: flex; align-items: center; gap: 9px; min-height: 28px; font-size: 15px; cursor: pointer; }
	.pool-row strong { min-width: 0; overflow: hidden; font-weight: 600; text-overflow: ellipsis; white-space: nowrap; }
	.account-chips { display: flex; flex-wrap: wrap; gap: 6px; }
	.chip { padding: 4px 10px; border: 1px solid var(--border); border-radius: 999px; background: transparent; color: var(--muted); font-size: 14px; }
	.chip:hover:not(:disabled) { border-color: var(--blue); color: var(--text); }
	.chip.on { border-color: var(--blue); background: #5478f622; color: var(--text); }
	.chip:disabled { cursor: default; opacity: .75; }
	input[type='checkbox'] { width: 17px; height: 17px; padding: 0; accent-color: var(--primary); }
	.share-list { display: grid; border: 1px solid var(--border); border-radius: 11px; background: var(--background); }
	.share-row { display: grid; grid-template-columns: 1fr auto 120px; align-items: center; gap: 14px; padding: 9px 14px; }
	.share-group + .share-group { border-top: 1px solid var(--border); }
	.pool-default { background: #5478f608; }
	.account-share { padding-left: 46px; }
	.share-account { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-size: 14px; }
	.share-pool { display: flex; align-items: center; gap: 10px; min-width: 0; font-size: 15px; font-weight: 550; }
	.share-input { display: flex; align-items: center; gap: 7px; color: var(--muted); font-size: 15px; }
	.share-input input { width: 74px; height: 38px; text-align: right; background: var(--surface); }
	.share-common { text-align: right; font-size: 14px; color: var(--muted); font-variant-numeric: tabular-nums; }
	.share-common.over { color: var(--blue); }
	@media(max-width:650px) { .pool-list { grid-template-columns: 1fr; } .share-row { grid-template-columns: 1fr auto; } .share-common { grid-column: 1/-1; text-align: left; margin-top: -4px; } }
</style>
