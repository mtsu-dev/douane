<script lang="ts">
	import Plus from '@lucide/svelte/icons/plus';
	import Pencil from '@lucide/svelte/icons/pencil';
	import Trash2 from '@lucide/svelte/icons/trash-2';
	import RotateCcwKey from '@lucide/svelte/icons/rotate-ccw-key';
	import Lock from '@lucide/svelte/icons/lock';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import Info from '@lucide/svelte/icons/info';
	import ChartPie from '@lucide/svelte/icons/chart-pie';
	import ProviderMark from './provider-mark.svelte';
	import EmptyState from './empty-state.svelte';
	import Modal from './modal.svelte';
	import ClientEditor from './client-editor.svelte';
	import ClientKey from './client-key.svelte';
	import ClientStats from './client-stats.svelte';
	import { management, pools as poolsOf, privateLabel, providerName, formatDecimal, resetLabel, formatDate, reserveWindows, reserveWindowLabel, type Account, type Client, type ClientSaved, type ClientsReport, type ClientUsage, type Pool, type ReserveWindow } from '$lib/management';
	import { t } from '$lib/i18n.svelte';
	let { report, accounts, busy, updated }: { report: ClientsReport | null; accounts: Account[]; busy: string; updated: (message: string) => Promise<void> } = $props();
	const noKeyColor = '#d6def0';
	const formerColor = '#9b8ec4';
	const palette = ['#7899ff', '#4dd4ac', '#f4c154', '#c792ea', '#5ccfe6', '#ff9e7a', '#a3e635', '#f78fb3'];
	let clients = $derived(report?.clients ?? []);
	let usage = $derived(report?.usage ?? []);
	let pools = $derived(poolsOf(accounts));
	let editor = $state<{ client: Client | null } | null>(null);
	let shownKey = $state<{ name: string; key: string } | null>(null);
	let confirmation = $state<{ kind: 'rotate' | 'delete'; client: Client } | null>(null);
	let working = $state(false);
	let failure = $state('');
	let colors = $derived(new Map(clients.map((client, index) => [client.id, palette[index % palette.length]])));
	let names = $derived(new Map(clients.map(client => [client.id, client.name])));
	let anonymous = $derived(usage.some(row => (row.clients[''] ?? 0) > 0));
	let former = $derived(usage.some(row => Object.entries(row.clients).some(([id, points]) => id && !names.has(id) && points > 0)));
	function percent(value: number) { return t('common.percent', { value: formatDecimal(value) }); }
	function pool(id: string): Pool { return pools.find(item => item.id === id) ?? { id, provider: id, name: id, accounts: [] }; }
	function accountName(id: string) { const account = accounts.find(item => item.id === id); return account ? privateLabel(account.name) : t('activity.unknownAccount'); }
	function byKind(a: { kind: ReserveWindow }, b: { kind: ReserveWindow }) { return reserveWindows.indexOf(a.kind) - reserveWindows.indexOf(b.kind); }
	/** Keep each account's usage beside its effective share, including pool defaults. */
	function clientUsage(client: Client) {
		return accounts.filter(account => client.access.all || client.access.pools.includes(account.route || account.provider) || client.access.accounts.includes(account.id)).map(account => {
			const item = pool(account.route || account.provider);
			const kinds = usage.filter(row => row.account_id === account.id).map(row => ({ kind: row.kind, points: row.clients[client.id] ?? 0 })).sort(byKind);
			return { account, pool: item, share: client.account_shares?.[account.id] ?? client.shares[item.id] ?? null, kinds };
		}).filter(entry => entry.kinds.length || entry.share != null);
	}
	type Segment = { id: string; label: string; points: number; color: string; kind: 'client' | 'outside' | 'rest' };
	function segments(row: ClientUsage): Segment[] {
		const list: Segment[] = [];
		for (const client of clients) { const points = row.clients[client.id] ?? 0; if (points > 0) list.push({ id: client.id, label: client.name, points, color: colors.get(client.id) ?? palette[0], kind: 'client' }); }
		const anonymousPoints = row.clients[''] ?? 0;
		if (anonymousPoints > 0) list.push({ id: 'no-key', label: t('clients.noKey'), points: anonymousPoints, color: noKeyColor, kind: 'client' });
		const formerPoints = Object.entries(row.clients).reduce((sum, [id, points]) => id && !names.has(id) ? sum + points : sum, 0);
		if (formerPoints > 0) list.push({ id: 'former', label: t('clients.former'), points: formerPoints, color: formerColor, kind: 'client' });
		if (row.outside > 0) list.push({ id: 'outside', label: t('clients.outside'), points: row.outside, color: '#6f80a0', kind: 'outside' });
		const rest = row.used - list.reduce((sum, segment) => sum + segment.points, 0);
		if (rest > 0.05) list.push({ id: 'rest', label: t('clients.unexplained'), points: rest, color: '', kind: 'rest' });
		return list;
	}
	// Accounts with measured windows, under their provider, in the order of the account list.
	let breakdown = $derived.by(() => {
		const groups: { provider: string; name: string; accounts: { id: string; rows: ClientUsage[] }[] }[] = [];
		const seen = new Set<string>();
		for (const item of pools) for (const account of item.accounts) {
			const rows = usage.filter(row => row.account_id === account.id).sort(byKind);
			seen.add(account.id);
			if (!rows.length) continue;
			let group = groups.find(entry => entry.provider === account.provider);
			if (!group) { group = { provider: account.provider, name: providerName(account.provider), accounts: [] }; groups.push(group); }
			group.accounts.push({ id: account.id, rows });
		}
		const unknown = [...new Set(usage.filter(row => !seen.has(row.account_id)).map(row => row.account_id))];
		if (unknown.length) groups.push({ provider: '', name: t('activity.unknownAccount'), accounts: unknown.map(id => ({ id, rows: usage.filter(row => row.account_id === id).sort(byKind) })) });
		return groups;
	});
	function accessSummary(client: Client) {
		const whole = client.access.pools.map(pool);
		const single = accounts.filter(account => client.access.accounts.includes(account.id) && !client.access.pools.includes(account.route || account.provider));
		const missing = client.access.accounts.filter(id => !accounts.some(account => account.id === id));
		return { whole, single, missing };
	}
	function saved(result: ClientSaved, created: boolean) {
		editor = null;
		if (result.key) shownKey = { name: result.client.name, key: result.key };
		void updated(t(created ? 'notice.clientCreated' : 'notice.clientSaved'));
	}
	async function confirm() {
		const current = confirmation;
		if (!current || working) return;
		working = true; failure = '';
		try {
			if (current.kind === 'rotate') {
				const result = await management.updateClient(current.client.id, { rotate_key: true });
				confirmation = null;
				if (result.key) shownKey = { name: result.client.name, key: result.key };
				void updated(t('notice.keyReplaced'));
			} else {
				await management.deleteClient(current.client.id);
				confirmation = null;
				void updated(t('notice.clientDeleted'));
			}
		} catch (error) { failure = (error as Error).message; }
		finally { working = false; }
	}
</script>

<div class="page-heading">
	<div><h1>{t('clients.title')}</h1>
		{#if report}<p class="page-description"><span class="state-badge" title={t('clients.enforcedInfo')}><Lock size={14}/>{t('clients.enforced')}</span></p>{/if}
	</div>
	<div class="heading-actions"><button class="button primary" disabled={!report || !!busy} onclick={() => editor = { client: null }}><Plus size={20}/>{t('clients.new')}</button></div>
</div>

{#if report}<ClientStats {clients} {colors} {noKeyColor} {formerColor} version={report}/>{/if}

{#if report && !clients.length}
	<section class="ledger"><EmptyState title={t('clients.emptyTitle')} description={t('clients.emptyDescription')} action={t('clients.new')} onclick={() => editor = { client: null }}/></section>
{:else if clients.length}
	<div class="section-toolbar"><h2 class="section-title">{t('clients.keys')}</h2></div>
	<div class="client-grid">
		{#each clients as client (client.id)}
			{@const access = accessSummary(client)}
			{@const shares = Object.entries(client.shares)}
			{@const accountShares = Object.entries(client.account_shares ?? {})}
			<article class="client-card" style:--client={colors.get(client.id)}>
				<header>
					<div class="client-title"><span class="client-dot"></span><h2>{client.name}</h2><code title={t('clients.createdAt', { date: formatDate(client.created_at) })}>{client.key_hint}</code></div>
					<div class="card-actions">
						<button class="icon-button" title={t('clients.editTitle', { name: client.name })} aria-label={t('clients.editTitle', { name: client.name })} disabled={!!busy} onclick={() => editor = { client }}><Pencil size={18}/></button>
						<button class="icon-button" title={t('clients.rotate')} aria-label={t('clients.rotateNamed', { name: client.name })} disabled={!!busy} onclick={() => { failure = ''; confirmation = { kind: 'rotate', client }; }}><RotateCcwKey size={18}/></button>
						<button class="icon-button danger" title={t('clients.delete')} aria-label={t('clients.deleteNamed', { name: client.name })} disabled={!!busy} onclick={() => { failure = ''; confirmation = { kind: 'delete', client }; }}><Trash2 size={18}/></button>
					</div>
				</header>
				<div class="access">
					{#if client.access.all}<span class="tag strong">{t('clients.allAccounts')}</span>
					{:else}
						{#each access.whole as item (item.id)}<span class="tag"><ProviderMark provider={item.provider} size={17}/>{item.name}</span>{/each}
						{#each access.single as account (account.id)}<span class="tag"><ProviderMark provider={account.provider} size={17}/>{privateLabel(account.name)}</span>{/each}
						{#each access.missing as id (id)}<span class="tag muted">{t('activity.unknownAccount')}</span>{/each}
						{#if !access.whole.length && !access.single.length && !access.missing.length}<span class="tag muted">{t('clients.noAccess')}</span>{/if}
					{/if}
				</div>
				<div class="shares">
					{#each shares as [id, share] (id)}{@const item = pool(id)}<span class="share-chip" title={t('clients.shareOf', { pool: item.name })}><ChartPie size={15}/>{item.name}<strong>{percent(share)}</strong></span>{/each}
					{#each accountShares as [id, share] (id)}<span class="share-chip" title={t('clients.accountShareOf', { name: accountName(id) })}><ChartPie size={15}/>{accountName(id)}<strong>{percent(share)}</strong></span>{/each}
					{#if !shares.length && !accountShares.length}<span class="muted small">{t('clients.noShare')}</span>{/if}
				</div>
				{#each clientUsage(client) as entry (entry.account.id)}
					<div class="pool-usage">
						<p class="pool-name"><ProviderMark provider={entry.pool.provider} size={18}/>{accountName(entry.account.id)}</p>
						{#each entry.kinds as kind (kind.kind)}
							<div class="usage-line">
								<span class="kind">{reserveWindowLabel(kind.kind)}</span>
								<span class="usage-track"><span class="usage-fill" style:width="{Math.min(100, kind.points)}%"></span>{#if entry.share != null}<span class="share-mark" style:left="{Math.min(100, entry.share)}%"></span>{/if}</span>
								<span class="usage-value tabular" class:over={entry.share != null && kind.points >= entry.share}>{entry.share == null ? percent(kind.points) : t('clients.ofShare', { used: formatDecimal(kind.points), share: formatDecimal(entry.share) })}</span>
							</div>
						{:else}<p class="muted small no-usage">{t('clients.notMeasured')}</p>{/each}
					</div>
				{/each}
			</article>
		{/each}
	</div>

	<div class="section-toolbar"><h2 class="section-title">{t('clients.breakdown')}<span class="info" title={t('clients.breakdownInfo')} aria-label={t('clients.breakdownInfo')}><Info size={16}/></span></h2>
		<div class="legend">
			{#each clients as client (client.id)}<span><i style:background={colors.get(client.id)}></i>{client.name}</span>{/each}
			{#if anonymous}<span><i style:background={noKeyColor}></i>{t('clients.noKey')}</span>{/if}
			{#if former}<span><i style:background={formerColor}></i>{t('clients.former')}</span>{/if}
			<span><i style:background="#6f80a0"></i>{t('clients.outside')}</span>
			<span><i class="rest"></i>{t('clients.unexplained')}</span>
		</div>
	</div>
	{#if !breakdown.length}<section class="ledger"><p class="empty-breakdown">{t('clients.breakdownEmpty')}</p></section>
	{:else}
		<div class="breakdown">
			{#each breakdown as group (group.provider)}
				<section class="ledger">
					<header class="group-heading">{#if group.provider}<ProviderMark provider={group.provider} size={24}/>{/if}<h3>{group.name}</h3></header>
					{#each group.accounts as account (account.id)}
						<div class="account-usage">
							<p class="account-label">{accountName(account.id)}</p>
							<div class="windows">
								{#each account.rows as row (row.kind)}
									<div class="window-row">
										<span class="kind">{reserveWindowLabel(row.kind)}</span>
										<span class="stack" role="img" aria-label={segments(row).map(segment => `${segment.label} ${percent(segment.points)}`).join(', ')}>
											{#each segments(row) as segment (segment.id)}<span class={['segment', segment.kind]} style:width="{Math.min(100, segment.points)}%" style:background={segment.color || undefined} title="{segment.label}: {percent(segment.points)}"></span>{/each}
										</span>
										<span class="used tabular">{t('clients.used', { value: formatDecimal(row.used) })}</span>
										<span class="reset" title={t('quotas.resetAt', { date: formatDate(new Date(row.reset * 1000).toISOString()) })}><RefreshCw size={14}/>{resetLabel(new Date(row.reset * 1000).toISOString())}</span>
									</div>
								{/each}
							</div>
						</div>
					{/each}
				</section>
			{/each}
		</div>
	{/if}
{/if}

{#if editor}<ClientEditor client={editor.client} {clients} {accounts} close={() => editor = null} {saved}/>{/if}
{#if shownKey}<ClientKey name={shownKey.name} key={shownKey.key} close={() => shownKey = null}/>{/if}
{#if confirmation}
	{@const current = confirmation}
	<Modal title={t(current.kind === 'rotate' ? 'clients.rotateTitle' : 'clients.deleteTitle', { name: current.client.name })} close={() => confirmation = null} locked={working}>
		{#if current.kind === 'rotate'}<p class="dialog-text">{t('clients.rotateText')}</p>
		{:else}<p class="dialog-text">{t('clients.deleteText')}</p>{#if clients.length === 1}<p class="dialog-text open-warning">{t('clients.deleteLast')}</p>{/if}{/if}
		{#if failure}<p class="dialog-error" role="alert">{failure}</p>{/if}
		<div class="dialog-actions"><button class="button" disabled={working} onclick={() => confirmation = null}>{t('common.cancel')}</button><button class={['button', current.kind === 'delete' ? 'danger' : 'primary']} disabled={working} onclick={confirm}>{t(current.kind === 'rotate' ? 'clients.rotateButton' : 'clients.deleteButton')}</button></div>
	</Modal>
{/if}

<style>
	.page-description .state-badge { gap: 7px; }
	.client-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); gap: 16px; }
	.client-card { display: flex; flex-direction: column; gap: 14px; padding: 20px 20px 22px; border: 1px solid var(--border); border-top: 3px solid var(--client); border-radius: 14px; background: var(--surface); }
	.client-card header { display: flex; align-items: flex-start; justify-content: space-between; gap: 10px; }
	.client-title { display: flex; flex-wrap: wrap; align-items: center; gap: 6px 10px; min-width: 0; padding-top: 6px; }
	.client-dot { width: 10px; height: 10px; border-radius: 50%; background: var(--client); }
	.client-title h2 { font-family: var(--font-display); font-size: 22px; font-weight: 650; letter-spacing: -.03em; overflow-wrap: anywhere; }
	.client-title code { padding: 2px 7px; border: 1px solid var(--border); border-radius: 6px; background: var(--background); color: var(--muted); font-size: 13px; }
	.card-actions { display: flex; margin: -4px -8px 0 0; }
	.icon-button.danger:hover { color: var(--red); }
	.access, .shares { display: flex; flex-wrap: wrap; gap: 7px; }
	.tag, .share-chip { display: inline-flex; align-items: center; gap: 7px; padding: 4px 10px 4px 7px; border: 1px solid var(--border); border-radius: 8px; font-size: 14px; }
	.tag.strong { padding-left: 10px; border-color: #7899ff40; background: #5478f61c; color: #c4d3ff; }
	.tag.muted { padding-left: 10px; }
	.share-chip { padding-left: 9px; border-color: color-mix(in srgb, var(--client) 45%, transparent); border-radius: 999px; background: color-mix(in srgb, var(--client) 12%, transparent); color: var(--text); }
	.share-chip :global(svg) { color: var(--client); }
	.share-chip strong { color: var(--text); font-weight: 600; }
	.pool-usage { padding-top: 12px; border-top: 1px solid var(--border); }
	.pool-name { display: flex; align-items: center; gap: 8px; margin: 0 0 9px; font-size: 15px; font-weight: 550; }
	.usage-line { display: grid; grid-template-columns: 54px 1fr 74px; align-items: center; gap: 12px; min-height: 26px; }
	.kind { color: var(--muted); font-size: 14px; }
	.usage-track { position: relative; height: 7px; border-radius: 4px; background: #2a3a55; }
	.usage-fill { position: absolute; inset: 0 auto 0 0; border-radius: inherit; background: var(--client); }
	.share-mark { position: absolute; top: -4px; bottom: -4px; width: 2px; margin-left: -1px; border-radius: 1px; background: var(--text); }
	.usage-value { text-align: right; font-size: 14px; }
	.usage-value.over { color: var(--yellow); }
	.no-usage { margin: 0; }
	.section-toolbar { flex-wrap: wrap; margin-top: 38px; }
	.section-title, .info { display: inline-flex; align-items: center; gap: 8px; }
	.section-title { font-family: var(--font-display); font-size: 23px; font-weight: 600; }
	.info { color: var(--faint); cursor: help; }
	.info:hover { color: var(--blue); }
	.legend { display: flex; flex-wrap: wrap; gap: 8px 16px; font-size: 14px; color: var(--muted); }
	.legend span { display: inline-flex; align-items: center; gap: 7px; }
	.legend i { width: 11px; height: 11px; border-radius: 3px; }
	.rest, .segment.rest { background: repeating-linear-gradient(135deg, #55688a 0 3px, transparent 3px 6px); }
	.legend i.rest { outline: 1px solid #55688a; outline-offset: -1px; }
	.breakdown { display: grid; gap: 16px; }
	.group-heading { display: flex; align-items: center; gap: 10px; padding: 14px 20px; border-bottom: 1px solid var(--border); background: #17243b80; }
	.group-heading h3 { margin: 0; font-size: 16px; font-weight: 600; }
	.account-usage { display: grid; grid-template-columns: minmax(130px, 190px) 1fr; gap: 18px; padding: 14px 20px; }
	.account-usage + .account-usage { border-top: 1px solid var(--border); }
	.account-label { margin: 4px 0 0; font-size: 15px; font-weight: 550; overflow-wrap: anywhere; }
	.windows { display: grid; gap: 8px; }
	.window-row { display: grid; grid-template-columns: 54px 1fr 92px 230px; align-items: center; gap: 14px; min-height: 28px; }
	.stack { display: flex; height: 12px; overflow: hidden; border-radius: 4px; background: #2a3a55; }
	.segment { height: 100%; }
	.segment + .segment { box-shadow: inset 2px 0 0 var(--surface); }
	.used { font-size: 14px; text-align: right; }
	.reset { display: flex; align-items: center; gap: 6px; color: var(--muted); font-size: 14px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
	.reset :global(svg) { flex-shrink: 0; color: var(--faint); }
	.empty-breakdown { margin: 0; padding: 22px 20px; color: var(--muted); font-size: 15px; }
	.open-warning { color: var(--yellow); }
	@media(max-width:900px) { .account-usage { grid-template-columns: 1fr; gap: 10px; } }
	@media(max-width:650px) { .client-grid { grid-template-columns: 1fr; } .window-row { grid-template-columns: 48px 1fr 80px; } .reset { grid-column: 2/-1; margin-top: -4px; } .account-usage, .group-heading { padding-inline: 16px; } }
</style>
