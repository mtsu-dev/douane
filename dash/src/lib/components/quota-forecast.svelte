<script lang="ts">
  import Info from '@lucide/svelte/icons/info';
  import { forecastSummary } from '$lib/forecast';
  import { providerName, type Account, type Quota, type WindowForecast } from '$lib/management';
  import { t } from '$lib/i18n.svelte';
  let { accounts, quotas, forecasts, provider }: { accounts: Account[]; quotas: Quota[]; forecasts: WindowForecast[]; provider: string } = $props();
  let summary = $derived(forecastSummary(accounts, quotas, forecasts));
  let methodOpen = $state(false);
  let percent = $derived(summary.percent);
  let scale = $derived(Math.max(200, Math.ceil((percent ?? 0) / 100) * 100));
  let paceState = $derived(summary.exhausted ? 'fast' : percent == null ? 'unknown' : percent > 100 ? 'fast' : percent < 100 ? 'slow' : 'balanced');
  let value = $derived(summary.exhausted ? t('forecast.exhausted') : percent == null ? t('forecast.unknown') : percent === 100 ? t('forecast.balanced') : t('forecast.deviation', { value: `${percent > 100 ? '+' : ''}${percent - 100}` }));
</script>

<section class="forecast" aria-label={t('forecast.provider', { provider: providerName(provider) })} data-pace={paceState}>
  <div class="pace-line">
    <span class="caption">{t('forecast.title')}</span>
    <div class="pace-track" class:unknown={percent == null && !summary.exhausted} role="meter" aria-label={t('forecast.provider', { provider: providerName(provider) })} aria-valuenow={percent ?? undefined} aria-valuemin="0" aria-valuemax={scale} aria-valuetext={percent == null ? value : t('forecast.percent', { value: percent })}>
      <span class="pace-fill" style:width="{summary.exhausted ? 100 : (percent ?? 0) / scale * 100}%"></span>
      <span class="average" style:left="{100 / scale * 100}%" title={t('forecast.average')}><span>100 %</span></span>
    </div>
    <span class="pace-value tabular">{value}</span>
    {#if summary.known < summary.total}<span class="coverage" title={t('forecast.coverage', { known: summary.known, total: summary.total })} aria-label={t('forecast.coverage', { known: summary.known, total: summary.total })}>{summary.known}/{summary.total}</span>{/if}
    <button class="info icon-button" aria-label={t('forecast.about')} aria-expanded={methodOpen} aria-controls="pace-info-{provider}" onclick={() => methodOpen = !methodOpen}><Info size={17}/></button>
  </div>
  {#if methodOpen}
    <div class="method" id="pace-info-{provider}">
      <p>{t('forecast.method')}</p>
      <p>{t('forecast.coverage', { known: summary.known, total: summary.total })}{#if summary.exhausted} · {t('forecast.exhaustedCount', { count: summary.exhausted })}{/if}</p>
    </div>
  {/if}
</section>

<style>
  .forecast { position: relative; margin: 0 0 16px; --pace-color: #81d9b2; }
  .forecast[data-pace='fast'] { --pace-color: #ffba8b; }
  .forecast[data-pace='slow'] { --pace-color: #9bb9ff; }
  .forecast[data-pace='unknown'] { --pace-color: var(--muted); }
  .pace-line { display: flex; align-items: center; gap: 16px; min-height: 49px; padding: 0 2px; }
  .caption { color: var(--muted); font-size: 15px; }
  .pace-track { position: relative; flex: 1; min-width: 40px; height: 8px; background: #2a3a55; border-radius: 4px; }
  .pace-fill { display: block; height: 100%; background: var(--pace-color); border-radius: inherit; }
  .average { position: absolute; top: -5px; height: 18px; border-left: 2px solid var(--text); }
  .average>span { position: absolute; top: -17px; transform: translateX(-50%); white-space: nowrap; font-size: 11px; color: var(--muted); }
  .unknown .pace-fill { display: none; }
  .pace-value { white-space: nowrap; color: var(--pace-color); font-size: 15px; font-weight: 600; }
  .coverage { font-size: 13px; color: var(--muted); }
  .info { width: 28px; height: 28px; min-height: 28px; flex-shrink: 0; }
  .method { position: absolute; z-index: 3; top: 100%; right: 0; width: 440px; max-width: calc(100vw - 64px); padding: 16px 18px; border: 1px solid var(--border); border-radius: 10px; background: var(--surface); box-shadow: 0 10px 28px #0005; color: var(--muted); font-size: 14px; line-height: 1.6; }
  .method p { margin: 0; }
  .method p+p { margin-top: 10px; }
  @media(max-width:650px) { .pace-line { gap: 8px; }.caption, .pace-value { font-size: 13px; }.coverage { font-size: 12px; } }
</style>
