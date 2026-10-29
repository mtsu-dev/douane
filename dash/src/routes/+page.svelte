<script lang="ts">
	import { onMount } from 'svelte';
	import Gauge from '@lucide/svelte/icons/gauge';
	import LayoutDashboard from '@lucide/svelte/icons/layout-dashboard';
	import Users from '@lucide/svelte/icons/users';
	import KeyRound from '@lucide/svelte/icons/key-round';
	import Boxes from '@lucide/svelte/icons/boxes';
	import Activity from '@lucide/svelte/icons/activity';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import Rocket from '@lucide/svelte/icons/rocket';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import Menu from '@lucide/svelte/icons/menu';
	import X from '@lucide/svelte/icons/x';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import QuotasView from '$lib/components/quotas-view.svelte';
	import AccountsView from '$lib/components/accounts-view.svelte';
	import ClientsView from '$lib/components/clients-view.svelte';
	import DataViews from '$lib/components/data-views.svelte';
	import ModelsView from '$lib/components/models-view.svelte';
	import SettingsView from '$lib/components/settings-view.svelte';
	import ConnectDialog from '$lib/components/connect-dialog.svelte';
	import DouaneMark from '$lib/components/douane-mark.svelte';
	import { management, importErrors, type Account, type Quota, type Provider, type Model, type ModelListing, type Call, type Status, type Settings, type QuotaDisplay, type QuotaWindowChoice, type ReserveWindow, type EndpointPatch, type ClientsReport } from '$lib/management';
	import LanguageSetting from '$lib/components/language-setting.svelte';
	import { t, type StringKey } from '$lib/i18n.svelte';
	const groups: { label: StringKey; items: { id: string; label: StringKey; icon: typeof Gauge }[] }[] = [
		{ label:'nav.subscriptions', items:[{ id:'quotas',label:'nav.quotas',icon:Gauge },{ id:'overview',label:'nav.overview',icon:LayoutDashboard },{ id:'accounts',label:'nav.accounts',icon:Users },{ id:'clients',label:'nav.clients',icon:KeyRound },{ id:'models',label:'nav.models',icon:Boxes }] },
		{ label:'nav.service',items:[{ id:'activity',label:'nav.activity',icon:Activity },{ id:'settings',label:'nav.settings',icon:Settings2 },{ id:'quickstart',label:'nav.quickstart',icon:Rocket }] }
	];
	const mobileItems = groups.flatMap(group => group.items).filter(item => ['quotas','accounts','activity','settings'].includes(item.id));
	let screen = $state('quotas');
	let mobileOpen = $state(false);
	let connectOpen = $state(false);
	let accounts = $state.raw<Account[]>([]);
	let quotas = $state.raw<Quota[]>([]);
	let forecasts = $state.raw<import('$lib/management').WindowForecast[]>([]);
	let providers = $state.raw<Provider[]>([]);
	let models = $state.raw<Model[]>([]);
	let modelListings = $state.raw<Record<string, ModelListing>>({});
	let calls = $state.raw<Call[]>([]);
	let summary = $state.raw<Status | null>(null);
	let settings = $state.raw<Settings | null>(null);
	let clients = $state.raw<ClientsReport | null>(null);
	let loading = $state(true);
	let busy = $state('');
	let failure = $state('');
	let notice = $state('');
	let pageTitle = $derived(t(groups.flatMap(g => g.items).find(item => item.id === screen)?.label ?? 'nav.quotas'));
	let loadingRequest: Promise<void> | null = null;
	async function load(silent = false) {
		if (loadingRequest) return loadingRequest;
		if (!silent) loading = true;
		loadingRequest = (async () => {
			const responses = await Promise.allSettled([management.accounts(),management.quotas(),management.providers(),management.models(),management.calls(),management.status(),management.settings(),management.clients()]);
			const errors: string[] = [];
			if (responses[0].status === 'fulfilled') accounts = responses[0].value.accounts; else errors.push(responses[0].reason.message);
			if (responses[1].status === 'fulfilled') { quotas = responses[1].value.quotas; forecasts = responses[1].value.forecasts ?? []; } else { forecasts = []; errors.push(responses[1].reason.message); }
			if (responses[2].status === 'fulfilled') providers = responses[2].value.providers; else errors.push(responses[2].reason.message);
			if (responses[3].status === 'fulfilled') { models = responses[3].value.models; modelListings = responses[3].value.listings ?? {}; } else errors.push(responses[3].reason.message);
			if (responses[4].status === 'fulfilled') calls = responses[4].value.calls; else errors.push(responses[4].reason.message);
			if (responses[5].status === 'fulfilled') summary = responses[5].value; else { summary = null; errors.push(responses[5].reason.message); }
			if (responses[6].status === 'fulfilled') settings = responses[6].value; else errors.push(responses[6].reason.message);
			if (responses[7].status === 'fulfilled') clients = responses[7].value; else errors.push(responses[7].reason.message);
			failure = [...new Set(errors)].join(' ');
			loading = false;
		})();
		try { await loadingRequest; } finally { loadingRequest = null; }
	}
	function navigate(next: string) { screen = next; mobileOpen = false; failure = ''; notice = ''; location.hash = next; window.scrollTo(0,0); }
	function hashChange() { const hash = location.hash.slice(1); if (groups.flatMap(g => g.items).some(item => item.id === hash)) screen = hash; }
	async function mutate(name: string, action: () => Promise<unknown>, message = '') {
		if (busy) return false; busy = name; failure = ''; notice = '';
		try { await action(); if (loadingRequest) await loadingRequest; await load(true); notice = message; return true; }
		catch (error) { failure = (error as Error).message; return false; }
		finally { busy = ''; }
	}
	function refresh(id?: string) { return mutate('quotas', () => management.refreshQuotas(id)); }
	async function setQuotaDisplay(provider: string, display: QuotaDisplay) {
		const next = { ...settings?.quota_display };
		if (display === 'average') next[provider] = display; else delete next[provider];
		await mutate('settings', () => management.settingsSave({ quota_display: next }), t('notice.displaySaved'));
	}
	async function setQuotaWindows(provider: string, choice: QuotaWindowChoice) {
		const next = { ...settings?.quota_windows };
		if (choice.hidden?.length || choice.primary) next[provider] = { hidden: choice.hidden ?? [], primary: choice.primary ?? null }; else delete next[provider];
		await mutate('settings', () => management.settingsSave({ quota_windows: next }), t('notice.windowsSaved'));
	}
	async function rename(account: Account, name: string) {
		if (busy) throw new Error(t('notice.busy'));
		busy = 'rename';
		try { const result = await management.renameAccount(account.id, name); accounts = accounts.map(item => item.id === account.id ? result.account : item); }
		finally { busy = ''; }
	}
	function updateEndpoint(account: Account, patch: EndpointPatch) {
		return mutate('endpoint', () => management.updateEndpoint(account.id, patch), t('notice.endpointSaved'));
	}
	function setReserve(account: Account, reserve: number | null, windows: ReserveWindow[]) {
		return mutate('reserve', () => management.setAccountReserve(account.id, reserve, windows), t(reserve == null ? 'notice.reserveRemoved' : 'notice.reserveSaved'));
	}
	async function importFiles(files: FileList) {
		const selected = Array.from(files);
		await mutate('import',async () => {
			const contents = await Promise.all(selected.map(async file => { if (file.size > 2*1024*1024) throw new Error(t('notice.fileTooLarge', { name: file.name })); try { return { name:file.name,content:JSON.parse(await file.text()) }; } catch { throw new Error(t('notice.invalidJson', { name: file.name })); } }));
			const result = await management.importAccounts(contents);
			if (result.errors?.length) { await load(true); throw new Error(t('notice.partialImport', { count: result.imported, errors: importErrors(result.errors) })); }
			if (!result.imported) throw new Error(t('notice.nothingImported'));
		},t('notice.imported'));
	}
	async function restoreBackup(bundle: unknown, applySettings: boolean) {
		let restored = 0;
		const done = await mutate('restore', async () => {
			const result = await management.restore(bundle, applySettings);
			restored = result.restored;
			if (result.errors?.length) { await load(true); throw new Error(t('notice.partialRestore', { count: result.restored, errors: importErrors(result.errors) })); }
		});
		if (done) notice = t(restored === 1 ? 'notice.restored.one' : 'notice.restored.other', { count: restored });
		return done;
	}
	onMount(() => { hashChange(); void load(); const timer = setInterval(() => { if (!document.hidden && !busy) void load(true); },15_000); return () => clearInterval(timer); });
</script>

<svelte:head><title>{pageTitle} · douane</title><meta name="description" content={t('page.description')}/></svelte:head>
<svelte:window onhashchange={hashChange} onkeydown={(event) => { if (event.key === 'Escape') mobileOpen = false; }}/>
<svelte:document onvisibilitychange={() => { if (!document.hidden) void load(true); }}/>

<div class="app-shell">
	{#if mobileOpen}<button class="nav-backdrop" aria-label={t('nav.closeMenu')} onclick={() => mobileOpen = false}></button>{/if}
	<aside id="main-navigation" class="sidebar" class:mobile-open={mobileOpen}>
		<div class="brand"><span class="brand-symbol"><DouaneMark/></span><span>douane</span><button class="icon-button mobile-close" aria-label={t('nav.closeMenu')} onclick={() => mobileOpen = false}><X size={18}/></button></div>
		<nav aria-label={t('nav.main')}>{#each groups as group (group.label)}<div class="nav-group"><p>{t(group.label)}</p>{#each group.items as item (item.id)}{@const Icon = item.icon}<button class="nav-link" class:active={screen === item.id} aria-current={screen === item.id ? 'page' : undefined} onclick={() => navigate(item.id)}><Icon size={17}/><span>{t(item.label)}</span>{#if item.id === 'accounts' && accounts.length}<span class="nav-count">{accounts.length}</span>{:else if item.id === 'clients' && clients?.clients.length}<span class="nav-count">{clients.clients.length}</span>{/if}</button>{/each}</div>{/each}</nav>
		<div class="sidebar-bottom"><button class="sidebar-help" onclick={() => navigate('quickstart')}>{t('nav.setupTool')}<ChevronRight size={19}/></button></div>
	</aside>
	<div class="workspace">
		<main id="main-content">
			{#if failure}<div class="notice error" role="alert"><span>{failure}</span><button class="button small" onclick={() => load()} disabled={loading}>{t('common.retry')}</button></div>{/if}
			{#if notice}<div class="notice success" role="status">{notice}<button class="icon-button" onclick={() => notice = ''} aria-label={t('page.dismissNotice')}><X size={14}/></button></div>{/if}
			{#if loading && !summary}<div class="loading-state" role="status"><RefreshCw size={18} class="spinning"/><span>{t('page.loading')}</span></div>{/if}
			{#if screen === 'quotas'}<QuotasView {accounts} {quotas} {forecasts} {providers} {busy} {refresh} {rename} {settings} {setQuotaDisplay} {setQuotaWindows} resetUpdated={() => load(true)} connect={() => connectOpen = true}/>
			{:else if screen === 'accounts'}<AccountsView {accounts} {quotas} {busy} {rename} {setReserve} {updateEndpoint} connect={() => connectOpen = true} {importFiles} toggle={(account) => mutate('account',() => management.setAccountEnabled(account.id,!account.enabled),t(account.enabled ? 'notice.accountDisabled' : 'notice.accountEnabled'))} remove={(account) => mutate('account',() => management.deleteAccount(account.id),t('notice.accountDeleted'))}/>
			{:else if screen === 'clients'}<ClientsView report={clients} {accounts} {busy} updated={async (message) => { await load(true); notice = message; }}/>
			{:else if screen === 'settings'}{#if settings}{#key JSON.stringify(settings)}<SettingsView {settings} {providers} {busy} restore={restoreBackup} save={(next) => mutate('settings',() => management.settingsSave(next),t('notice.settingsSaved'))}/>{/key}{:else}<div class="page-heading"><div><h1>{t('nav.settings')}</h1><p class="page-description">{t('page.settingsUnavailable')}</p></div></div><LanguageSetting/>{/if}
			{:else if screen === 'models'}<ModelsView {models} listings={modelListings} {accounts} {busy} {navigate} toggleModel={(model) => mutate('model',() => management.setModelEnabled(model.id,model.enabled === false),t('notice.modelUpdated'))} setModels={(ids, enabled) => mutate('models',() => management.setModelsEnabled(ids,enabled),t('notice.modelsUpdated', { count: ids.length }))} refreshModels={(pool) => mutate(`models:${pool ?? '*'}`,() => management.refreshModels(pool))}/>
			{:else}{#key screen}<DataViews {screen} {summary} {calls} {accounts} clients={clients?.clients ?? []} {navigate}/>{/key}{/if}
		</main>
	</div>
</div>
<nav class="mobile-nav" aria-label={t('nav.mobile')}>{#each mobileItems as item (item.id)}{@const Icon = item.icon}<button class:active={screen === item.id} aria-current={screen === item.id ? 'page' : undefined} onclick={() => navigate(item.id)}><Icon size={19}/><span>{t(item.label)}</span></button>{/each}<button onclick={() => mobileOpen = true} aria-label={t('nav.openMenu')} aria-expanded={mobileOpen} aria-controls="main-navigation"><Menu size={19}/><span>{t('nav.menu')}</span></button></nav>
{#if connectOpen}<ConnectDialog {providers} close={() => connectOpen = false} completed={() => { notice = t('notice.accountConnected'); void load(true); }} {importFiles}/>{/if}
