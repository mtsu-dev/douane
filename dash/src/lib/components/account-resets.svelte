<script lang="ts">
	import { untrack } from 'svelte';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import X from '@lucide/svelte/icons/x';
	import { management, privateLabel, formatDate, newResetRequestId, type Account, type ResetAvailability } from '$lib/management';
	import { t, type StringKey } from '$lib/i18n.svelte';
	let { account, busy, updated }: { account: Account; busy: string; updated: () => Promise<void> } = $props();
	let availability = $state.raw<ResetAvailability | null>(null);
	let checking = $state(false);
	let saving = $state(false);
	let failure = $state('');
	let result = $state('');
	let pending = $state(false);
	let confirmation = $state('');
	let requestId = '';
	let dialog = $state<HTMLDialogElement>();
	let supported = $derived(account.enabled && ['claude', 'codex'].includes(account.provider));
	let storageKey = $derived(`douane-reset-${account.id}`);
	let canReset = $derived(availability?.status === 'ready' && (availability.available_count ?? 0) > 0);
	async function check() {
		if (!supported || checking) return;
		checking = true; failure = '';
		try { availability = await management.accountResets(account.id); if (availability.pending_request_id) { sessionStorage.setItem(storageKey, availability.pending_request_id); pending = true; } }
		catch (error) { failure = privateLabel((error as Error).message); }
		finally { checking = false; }
	}
	$effect(() => { if (supported) untrack(() => { pending = !!sessionStorage.getItem(storageKey); void check(); }); else availability = null; });
	function open() {
		requestId = availability?.pending_request_id ?? sessionStorage.getItem(storageKey) ?? newResetRequestId();
		failure = ''; result = ''; confirmation = ''; dialog?.showModal();
	}
	async function redeem() {
		if (saving || busy || confirmation !== 'reset' || !dialog?.open) return;
		saving = true; failure = '';
		try {
			sessionStorage.setItem(storageKey, requestId); pending = true;
			const response = await management.redeemReset(account.id, requestId);
			const messages: Record<string, StringKey> = { reset: 'resets.reset', already_redeemed: 'resets.alreadyRedeemed', nothing_to_reset: 'resets.nothingToReset', no_credit: 'resets.noCredit' };
			const message = messages[response.outcome];
			if (!message) throw new Error(t('resets.unconfirmed'));
			result = t(message);
			sessionStorage.removeItem(storageKey); pending = false;
			await check(); await updated();
		} catch (error) { failure = privateLabel((error as Error).message); }
		finally { saving = false; }
	}
</script>

{#if supported}
	<div class="reset-control">
		{#if canReset || pending}<button class="button reset-button" disabled={!!busy || checking} onclick={open}><RefreshCw size={17}/>{pending ? t('resets.resume') : t('resets.button', { count: availability?.available_count ?? 0 })}</button>
		{:else if checking}<span class="reset-status">{t('resets.checking')}</span>
		{:else if availability?.status === 'ready'}<span class="reset-status">{t('resets.none')}</span>
		{:else if availability?.status !== 'unsupported'}<button class="reset-retry" disabled={!!busy} onclick={check}>{t('resets.check')}</button>{/if}
		{#if failure && !dialog?.open}<p class="reset-error" role="alert">{failure}</p>{/if}
		{#if availability?.error && !failure && availability.status !== 'unsupported'}<p class="reset-error">{privateLabel(availability.error)}</p>{/if}
	</div>
	<dialog bind:this={dialog} oncancel={(event) => { if (saving) event.preventDefault(); }}>
		<div class="reset-dialog">
			<header><h2>{t('resets.title')}</h2><button class="icon-button" aria-label={t('resets.closeConfirmation')} disabled={saving} onclick={() => dialog?.close()}><X size={22}/></button></header>
			<p class="reset-account">{privateLabel(account.name)}</p>
			{#if result}<p class="reset-result" role="status">{result}</p><div class="dialog-actions"><button class="button primary" onclick={() => dialog?.close()}>{t('common.close')}</button></div>
			{:else}
				<p>{t('resets.warning')}</p>
				{#if availability?.next_expires_at}<p>{t('resets.expiresFirst', { date: formatDate(availability.next_expires_at) })}</p>{/if}
				{#if pending}<p>{t('resets.pending')}</p>{/if}
				{#if failure}<p class="reset-error" role="alert">{failure}</p>{/if}
				<form onsubmit={(event) => { event.preventDefault(); void redeem(); }}>
					<label class="reset-confirmation">{t('resets.typeConfirmation')}<input bind:value={confirmation} required autocomplete="off" autocapitalize="none" spellcheck={false} disabled={saving}/></label>
					<div class="dialog-actions"><button type="button" class="button" disabled={saving} onclick={() => dialog?.close()}>{t('common.cancel')}</button><button type="submit" class="button primary" disabled={saving || !!busy || confirmation !== 'reset'}>{saving ? t('resets.running') : pending ? t('resets.retrySame') : t('resets.use')}</button></div>
				</form>
			{/if}
		</div>
	</dialog>
{/if}

<style>
	.reset-confirmation { display: grid; gap: 10px; font-size: 15px; margin-top: 20px; }.reset-confirmation input { width: 100%; }
	.reset-control { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 15px; }
	.reset-button { font-size: 15px; padding: 8px 11px; min-height: 42px; }
	.reset-status { font-size: 15px; color: var(--muted); }
	.reset-retry { padding: 4px 0; border: 0; background: transparent; color: var(--blue); font-size: 15px; }
	.reset-error { flex-basis: 100%; font-size: 15px; color: var(--red); line-height: 1.5; margin: 6px 0; overflow-wrap: anywhere; }
	dialog { max-width: min(520px,calc(100vw - 32px)); width: 100%; padding: 0; border: 1px solid var(--border); border-radius: 16px; background: var(--surface); color: var(--text); box-shadow: 0 24px 100px #0008; }
	dialog::backdrop { background: #030711b5; backdrop-filter: blur(5px); }
	.reset-dialog { padding: 25px; }
	.reset-dialog header { display: flex; align-items: center; justify-content: space-between; gap: 15px; }
	.reset-dialog h2 { font-family: var(--font-display); font-size: 24px; }
	.reset-dialog p { font-size: 16px; color: var(--muted); line-height: 1.6; }
	.reset-dialog .reset-account { color: var(--text); font-weight: 600; }
	.reset-dialog .reset-error { color: var(--red); }
	.reset-dialog .reset-result { color: var(--green); }
	.dialog-actions { display: flex; flex-wrap: wrap; justify-content: flex-end; gap: 10px; margin-top: 24px; }
	@media(max-width:650px) { .reset-dialog { padding: 20px; }.dialog-actions .button { flex: 1; }.reset-dialog h2 { font-size: 22px; } }
</style>
