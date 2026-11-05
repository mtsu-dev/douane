import { language, locale, t } from './i18n.svelte';
import { DAY, HOUR } from './pool.ts';

/** Each provider's line color on the pool chart; douane blue for the rest. */
const accents: Record<string, string> = { claude: '#f0895f', codex: '#7c9dff', openai: '#7c9dff', opencode_go: '#4dd4ac', opencode: '#4dd4ac', xai: '#c9d3e6', grok: '#c9d3e6', copilot: '#b39bff', antigravity: '#5fb4ff', gemini: '#5fb4ff', kimi: '#7fd3ff', meta: '#6fa8ff', qwen: '#a594ff' };
export const providerAccent = (provider: string) => accents[provider] ?? '#7899ff';
/** Per-account overlay colors: distinct from each other and from the navy surface. */
export const accountColors = ['#8fb0ff', '#4dd4ac', '#f4c154', '#ff8195', '#c49bff', '#5fd0e6', '#ffb27a', '#9be07a'];

const hour = () => language() === 'en' ? 'numeric' as const : '2-digit' as const;
/** `570%`, `570 %` in French. */
export const percent = (value: number) => `${new Intl.NumberFormat(locale(), { maximumFractionDigits: 0 }).format(Math.round(value))}${language() === 'fr' ? ' ' : ''}%`;
export const rate = (value: number) => t('pool.perHour', { value: new Intl.NumberFormat(locale(), { maximumFractionDigits: value >= 10 ? 0 : 1, minimumFractionDigits: value >= 10 ? 0 : 1 }).format(value) });
export const weight = (value: number) => new Intl.NumberFormat(locale(), { maximumFractionDigits: 2 }).format(value);
/** `Tue 10:00 AM`, with the day and month past a week. */
export function moment(ms: number, now = Date.now()) {
	const distant = Math.abs(ms - now) > 6 * DAY;
	return new Intl.DateTimeFormat(locale(), { weekday: 'short', ...(distant ? { day: 'numeric', month: 'short' } as const : {}), hour: hour(), minute: '2-digit' }).format(ms);
}
export const day = (ms: number) => new Intl.DateTimeFormat(locale(), { month: 'short', day: 'numeric' }).format(ms);
export const weekday = (ms: number) => new Intl.DateTimeFormat(locale(), { weekday: 'short' }).format(ms);
export const dayNumber = (ms: number) => new Intl.DateTimeFormat(locale(), { day: 'numeric' }).format(ms);
/** `2d 9h`, `5h 20m` or `12 min`. */
export function duration(ms: number) {
	const minutes = Math.max(0, Math.round(ms / 60_000));
	if (ms >= DAY) return t('pool.duration.days', { d: Math.floor(ms / DAY), h: Math.floor((ms % DAY) / HOUR) });
	if (ms >= HOUR) return t('pool.duration.hours', { h: Math.floor(minutes / 60), m: minutes % 60 });
	return t('pool.duration.minutes', { m: minutes });
}
/** Local midnight of the day holding `ms`. */
export function midnight(ms: number) { const date = new Date(ms); date.setHours(0, 0, 0, 0); return date.getTime(); }
/** Local midnight `days` days after `ms`'s midnight, safe across daylight saving changes. */
export function addDays(ms: number, days: number) { const date = new Date(midnight(ms)); date.setDate(date.getDate() + days); return date.getTime(); }
