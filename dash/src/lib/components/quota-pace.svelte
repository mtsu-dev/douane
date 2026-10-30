<script lang="ts">
  import Info from '@lucide/svelte/icons/info';
  import { providerPace } from '$lib/pace';
  import { providerName, type Account, type Quota } from '$lib/management';
  import { t } from '$lib/i18n.svelte';
  let { accounts, quotas, provider }: { accounts: Account[]; quotas: Quota[]; provider: string } = $props();
  let pace = $derived(providerPace(accounts, quotas));
  let percent = $derived(pace.percent);
  let paceState = $derived(percent == null ? 'unknown' : percent > 110 ? 'fast' : percent < 90 ? 'slow' : 'balanced');
  // 0 to 200% with 100% in the middle: the fill grows from the middle toward the deviation.
  let position = $derived(Math.min(200, percent ?? 100) / 2);
  let methodOpen = $state(false);
</script>

<section class="pace" aria-label={t('pace.provider', { provider: providerName(provider) })} data-pace={paceState}>
  <span class="caption">{t('pace.title')}</span>
  <div class="track" role="meter" aria-label={t('pace.provider', { provider: providerName(provider) })} aria-valuenow={percent ?? undefined} aria-valuemin="0" aria-valuemax="200" aria-valuetext={percent == null ? t('pace.unknown') : t('pace.percent', { value: percent })}>
    <span class="fill" class:right={position > 50} style:left="{Math.min(50, position)}%" style:width="{Math.abs(position - 50)}%"></span>
    <span class="target" title={t('pace.target')}></span>
  </div>
  <span class="value tabular">{percent == null ? t('pace.unknown') : `${percent} %`}</span>
  {#if percent != null}<span class="verdict">{t(`pace.${paceState as 'fast' | 'slow' | 'balanced'}`)}</span>{/if}
  {#if pace.known < pace.total}<span class="coverage" title={t('pace.coverage', { known: pace.known, total: pace.total })}>{pace.known}/{pace.total}</span>{/if}
  <button class="info icon-button" aria-label={t('pace.about')} aria-expanded={methodOpen} aria-controls="pace-info-{provider}" onclick={() => methodOpen = !methodOpen}><Info size={17}/></button>
  {#if methodOpen}
    <div class="method" id="pace-info-{provider}">
      <p>{t('pace.method')}</p>
      <p>{t('pace.coverage', { known: pace.known, total: pace.total })}</p>
    </div>
  {/if}
</section>

<style>
  .pace { position: relative; display: flex; align-items: center; gap: 14px; margin: 0 0 14px; padding: 12px 14px 12px 18px; border: 1px solid var(--border); border-radius: 12px; background: var(--surface); --pace-color: #81d9b2; }
  .pace[data-pace='fast'] { --pace-color: #ffa070; }
  .pace[data-pace='slow'] { --pace-color: #8fb0ff; }
  .pace[data-pace='unknown'] { --pace-color: var(--muted); }
  .caption { color: var(--muted); font-size: 14px; font-weight: 500; }
  .track { position: relative; flex: 1; min-width: 60px; height: 16px; background: #22324d; border-radius: 8px; overflow: hidden; }
  .fill { position: absolute; top: 0; bottom: 0; background: var(--pace-color); border-radius: 8px 0 0 8px; transition: left .3s, width .3s; }
  .fill.right { border-radius: 0 8px 8px 0; }
  .target { position: absolute; left: calc(50% - 1px); top: 0; bottom: 0; width: 2px; background: var(--text); }
  .value { white-space: nowrap; color: var(--pace-color); font-size: 16px; font-weight: 700; min-width: 54px; text-align: right; }
  .verdict { white-space: nowrap; color: var(--pace-color); font-size: 14px; }
  .coverage { font-size: 13px; color: var(--muted); }
  .info { width: 28px; height: 28px; min-height: 28px; }
  .method { position: absolute; z-index: 3; top: calc(100% + 6px); right: 0; width: 440px; max-width: calc(100vw - 64px); padding: 16px 18px; border: 1px solid var(--border); border-radius: 10px; background: var(--surface); box-shadow: 0 10px 28px #0005; color: var(--muted); font-size: 14px; line-height: 1.6; }
  .method p { margin: 0; }
  .method p+p { margin-top: 10px; }
  @media(max-width:650px) { .pace { flex-wrap: wrap; gap: 8px 10px; padding: 10px 12px; }.track { flex-basis: 100%; order: 5; }.caption { margin-right: auto; } }
</style>
