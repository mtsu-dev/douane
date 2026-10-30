import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';
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
  const { enabled = true, known = true, current = 50, credits = [] } = options;
  const pace = 'pace' in options ? options.pace : true;
  return render(View, { props: {
    accounts: [{ id: 'a', provider: 'claude', name: 'A', status: 'ready', enabled }, { id: 'b', provider: 'codex', name: 'B', status: 'ready', enabled }],
    quotas: ['a', 'b'].map(account_id => ({ account_id, updated_at: '2026-10-30T12:00:00Z', credits, windows: [{ id: 'weekly', label: 'Weekly', remaining_percent: current, reset_at: known ? '2026-10-31T12:00:00Z' : null }] })),
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

test('one pace line per provider is computed from the current weekly reading, with no history', () => {
  // 50% used with 1 day of 7 left: 50 / (6/7 * 100) = 58% of the sustainable pace.
  const body = page();
  const lines = tags(body, 'meter').filter(tag => tag.includes('pace'));
  assert.equal(lines.length, 2);
  assert.ok(lines.every(tag => tag.includes('aria-valuenow="58"')));
  assert.equal((body.match(/Under-used/g) ?? []).length, 2);
  assert.equal(tags(body, 'meter').length, 4);
  assert.doesNotMatch(body, /Not enough data|Overall pace|At reset \(estimate\)/);
  assert.equal(tags(page({ current: 0 }), 'meter').filter(tag => tag.includes('aria-valuenow="117"')).length, 2);
});

test('the header holds title, provider tabs, search and sort in one toolbar', () => {
  const header = page().match(/<header class="quota-toolbar[\s\S]*?<\/header>/)?.[0] ?? '';
  assert.match(header, /<h1[^>]*>Quotas<\/h1>/);
  assert.match(header, /provider-tabs/);
  assert.match(header, /type="search"/);
  assert.match(header, /<select/);
  // Counts live in each provider heading; the tabs stay short so the line holds them.
  assert.doesNotMatch(header, /count-badge/);
  assert.doesNotMatch(header, /search-input[^"]*filled/);
});

test('the toolbar search overrides the mobile minimum and tabs are re-measured when they resize', () => {
  const source = readFileSync(new URL('../src/lib/components/quotas-view.svelte', import.meta.url), 'utf8');
  // app.css gives every search input min-width: 140px on phones, which pushed the toolbar off a 390px screen.
  assert.match(source, /\.quota-filters \.search-input \{[^}]*min-width: 40px/);
  // A late web font widens the tabs without resizing the toolbar, so the stacking decision must observe the tabs themselves.
  assert.match(source, /new ResizeObserver\(\(\) => tabsVersion\+\+\)/);
});

test('extra usage is hidden when it is off or reports nothing', () => {
  const credit = extra => ({ id: 'extra', label: 'Extra usage', unit: 'USD', ...extra });
  assert.doesNotMatch(page({ credits: [credit({ enabled: false })] }), /Extra usage/);
  assert.doesNotMatch(page({ credits: [credit({ enabled: true })] }), /Extra usage/);
  assert.match(page({ credits: [credit({ enabled: true, remaining: 12 })] }), /Extra usage/);
});

test('unknown estimates are visibly partial; disabled accounts retain only their actual quota', () => {
  const body = page({ known: false });
  assert.match(body, /No reset date yet/);
  assert.ok(tags(body, 'meter').filter(tag => tag.includes('pace')).every(tag => !tag.includes('aria-valuenow')));
  assert.equal(tags(page({ enabled: false }), 'meter').length, 2);
  assert.ok(tags(page({ current: null }), 'meter').every(tag => !tag.includes('aria-valuenow')));
});
