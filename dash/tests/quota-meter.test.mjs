import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';

let server, View, render;
before(async () => {
	server = await createServer({ root: fileURLToPath(new URL('../', import.meta.url)), server: { middlewareMode: true }, appType: 'custom', logLevel: 'error' });
	({ default: View } = await server.ssrLoadModule('/src/lib/components/quotas-view.svelte'));
	({ render } = await server.ssrLoadModule('svelte/server'));
	const { setLanguage } = await server.ssrLoadModule('/src/lib/i18n.svelte.ts');
	setLanguage('en');
});
after(async () => { await server?.close(); });

function page({ current = 64, projected = 31, enabled = true, exhausts = null } = {}) {
	return render(View, { props: {
		accounts: [{ id: 'sample', provider: 'claude', name: 'Sample', status: 'ready', enabled }],
		quotas: [{ account_id: 'sample', windows: [{ id: 'five-hour', label: '5 hours', remaining_percent: current, reset_at: '2026-11-01T12:00:00Z' }] }],
		forecasts: projected === null ? [] : [{ account_id: 'sample', window_id: 'five-hour', remaining_at_reset: projected, exhausts_at: exhausts, rate_per_hour: 3, observed_seconds: 3600 }],
		busy: '', settings: null, refresh() {}, connect() {}, rename() {}, resetUpdated() {}, setQuotaDisplay() {}, setQuotaWindows() {}
	} }).body;
}

function meters(body) {
	return [...body.matchAll(/<[^>]+role="meter"[^>]*>/g)].map(([tag]) => ({
		label: tag.match(/aria-label="([^"]+)"/)?.[1],
		value: tag.match(/aria-valuenow="([^"]+)"/)?.[1]
	}));
}

test('each visible quota compares current and projected remaining capacity without opening details', () => {
	assert.deepEqual(meters(page()), [
		{ label: '5 hours / Current', value: '64' },
		{ label: '5 hours / At reset (estimate)', value: '31' }
	]);
});

test('missing estimates remain unknown, while a predicted empty quota is a real zero', () => {
	assert.deepEqual(meters(page({ projected: null })), [
		{ label: '5 hours / Current', value: '64' },
		{ label: '5 hours / At reset (estimate)', value: undefined }
	]);
	const empty = page({ projected: 0, exhausts: '2026-11-01T10:00:00Z' });
	assert.equal(meters(empty)[1].value, '0');
	assert.match(empty, /Empty by/);
});

test('disabled accounts show only the actual quota; unknown quotas cannot show a known projection', () => {
	assert.equal(meters(page({ enabled: false })).length, 1);
	assert.deepEqual(meters(page({ current: null })).map(meter => meter.value), [undefined, undefined]);
});
