import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';

let server, View, Resets, render;
before(async () => {
  server = await createServer({ root: fileURLToPath(new URL('../', import.meta.url)), server: { middlewareMode: true }, appType: 'custom', logLevel: 'error' });
  ({ default: View } = await server.ssrLoadModule('/src/lib/components/quotas-view.svelte'));
  ({ default: Resets } = await server.ssrLoadModule('/src/lib/components/account-resets.svelte'));
  ({ render } = await server.ssrLoadModule('svelte/server'));
  const { setLanguage } = await server.ssrLoadModule('/src/lib/i18n.svelte.ts');
  setLanguage('en');
});
after(async () => { await server?.close(); });

function page(options = {}) {
  const { enabled = true, known = true, current = 50 } = options;
  const pace = 'pace' in options ? options.pace : true;
  return render(View, { props: {
    accounts: [{ id: 'a', provider: 'claude', name: 'A', status: 'ready', enabled }, { id: 'b', provider: 'codex', name: 'B', status: 'ready', enabled }],
    quotas: ['a', 'b'].map(account_id => ({ account_id, updated_at: '2026-10-30T12:00:00Z', windows: [{ id: 'weekly', label: 'Weekly', remaining_percent: current, reset_at: '2026-10-31T12:00:00Z' }] })),
    forecasts: known ? ['a', 'b'].map(account_id => ({ account_id, window_id: 'weekly', rate_per_hour: 3, observed_seconds: 3600, remaining_at_reset: 0, exhausts_at: '2026-10-31T04:40:00Z' })) : [],
    busy: '', settings: { show_provider_pace: pace }, resetUpdated() {}, manageAccounts() {}
  } }).body;
}
const tags = (body, role) => [...body.matchAll(new RegExp(`<[^>]+role="${role}"[^>]*>`, 'g'))].map(([tag]) => tag);

test('provider pace is hidden unless enabled in settings', () => {
  for (const pace of [false, undefined]) {
    assert.equal(tags(page({ pace }), 'meter').filter(tag => tag.includes('pace')).length, 0);
    assert.equal(tags(page({ pace }), 'meter').length, 2);
  }
});

test('quotas only display data and resets; account management lives elsewhere', () => {
  const body = page();
  assert.doesNotMatch(body, /Connect an account|In Boite \/ clients|Rename account|Refresh quotas|Refresh A|Windows<\/button>/);
});

test('reset requires an explicit typed confirmation before its submit button is enabled', () => {
  const body = render(Resets, { props: { account: { id: 'reset-test', name: 'A', provider: 'claude', enabled: true }, busy: '', updated: async () => {} } }).body;
  assert.match(body, /Type reset to confirm/);
  assert.match(body, /<input[^>]+required/);
  assert.match(body, /<button[^>]+disabled[^>]*>Use 1 reset<\/button>/);
});

test('one pace line per provider compares consumption with average without repeated window projections', () => {
  const body = page();
  const lines = tags(body, 'meter').filter(tag => tag.includes('pace'));
  assert.equal(lines.length, 2);
  assert.ok(lines.every(tag => tag.includes('aria-valuenow="144"')));
  assert.equal((body.match(/\+44% vs average/g) ?? []).length, 2);
  assert.equal(tags(body, 'meter').length, 4);
  assert.doesNotMatch(body, /Overall pace|At reset \(estimate\)|Run out early:/);
});

test('unknown estimates are visibly partial; disabled accounts retain only their actual quota', () => {
  const body = page({ known: false });
  assert.match(body, /0\/1 accounts measured/);
  assert.ok(tags(body, 'meter').filter(tag => tag.includes('pace')).every(tag => !tag.includes('aria-valuenow')));
  assert.equal(tags(page({ enabled: false }), 'meter').length, 2);
  assert.ok(tags(page({ current: null }), 'meter').every(tag => !tag.includes('aria-valuenow')));
});
