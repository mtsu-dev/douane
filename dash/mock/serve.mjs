// Serves the built dashboard with sample data, for README captures and UI work
// without a running douane or any real account: `pnpm build && pnpm mock`.
// Writes are accepted and ignored, except client keys, kept in memory. Nothing here reaches a provider.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../build/', import.meta.url));
const port = Number(process.env.PORT ?? 5199);
const now = Date.now();
const at = (hours) => new Date(now + hours * 3600_000).toISOString();
const ago = (minutes) => new Date(now - minutes * 60_000).toISOString();

const account = (id, provider, name, extra = {}) => ({
	id, provider, name, email: null, plan: null, subscription: null, enabled: true, status: 'ready',
	last_error: null, reserve_percent: null, reserve_windows: [], reserve_measured: true, reserve_reached: false,
	created_at: ago(60 * 24 * 20), updated_at: ago(30), ...extra
});
const tier = (id, label, multiplier = null) => ({ id, label, multiplier, source: 'provider_usage' });
const accounts = [
	account('acc-claude-home', 'claude', 'Home', { email: 'alex@example.com', subscription: tier('max', 'Claude Max x20', 20), reserve_percent: 5 }),
	account('acc-claude-work', 'claude', 'Work', { email: 'alex.work@example.com', subscription: tier('max', 'Claude Max x5', 5) }),
	account('acc-codex', 'codex', 'ChatGPT', { email: 'alex@example.com', subscription: tier('pro', 'Codex Pro') }),
	account('acc-grok', 'xai', 'SuperGrok', { subscription: tier('supergrok', 'SuperGrok') }),
	account('acc-copilot', 'copilot', 'GitHub', { subscription: tier('pro', 'Copilot Pro') }),
	account('acc-go', 'opencode_go', 'OpenCode Go', { subscription: tier('go', 'OpenCode Go') }),
	account('acc-deepseek', 'openai_compatible', 'DeepSeek', { route: 'deepseek', base_url: 'https://api.deepseek.com/v1', dialect: 'chat', models: [] }),
	account('acc-openrouter', 'openai_compatible', 'OpenRouter', { route: 'openrouter', base_url: 'https://openrouter.ai/api/v1', dialect: 'chat', models: [] })
];
const window = (id, label, remaining_percent, hours) => ({ id, label, remaining_percent, reset_at: at(hours) });
const quota = (account_id, windows, extra = {}) => ({
	account_id, windows, credits: [], subscription: accounts.find((a) => a.id === account_id)?.subscription ?? null,
	updated_at: ago(3), error: null, verification_required: false, verification_available: false, ...extra
});
const quotas = [
	quota('acc-claude-home', [window('five-hour', '5 hours', 64, 2.7), window('weekly', 'Weekly', 71, 61), window('seven-day-opus', 'Opus weekly', 83, 61)]),
	quota('acc-claude-work', [window('five-hour', '5 hours', 34, 1.4), window('weekly', 'Weekly', 22, 110), window('seven-day-opus', 'Opus weekly', 74, 110)]),
	quota('acc-codex', [window('five-hour', '5 hours', 92, 3.9), window('weekly', 'Weekly', 77, 85)], {
		credits: [{ id: 'credits', label: 'Codex credits', used: null, limit: null, remaining: 1250, unit: 'credits', reset_at: null, unlimited: false, enabled: true }]
	}),
	quota('acc-grok', [window('weekly', 'Weekly', 58, 132)]),
	quota('acc-copilot', [window('premium', 'Premium requests', 81, 24 * 15)]),
	quota('acc-go', [window('five-hour', '5 hours', 100, 5), window('weekly', 'Weekly', 88, 70), window('monthly', 'Monthly', 61, 24 * 15)])
];
const providers = [
	['claude', 'Claude', true, true], ['codex', 'Codex', true, true], ['opencode_go', 'OpenCode Go', false, true],
	['xai', 'Grok', true, true], ['copilot', 'GitHub Copilot', true, true], ['antigravity', 'Antigravity', false, true],
	['kimi', 'Kimi', false, true], ['qwen', 'Qwen', false, false], ['openai_compatible', 'OpenAI-compatible', false, false]
].map(([id, name, oauth, quota]) => ({ id, name, oauth, quota, docs_url: 'https://example.com' }));
const model = (provider, id, name, context_length) => ({ id: `${provider}/${id}`, provider, name, context_length, enabled: true });
const models = [
	model('claude', 'claude-opus-5-5', 'Claude Opus 5.5', 1_000_000), model('claude', 'claude-sonnet-5-5', 'Claude Sonnet 5.5', 1_000_000),
	model('claude', 'claude-haiku-5-5', 'Claude Haiku 5.5', 200_000), model('codex', 'gpt-6.1-sol', 'GPT-6.1 Sol', 1_050_000),
	model('codex', 'gpt-6-luna', 'GPT-6 Luna', 1_000_000), model('xai', 'grok-4.7', 'Grok 4.7', 2_000_000),
	model('copilot', 'gpt-4.1', 'GPT-4.1', 128_000), model('opencode_go', 'glm-5.3-flash', 'GLM 5.3 Flash', 200_000),
	{ ...model('openai_compatible', 'deepseek-v4-pro', 'deepseek-v4-pro', 128_000), id: 'deepseek/deepseek-v4-pro' },
	{ ...model('openai_compatible', 'google/gemma-4-31b-it:free', 'google/gemma-4-31b-it:free', 262_000), id: 'openrouter/google/gemma-4-31b-it:free' }
];
const traffic = [
	['claude-opus-5-5', 'claude', 'acc-claude-home', 2896], ['gpt-6.1-sol', 'codex', 'acc-codex', 3888], ['claude-sonnet-5-5', 'claude', 'acc-claude-work', 1239],
	['grok-4.7', 'xai', 'acc-grok', 1346], ['claude-opus-5-5', 'claude', 'acc-claude-home', 5530], ['glm-5.3-flash', 'opencode_go', 'acc-go', 2092],
	['deepseek-v4-pro', 'deepseek', 'acc-deepseek', 1730], ['gpt-4.1', 'copilot', 'acc-copilot', 940], ['claude-haiku-5-5', 'claude', 'acc-claude-work', 612],
	['gpt-6-luna', 'codex', 'acc-codex', 7210], ['claude-opus-5-5', 'claude', 'acc-claude-home', 6399], ['google/gemma-4-31b-it:free', 'openrouter', 'acc-openrouter', 2480]
];
// Client keys: kept in memory, so edits show until the server stops.
const client = (id, name, hint, access, shares, account_shares = {}) => ({ id, name, key_hint: `dk_\u2026${hint}`, created_at: ago(60 * 24 * 6), access: { pools: [], accounts: [], ...access }, shares, account_shares });
const clients = [
	client('cl-desktop', 'Desktop', '9f3a', { all: true }, {}),
	client('cl-family', 'Family', '41c7', { all: false, pools: ['claude'] }, { claude: 25 }),
	client('cl-laptop', 'Laptop', 'b80e', { all: false, pools: ['codex', 'claude'] }, { codex: 40 })
];
// A month of hourly traffic per client, quieter at night; '' predates the keys.
const hourly = [];
for (let back = 24 * 30 - 1; back >= 0; back--) {
	const start = new Date(now - back * 3600_000);
	const hour = start.toISOString().slice(0, 13);
	const day = ((start.getUTCHours() + 2) % 24) >= 9;
	for (const [id, scale, phase] of [['cl-desktop', 900_000, 0], ['cl-family', 260_000, 2], ['cl-laptop', 420_000, 5], ['', 120_000, 1]]) {
		if (id === '' && back < 24 * 12) continue;
		const wave = (Math.sin((back + phase) / 5) + 1.2) * (day ? 1 : 0.15);
		if (wave < 0.25) continue;
		hourly.push({ client_id: id, hour, calls: Math.round(wave * 9), tokens: Math.round(wave * scale) });
	}
}
const clientModels = { 'cl-desktop': [['claude', 'claude-opus-5-5', .62], ['codex', 'gpt-6.1-sol', .3], ['claude', 'claude-haiku-5-5', .08]], 'cl-family': [['claude', 'claude-sonnet-5-5', 1]], 'cl-laptop': [['codex', 'gpt-6-luna', .7], ['claude', 'claude-opus-5-5', .3]], '': [['claude', 'claude-opus-5-5', 1]] };
function clientStats(hours) {
	const since = new Date(now - hours * 3600_000).toISOString();
	const rows = hourly.filter((row) => row.hour >= since.slice(0, 13));
	const totals = [];
	for (const [id, list] of Object.entries(clientModels)) {
		const mine = rows.filter((row) => row.client_id === id);
		if (!mine.length) continue;
		const calls = mine.reduce((sum, row) => sum + row.calls, 0), tokens = mine.reduce((sum, row) => sum + row.tokens, 0);
		for (const [provider, model, part] of list) {
			const input = Math.round(tokens * part * 0.96);
			totals.push({ client_id: id, provider, model, calls: Math.max(1, Math.round(calls * part)), errors: id === 'cl-laptop' && model === 'gpt-6-luna' ? 3 : 0, input_tokens: input, output_tokens: Math.round(tokens * part * 0.04), cache_read_tokens: Math.round(input * 0.81), cache_write_tokens: Math.round(input * 0.05), latency_ms: Math.round(calls * part * 7400), last_at: `${mine.at(-1).hour}:41:07Z` });
		}
	}
	return { since, totals, hours: rows };
}
const unix = (hours) => Math.floor((now + hours * 3600_000) / 1000);
const used = (account_id, kind, hours, total, points, outside = 0) => ({ account_id, kind, reset: unix(hours), used: total, clients: points, outside });
const usage = [
	used('acc-claude-home', 'five-hour', 2.7, 36, { 'cl-desktop': 18, 'cl-family': 6, 'cl-laptop': 7 }, 2),
	used('acc-claude-home', 'weekly', 61, 29, { 'cl-desktop': 14, 'cl-family': 9, 'cl-laptop': 4 }, 1),
	used('acc-claude-work', 'five-hour', 1.4, 66, { 'cl-desktop': 40, 'cl-family': 12, 'cl-laptop': 10 }),
	used('acc-claude-work', 'weekly', 110, 78, { 'cl-desktop': 41, 'cl-family': 21, 'cl-laptop': 9, '': 3 }, 2),
	used('acc-codex', 'five-hour', 3.9, 8, { 'cl-desktop': 3, 'cl-laptop': 5 }),
	used('acc-codex', 'weekly', 85, 23, { 'cl-desktop': 6, 'cl-laptop': 15 }, 1),
	used('acc-go', 'weekly', 70, 12, { 'cl-desktop': 10 }, 2),
	used('acc-go', 'monthly', 24 * 15, 39, { 'cl-desktop': 30 }, 6)
];
const fakeKey = () => `dk_${Array.from({ length: 48 }, () => '0123456789abcdef'[Math.floor(Math.random() * 16)]).join('')}`;
// Individual shares stay within 100%; their sum may exceed it.
function invalidShare(candidate) {
	return [candidate.shares, candidate.account_shares].some(shares => Object.values(shares).some(value => typeof value !== 'number' || !Number.isFinite(value) || value <= 0 || value > 100));
}
function changeClient(method, id, body) {
	if (method === 'POST') {
		const key = fakeKey();
		const created = client(`cl-${Date.now()}`, body.name ?? 'Client', key.slice(-4), body.access ?? { all: true }, body.shares ?? {}, body.account_shares ?? {});
		if (invalidShare(created)) return [400, { error: { message: 'a share must be above 0 and at most 100' } }];
		clients.push(created);
		return [201, { client: created, key }];
	}
	const index = clients.findIndex((c) => c.id === id);
	if (index < 0) return [404, { error: { message: 'client not found' } }];
	if (method === 'DELETE') { clients.splice(index, 1); return [204, null]; }
	const next = { ...clients[index], ...(body.name ? { name: body.name } : {}), ...(body.access ? { access: body.access } : {}), ...(body.shares ? { shares: body.shares } : {}), ...(body.account_shares ? { account_shares: body.account_shares } : {}) };
	if (invalidShare(next)) return [400, { error: { message: 'a share must be above 0 and at most 100' } }];
	let key = null;
	if (body.rotate_key) { key = fakeKey(); next.key_hint = `dk_\u2026${key.slice(-4)}`; }
	clients[index] = next;
	return [200, { client: next, key }];
}
const calls = Array.from({ length: 60 }, (_, i) => {
	const [m, provider, account_id, latency] = traffic[i % traffic.length];
	const failed = i === 17 || i === 41;
	return {
		id: `call-${i}`, started_at: ago(2 + i * 3.5), model: m, provider, account_id, status: failed ? 429 : 200,
		latency_ms: latency + ((i * 137) % 900), input_tokens: 18_000 + ((i * 7919) % 60_000), output_tokens: 300 + ((i * 271) % 4000),
		cache_read_tokens: 120_000 + ((i * 3571) % 400_000), cache_write_tokens: (i * 613) % 9000,
		error: failed ? 'upstream quota or rate limit reached' : null,
		client_id: provider === 'codex' ? 'cl-laptop' : provider === 'claude' ? ['cl-desktop', 'cl-family', 'cl-laptop'][i % 3] : i % 4 === 3 ? null : 'cl-desktop'
	};
});
const summary = {
	version: '0.3.0', uptime_seconds: 86_400 * 3 + 7200, in_flight: 2, requests: 18_412, errors: 97, accounts: accounts.length,
	enabled_accounts: accounts.length, providers: 7, input_tokens: 58_200_000, output_tokens: 13_000_000,
	cache_read_tokens: 1_510_000_000, cache_write_tokens: 21_000_000, client_api_base_url: 'http://127.0.0.1:8787/v1'
};
const settings = {
	strategy: 'smart', quota_refresh_seconds: 300, free_before_reset_hours: 6, request_timeout_seconds: 180, default_provider: null,
	provider_strategies: { claude: 'reset-first' }, quota_display: {}, quota_windows: {}
};
const resets = (id) => {
	const owner = accounts.find((a) => a.id === id);
	const ready = owner && ['claude', 'codex'].includes(owner.provider);
	return { account_id: id, provider: owner?.provider ?? '', status: ready ? 'ready' : 'unsupported', available_count: ready ? (id === 'acc-claude-home' ? 1 : 0) : null, next_expires_at: ready ? at(24 * 9) : null, error: null };
};

function api(method, path, body, query = new URLSearchParams()) {
	const target = path.match(/^\/clients(?:\/([^/]+))?$/);
	if (target && method !== 'GET') return changeClient(method, target[1] && decodeURIComponent(target[1]), body ?? {});
	if (method === 'POST' && path === '/restore') return { restored: 2, accounts: [], errors: [], settings: false };
	if (method !== 'GET') return { ok: true };
	const reset = path.match(/^\/accounts\/([^/]+)\/resets$/);
	if (reset) return resets(decodeURIComponent(reset[1]));
	if (path === '/clients/stats') return clientStats(Number(query.get('hours') ?? 24));
	switch (path) {
		case '/summary': case '/status': return { ...summary, clients: clients.length };
		case '/clients': return { clients, usage: usage.filter((row) => row.reset * 1000 > Date.now()) };
		case '/providers': return { providers };
		case '/accounts': return { accounts };
		case '/quotas': return { quotas };
		case '/models': return { models };
		case '/calls': return { calls };
		case '/settings': return settings;
		// Sample records only: the shape of an export, with no real token.
		case '/export': return { format: 'douane-export', version: 1, exported_at: new Date(now).toISOString(), settings, disabled_models: [], accounts: accounts.map(({ id, name, provider }) => ({ id, name, provider, access_token: 'sample' })) };
		default: return null;
	}
}

const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png', '.woff2': 'font/woff2', '.json': 'application/json' };

createServer(async (request, response) => {
	const url = new URL(request.url ?? '/', 'http://localhost');
	if (url.pathname.startsWith('/admin/api/')) {
		let input = null;
		if (request.method !== 'GET') {
			const chunks = [];
			for await (const chunk of request) chunks.push(chunk);
			try { input = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : null; } catch { input = null; }
		}
		const result = api(request.method ?? 'GET', url.pathname.slice('/admin/api'.length), input, url.searchParams);
		const [status, body] = Array.isArray(result) ? result : [result ? 200 : 404, result ?? { error: { message: 'not in the sample data' } }];
		if (status === 204) { response.writeHead(204).end(); return; }
		response.writeHead(status, { 'content-type': 'application/json' });
		response.end(JSON.stringify(body));
		return;
	}
	if (url.pathname === '/health') {
		response.writeHead(200, { 'content-type': 'application/json' }).end('{"status":"ok"}');
		return;
	}
	const relative = url.pathname.replace(/^\/admin\/?/, '') || 'index.html';
	const file = normalize(join(root, relative));
	try {
		if (!file.startsWith(root)) throw new Error('outside the build');
		const data = await readFile(file);
		response.writeHead(200, { 'content-type': types[extname(file)] ?? 'application/octet-stream' }).end(data);
	} catch {
		const index = await readFile(join(root, 'index.html'));
		response.writeHead(200, { 'content-type': 'text/html' }).end(index);
	}
}).listen(port, '127.0.0.1', () => console.log(`sample dashboard on http://127.0.0.1:${port}/admin/`));
