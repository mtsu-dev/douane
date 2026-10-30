import { language, locale, t, type StringKey } from './i18n.svelte';

export type Provider = { id: string; name: string; oauth: boolean; quota: boolean; docs_url: string; credential?: boolean; base_url?: string };
export type Account = {
	id: string; name: string; provider: string; email?: string | null; plan?: string | null;
	subscription?: Subscription | null;
	enabled: boolean; status: string; last_error?: string | null; updated_at?: string | null;
	reserve_percent?: number | null; reserve_windows?: ReserveWindow[]; reserve_measured?: boolean | null; reserve_reached?: boolean;
	route?: string | null; base_url?: string | null; dialect?: Dialect | null; models?: string[];
};
export type Dialect = 'chat' | 'responses' | 'messages';
export const dialects: (Dialect | '')[] = ['chat', 'responses', 'messages', ''];
export function dialectLabel(id: Dialect | '') { return id === '' ? t('dialect.client') : { chat: 'Chat Completions', responses: 'Responses', messages: 'Messages (Anthropic)' }[id]; }
export type EndpointPatch = { route?: string; base_url?: string; dialect?: Dialect | null; models?: string[]; api_key?: string };
export type ReserveWindow = 'five-hour' | 'daily' | 'weekly' | 'monthly';
export const reserveWindows: ReserveWindow[] = ['five-hour', 'daily', 'weekly', 'monthly'];
export function reserveWindowLabel(id: ReserveWindow) { return t(`reserveWindow.${id}`); }
export function reserveWindow(id: string): ReserveWindow | null { return ({ 'five-hour': 'five-hour', daily: 'daily', weekly: 'weekly', 'seven-day': 'weekly', seven_day: 'weekly', monthly: 'monthly' } as Record<string, ReserveWindow>)[id] ?? null; }
export type QuotaWindow = { id: string; label: string; remaining_percent: number | null; reset_at?: string | null };
export type Subscription = { id: string; label: string; multiplier?: number | null; source: string };
export type CreditUsage = { id: string; label: string; used?: number | null; limit?: number | null; remaining?: number | null; unit: string; reset_at?: string | null; unlimited?: boolean | null; enabled?: boolean | null };
export type Quota = { account_id: string; windows: QuotaWindow[]; subscription?: Subscription | null; credits?: CreditUsage[]; updated_at?: string | null; error?: string | null; verification_required?: boolean; verification_available?: boolean };
export type ResetAvailability = { account_id: string; provider: string; status: 'ready' | 'unsupported' | 'unavailable' | 'ineligible' | 'disabled'; available_count: number | null; next_expires_at: string | null; pending_request_id?: string | null; error: string | null };
export type ResetResult = { request_id: string; outcome: 'reset' | 'nothing_to_reset' | 'no_credit' | 'already_redeemed' };
export type Model = { id: string; provider: string; name?: string; context_length?: number | null; enabled?: boolean; route?: string | null };
/** When a pool's catalog was last read; `stale` when its live listing failed and the previous catalog stays. */
export type ModelListing = { refreshed_at: string; stale: boolean };
export type ModelsReport = { models: Model[]; listings?: Record<string, ModelListing> };
export type Call = { id: string | number; started_at: string; model: string; provider: string; account_id?: string | null; status: number; latency_ms?: number; input_tokens?: number; output_tokens?: number; cache_read_tokens?: number; cache_write_tokens?: number; error?: string | null; client_id?: string | null };
export type Status = { version: string; uptime_seconds: number; in_flight: number; requests: number; errors: number; accounts: number; enabled_accounts: number; providers: number; input_tokens: number; output_tokens: number; cache_read_tokens?: number; cache_write_tokens?: number; client_api_base_url?: string | null; clients?: number };
/** The accounts a client key may use; `all` includes accounts connected later. */
export type ClientAccess = { all: boolean; pools: string[]; accounts: string[] };
export type Client = { id: string; name: string; key_hint: string; created_at: string; access: ClientAccess; shares: Record<string, number>; account_shares: Record<string, number> };
/** Percent points of one account-wide window attributed to each client id (`""` is a call without key). */
export type ClientUsage = { account_id: string; kind: ReserveWindow; reset: number; used: number; clients: Record<string, number>; outside: number };
export type ClientsReport = { clients: Client[]; usage: ClientUsage[] };
/** What one client spent on one model; `client_id` is empty for calls made before keys were required. */
export type ClientTotals = { client_id: string; provider: string; model: string; calls: number; errors: number; input_tokens: number; output_tokens: number; cache_read_tokens: number; cache_write_tokens: number; latency_ms: number; last_at: string };
/** One client's calls and tokens in one UTC hour, `hour` as `YYYY-MM-DDTHH`. */
export type ClientHour = { client_id: string; hour: string; calls: number; tokens: number };
export type ClientStats = { since: string; totals: ClientTotals[]; hours: ClientHour[] };
export type ClientPatch = { name?: string; access?: ClientAccess; shares?: Record<string, number>; account_shares?: Record<string, number>; rotate_key?: boolean };
export type ClientSaved = { client: Client; key: string | null };
export type Strategy = 'round-robin' | 'fill-first' | 'reset-first' | 'smart';
export type QuotaDisplay = 'accounts' | 'average';
export type QuotaWindowChoice = { hidden?: string[]; primary?: string | null };
export type Settings = { strategy: Strategy; quota_refresh_seconds: number; request_timeout_seconds: number; default_provider: string | null; provider_strategies?: Record<string, Strategy>; quota_display?: Record<string, QuotaDisplay>; quota_windows?: Record<string, QuotaWindowChoice>; show_provider_pace?: boolean; free_before_reset_hours?: number };
export type OAuthSession = { id: string; provider: string; url: string; state: string; redirect_uri: string; expires_at: string; status: string; account_id?: string | null; error?: string | null; user_code?: string | null; interval?: number | null; error_code?: string | null; verification_required?: boolean; verification_available?: boolean };
export type OAuthStatus = Partial<OAuthSession> & { status: string; account?: Account };
export type ImportError = string | { name?: string | null; error: string };
export type ImportResult = { accounts?: Account[]; imported: number; errors: ImportError[] };
export type RestoreResult = { accounts?: Account[]; restored: number; errors: ImportError[]; settings: boolean };
/** A configuration export: settings, disabled models and every account with its tokens. */
export type ExportBundle = { format: 'douane-export'; version: number; exported_at: string; settings: Settings; disabled_models: string[]; accounts: unknown[] };

export class ApiError extends Error {
	constructor(readonly status: number, message: string) { super(message); }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
	const response = await fetch(`/admin/api${path}`, {
		...init,
		headers: { ...(init?.body ? { 'content-type': 'application/json' } : {}), ...init?.headers }
	});
	if (!response.ok) {
		let message = t('api.serverStatus', { status: response.status });
		try {
			const body = await response.json();
			message = body.errors?.length ? importErrors(body.errors) : typeof body.error === 'string' ? body.error : body.error?.message ?? body.message ?? message;
		} catch { /* Preserve a meaningful HTTP error when the server does not return JSON. */ }
		throw new ApiError(response.status, message);
	}
	return response.status === 204 ? undefined as T : response.json();
}

export const management = {
	status: () => request<Status>('/summary'),
	providers: () => request<{ providers: Provider[] }>('/providers'),
	accounts: () => request<{ accounts: Account[] }>('/accounts'),
	quotas: () => request<{ quotas: Quota[] }>('/quotas'),
	models: () => request<ModelsReport>('/models'),
	refreshModels: (pool?: string) => request<ModelsReport>(`/models/refresh${pool ? `?pool=${encodeURIComponent(pool)}` : ''}`, { method: 'POST' }),
	setModelsEnabled: (ids: string[], enabled: boolean) => request('/models', { method: 'PATCH', body: JSON.stringify({ ids, enabled }) }),
	calls: () => request<{ calls: Call[] }>('/calls?limit=100'),
	settings: () => request<Settings>('/settings'),
	refreshQuotas: (account_id?: string) => request('/quotas/refresh', { method: 'POST', body: JSON.stringify({ account_id }) }),
	importAccounts: (files: { name: string; content: unknown }[]) => request<ImportResult>('/accounts/import', { method: 'POST', body: JSON.stringify({ files }) }),
	importCredential: (credential: { type: string; name: string; access_token: string; base_url: string; route?: string; dialect?: Dialect }) => request<ImportResult>('/accounts/import', { method: 'POST', body: JSON.stringify(credential) }),
	updateEndpoint: (id: string, patch: EndpointPatch) => request<{ account: Account }>(`/accounts/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify(patch) }),
	setAccountEnabled: (id: string, enabled: boolean) => request(`/accounts/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify({ enabled }) }),
	renameAccount: (id: string, name: string) => request<{ account: Account }>(`/accounts/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify({ name }) }),
	setAccountReserve: (id: string, reserve_percent: number | null, reserve_windows: ReserveWindow[]) => request<{ account: Account }>(`/accounts/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify({ reserve_percent, reserve_windows }) }),
	accountResets: (id: string) => request<ResetAvailability>(`/accounts/${encodeURIComponent(id)}/resets`),
	redeemReset: (id: string, request_id: string) => request<ResetResult>(`/accounts/${encodeURIComponent(id)}/resets/redeem`, { method: 'POST', body: JSON.stringify({ confirmation: true, request_id }) }),
	deleteAccount: (id: string) => request(`/accounts/${encodeURIComponent(id)}`, { method: 'DELETE' }),
	startOAuth: (provider: string) => request<OAuthSession>('/oauth/start', { method: 'POST', body: JSON.stringify({ provider }) }),
	oauth: (id: string) => request<OAuthStatus>(`/oauth/${encodeURIComponent(id)}`),
	oauthCallback: (id: string, code: string, state: string) => request<{ account: Account }>('/oauth/callback', { method: 'POST', body: JSON.stringify({ id, code, state }) }),
	setModelEnabled: (id: string, enabled: boolean) => request(`/models/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify({ enabled }) }),
	settingsSave: (settings: Partial<Settings>) => request<Settings>('/settings', { method: 'PATCH', body: JSON.stringify(settings) }),
	clients: () => request<ClientsReport>('/clients'),
	clientStats: (hours: number) => request<ClientStats>(`/clients/stats?hours=${hours}`),
	createClient: (patch: ClientPatch) => request<ClientSaved>('/clients', { method: 'POST', body: JSON.stringify(patch) }),
	updateClient: (id: string, patch: ClientPatch) => request<ClientSaved>(`/clients/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify(patch) }),
	deleteClient: (id: string) => request(`/clients/${encodeURIComponent(id)}`, { method: 'DELETE' }),
	exportConfig: () => request<ExportBundle>('/export'),
	restore: (bundle: unknown, settings: boolean) => request<RestoreResult>('/restore', { method: 'POST', body: JSON.stringify({ bundle, settings }) })
};

export const providerNames: Record<string, string> = {
	codex: 'Codex', claude: 'Claude', gemini: 'Gemini', antigravity: 'Antigravity', qwen: 'Qwen', kimi: 'Kimi', copilot: 'GitHub Copilot', meta: 'Muse Code', iflow: 'iFlow', opencode_go: 'OpenCode Go', xai: 'Grok', grok: 'Grok', openai_compatible: 'OpenAI-compatible'
};
/** The pool an account serves: its route for a custom endpoint, else its provider. */
export function accountPool(account: Account) { return account.route || account.provider; }
export type Pool = { id: string; provider: string; name: string; accounts: Account[] };
/** Pools with their accounts, in the order accounts arrive. */
export function pools(accounts: Account[]): Pool[] {
	const byId = new Map<string, Pool>();
	for (const account of accounts) {
		const id = accountPool(account);
		const pool = byId.get(id) ?? { id, provider: account.provider, name: account.route ? account.name : providerName(account.provider), accounts: [] };
		pool.accounts.push(account);
		byId.set(id, pool);
	}
	return [...byId.values()];
}
export function privateLabel(value: string, showEmails = false) { return showEmails ? value : value.replace(/[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}/gi, t('private.account')); }
export function importErrors(errors: ImportError[]) { return errors.map(error => typeof error === 'string' ? error : error.name ? t('api.errorPrefix', { name: privateLabel(error.name), error: error.error }) : error.error).join(' '); }
const statusKeys: Record<string, StringKey> = { ready: 'status.active', active: 'status.active', disabled: 'common.disabled', exhausted: 'status.exhausted', cooldown: 'status.cooldown', expired: 'status.expired', error: 'status.check' };
export function accountStatus(account: Account) { return t(!account.enabled ? 'common.disabled' : statusKeys[account.status] ?? 'status.check'); }
export function accountWarning(account: Account) { return account.enabled && (!!account.last_error || !['ready','active'].includes(account.status)); }
/** The dashboard's name for a provider; `fallback` is the server's name for an unknown id. */
export function providerName(id: string, fallback = id) { return id === 'openai_compatible' ? t('provider.openaiCompatible') : providerNames[id] ?? fallback; }
export function formatDecimal(value: number) { return new Intl.NumberFormat(locale(), { maximumFractionDigits: 1 }).format(value); }
export function formatCount(value: number | undefined) { return value == null ? '-' : new Intl.NumberFormat(locale(), { notation: value >= 10000 ? 'compact' : 'standard', maximumFractionDigits: 1 }).format(value); }
/** A 12-hour English clock reads `1:49 PM`, not `01:49 PM`; French keeps `13:49`. */
function hour() { return language() === 'en' ? 'numeric' as const : '2-digit' as const; }
export function formatDate(value?: string | null) { return value ? new Intl.DateTimeFormat(locale(), { day: '2-digit', month: 'short', hour: hour(), minute: '2-digit' }).format(new Date(value)) : '-'; }
/** How long ago `value` was, `5 min ago`; a date past a day. */
export function ageLabel(value: string, now = Date.now()) {
	const seconds = Math.round((new Date(value).getTime() - now) / 1000);
	const format = new Intl.RelativeTimeFormat(locale(), { numeric: 'auto', style: 'short' });
	if (seconds > -45) return format.format(0, 'second');
	if (seconds > -3600) return format.format(Math.round(seconds / 60), 'minute');
	if (seconds > -86400) return format.format(Math.round(seconds / 3600), 'hour');
	return formatDate(value);
}
export function resetLabel(value?: string | null) {
	if (!value) return t('date.unknown');
	const date = new Date(value);
	if (Number.isNaN(date.getTime())) return t('date.unknown');
	const distant = Math.abs(date.getTime() - Date.now()) > 7 * 24 * 60 * 60 * 1000;
	return new Intl.DateTimeFormat(locale(), { weekday: 'long', ...(distant ? { day: 'numeric', month: 'short' } as const : {}), hour: hour(), minute: '2-digit' }).format(date);
}
export function creditValue(value: number | null | undefined, unit: string) {
	if (value == null) return t('credit.unknown');
	if (unit === 'USD') return new Intl.NumberFormat(locale(), { style: 'currency', currency: 'USD', maximumFractionDigits: 2 }).format(value);
	if (/^[A-Z]{3}_minor$/.test(unit)) return new Intl.NumberFormat(locale(), { style: 'currency', currency: unit.slice(0, 3), maximumFractionDigits: 2 }).format(value / 100);
	return `${new Intl.NumberFormat(locale(), { maximumFractionDigits: 2 }).format(value)} ${unit === 'credits' ? t('credit.credits') : unit}`;
}
const usageKeys: Record<string, StringKey> = { weekly: 'usage.weekly', 'codex weekly': 'usage.weekly', 'grok weekly': 'usage.weekly', monthly: 'usage.monthly', '5 hours': 'usage.fiveHours', 'five hour': 'usage.fiveHours', 'extra usage': 'usage.extra', 'additional usage': 'usage.extra', 'codex credits': 'usage.codexCredits', 'credit balance': 'usage.creditBalance', 'premium requests': 'usage.premiumRequests', chat: 'usage.chat', completions: 'usage.completions' };
/** A display label for a quota label the server sends in English. */
export function usageLabel(label: string) {
	const key = usageKeys[label.toLowerCase()];
	return key ? t(key) : label;
}
export function newResetRequestId() {
	if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
	const bytes = crypto.getRandomValues(new Uint8Array(16));
	bytes[6] = (bytes[6] & 0x0f) | 0x40; bytes[8] = (bytes[8] & 0x3f) | 0x80;
	const hex = Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
	return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
