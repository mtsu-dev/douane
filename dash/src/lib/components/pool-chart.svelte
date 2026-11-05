<script module lang="ts">
	import type { Sample } from '$lib/pool';
	export type ChartLine = { id: string; name: string; color: string; recorded: Sample[]; projected: Sample[] };
	export type ChartLabel = { t: number; v: number; text: string };
	export type HoverInfo = { level: number; value: string; accounts: { id: string; name: string; color: string; value: string }[] };
</script>

<script lang="ts">
	import { addDays, dayNumber, moment, weekday } from '$lib/pool-format';
	import { t } from '$lib/i18n.svelte';

	let { start, end, now, max, accent, recorded, projected, labels = [], lines = [], label, hover, tick }: {
		start: number; end: number; now: number; max: number; accent: string;
		recorded: Sample[]; projected: Sample[]; labels?: ChartLabel[]; lines?: ChartLine[];
		label: string; hover: (t: number) => HoverInfo | null; tick: (value: number) => string;
	} = $props();

	const uid = $props.id();
	const M = { left: 48, right: 14, top: 30, bottom: 42 };
	let width = $state(0);
	let height = $derived(width < 560 ? 230 : 300);
	let plotW = $derived(Math.max(10, width - M.left - M.right));
	let plotH = $derived(height - M.top - M.bottom);
	let top = $derived(max > 0 ? max : 1);
	const x = (ms: number) => M.left + (ms - start) / (end - start) * plotW;
	const y = (v: number) => M.top + plotH - Math.max(0, Math.min(v, top)) / top * plotH;

	/** Samples inside the range plus one neighbour on each side; the clip path trims the rest. */
	function visible(samples: Sample[]) {
		const first = samples.findIndex(p => p.t >= start), last = samples.findLastIndex(p => p.t <= end);
		if (first < 0 || last < 0) return samples.length === 1 ? [] : samples.filter(p => p.t >= start && p.t <= end);
		return samples.slice(Math.max(0, first - 1), last + 2);
	}
	const line = (samples: Sample[]) => samples.map((p, i) => `${i ? 'L' : 'M'}${x(p.t).toFixed(1)},${y(p.v).toFixed(1)}`).join('');
	function area(samples: Sample[]) {
		if (samples.length < 2) return '';
		return `${line(samples)}L${x(samples[samples.length - 1].t).toFixed(1)},${y(0)}L${x(samples[0].t).toFixed(1)},${y(0)}Z`;
	}

	let seen = $derived(visible(recorded));
	let ahead = $derived(visible(projected));
	let days = $derived.by(() => { const list: number[] = []; for (let d = start; d < end; d = addDays(d, 1)) list.push(d); return list; });
	let dayW = $derived(plotW / Math.max(1, days.length));
	let every = $derived(Math.max(1, Math.ceil(34 / dayW)));
	let ticks = $derived([0, top / 2, top]);
	let nowShown = $derived(now >= start && now <= end);
	let last = $derived(recorded[recorded.length - 1]);

	/** Reset labels above the step; a crowded one drops below the line, a third is skipped. */
	let placed = $derived.by(() => {
		const rows = [-Infinity, -Infinity];
		const out: { x: number; y: number; text: string; below: boolean }[] = [];
		for (const item of labels) {
			if (item.t <= start || item.t >= end) continue;
			const px = x(item.t), width = item.text.length * 6.6 + 6;
			const row = px - width / 2 > rows[0] + 4 ? 0 : px - width / 2 > rows[1] + 4 ? 1 : -1;
			if (row < 0) continue;
			rows[row] = px + width / 2;
			const lineY = y(item.v);
			out.push({ x: Math.min(M.left + plotW - width / 2, Math.max(M.left + width / 2, px)), y: row === 0 ? Math.max(M.top - 4, lineY - 8) : lineY + 16, text: item.text, below: row === 1 });
		}
		return out;
	});

	let hoverT = $state<number | null>(null);
	let svg = $state<SVGSVGElement>();
	let tipW = $state(0);
	let info = $derived(hoverT == null ? null : hover(hoverT));
	function move(event: PointerEvent) {
		if (!svg) return;
		const px = event.clientX - svg.getBoundingClientRect().left;
		if (px < M.left || px > M.left + plotW) { hoverT = null; return; }
		hoverT = Math.round((start + (px - M.left) / plotW * (end - start)) / 300_000) * 300_000;
	}
	let hoverX = $derived(hoverT == null ? 0 : x(hoverT));
	let tipLeft = $derived(Math.min(Math.max(0, width - tipW), Math.max(0, hoverX + 14 + tipW > width ? hoverX - tipW - 14 : hoverX + 14)));
</script>

<div class="chart" bind:clientWidth={width} style:--accent={accent}>
	{#if width}
		<svg bind:this={svg} style:height="{height}px" viewBox="0 0 {width} {height}" role="img" aria-label={label} onpointermove={move} onpointerdown={move} onpointerleave={() => hoverT = null}>
			<defs>
				<linearGradient id="{uid}-fill" gradientUnits="userSpaceOnUse" x1="0" y1={M.top} x2="0" y2={M.top + plotH}>
					<stop offset="0" stop-color={accent} stop-opacity=".34"/>
					<stop offset=".55" stop-color={accent} stop-opacity=".1"/>
					<stop offset="1" stop-color={accent} stop-opacity="0"/>
				</linearGradient>
				<clipPath id="{uid}-clip"><rect x={M.left} y={M.top - 6} width={plotW} height={plotH + 6}/></clipPath>
			</defs>
			{#each days as d, i (d)}
				<line class="grid" x1={x(d)} x2={x(d)} y1={M.top} y2={M.top + plotH}/>
				{#if i % every === 0}
					<text class="axis day" x={x(d) + dayW / 2} y={M.top + plotH + 17} text-anchor="middle" class:today={now >= d && now < addDays(d, 1)}>{weekday(d)}</text>
					<text class="axis day" x={x(d) + dayW / 2} y={M.top + plotH + 33} text-anchor="middle" class:today={now >= d && now < addDays(d, 1)}>{dayNumber(d)}</text>
				{/if}
			{/each}
			{#each ticks as value (value)}
				<line class="grid" class:base={value === 0} x1={M.left} x2={M.left + plotW} y1={y(value)} y2={y(value)}/>
				<text class="axis" x={M.left - 8} y={y(value) + 4} text-anchor="end">{tick(value)}</text>
			{/each}
			<g clip-path="url(#{uid}-clip)">
				<path class="area" d={area(seen)} fill="url(#{uid}-fill)"/>
				<path class="area ahead" d={area(ahead)} fill="url(#{uid}-fill)"/>
				{#each lines as item (item.id)}
					<path class="account" d={line(visible(item.recorded))} stroke={item.color}/>
					<path class="account ahead" d={line(visible(item.projected))} stroke={item.color}/>
				{/each}
				<path class="projected" d={line(ahead)}/>
				<path class="recorded" d={line(seen)} pathLength="1"/>
			</g>
			{#each placed as item (item.x + item.text)}
				<text class="reset" class:below={item.below} x={item.x} y={item.y} text-anchor="middle">{item.text}</text>
			{/each}
			{#if nowShown}
				<line class="now" x1={x(now)} x2={x(now)} y1={M.top - 8} y2={M.top + plotH + 40}/>
				<text class="now-label" x={x(now)} y={M.top - 14} text-anchor="middle">{t('pool.now')}</text>
				{#if last}<circle class="pulse" cx={x(now)} cy={y(last.v)} r="4"/><circle class="dot" cx={x(now)} cy={y(last.v)} r="4"/>{/if}
			{/if}
			{#if hoverT != null && info}
				<line class="cross" x1={hoverX} x2={hoverX} y1={M.top} y2={M.top + plotH}/>
				<circle class="hover-dot" cx={hoverX} cy={y(info.level)} r="4.5"/>
			{/if}
		</svg>
		{#if hoverT != null && info}
			<div class="tip" bind:clientWidth={tipW} style:left="{tipLeft}px" style:top="{M.top - 2}px" role="status">
				<p><span>{moment(hoverT, now)}</span> · <strong>{info.value}</strong></p>
				{#each info.accounts as item (item.id)}<p class="tip-account"><i style:background={item.color}></i>{item.name}<b>{item.value}</b></p>{/each}
			</div>
		{/if}
	{/if}
</div>

<style>
	.chart { position: relative; width: 100%; min-width: 0; user-select: none; }
	svg { display: block; width: 100%; overflow: visible; touch-action: pan-y; cursor: crosshair; }
	.grid { stroke: #7899ff12; stroke-width: 1; }
	.grid.base { stroke: #7899ff2e; }
	.axis { fill: var(--faint); font-size: 12px; font-variant-numeric: tabular-nums; }
	.axis.today { fill: var(--text); font-weight: 600; }
	.area { opacity: 0; animation: fade .9s .15s ease-out forwards; }
	.area.ahead { animation-delay: .7s; --to: .45; }
	.recorded { fill: none; stroke: var(--accent); stroke-width: 2.2; stroke-linejoin: round; stroke-linecap: round; stroke-dasharray: 1; stroke-dashoffset: 1; animation: draw 1s cubic-bezier(.3,.7,.2,1) forwards; filter: drop-shadow(0 0 5px color-mix(in srgb, var(--accent) 45%, transparent)); }
	.projected { fill: none; stroke: var(--accent); stroke-width: 1.8; stroke-dasharray: 5 5; stroke-linejoin: round; opacity: 0; animation: fade .6s .8s ease-out forwards; --to: .9; }
	.account { fill: none; stroke-width: 1.3; stroke-linejoin: round; opacity: .9; }
	.account.ahead { stroke-dasharray: 3 4; opacity: .7; }
	.reset { fill: color-mix(in srgb, var(--accent) 70%, var(--text)); font-size: 12px; font-weight: 650; font-variant-numeric: tabular-nums; paint-order: stroke; stroke: var(--surface); stroke-width: 3px; opacity: 0; animation: fade .4s 1s forwards; }
	.reset.below { fill: color-mix(in srgb, var(--accent) 55%, var(--muted)); }
	.now { stroke: var(--muted); stroke-width: 1; opacity: .55; }
	.now-label { fill: var(--muted); font-size: 12px; font-weight: 600; }
	.dot { fill: var(--accent); stroke: var(--surface); stroke-width: 2; }
	.pulse { fill: var(--accent); transform-box: fill-box; transform-origin: center; animation: pulse 2.4s ease-out infinite; }
	.hover-dot { fill: var(--surface); stroke: var(--accent); stroke-width: 2.2; pointer-events: none; }
	.cross { stroke: var(--accent); stroke-width: 1; opacity: .7; pointer-events: none; }
	.tip { position: absolute; z-index: 2; pointer-events: none; padding: 7px 11px; border: 1px solid color-mix(in srgb, var(--accent) 30%, var(--border)); border-radius: 9px; background: color-mix(in srgb, var(--surface-raised) 92%, transparent); backdrop-filter: blur(6px); box-shadow: 0 10px 26px #02061366; font-size: 14px; white-space: nowrap; animation: pop .14s ease-out; }
	.tip p { margin: 0; color: var(--muted); }
	.tip strong { color: var(--text); font-weight: 650; }
	.tip-account { display: flex; align-items: center; gap: 7px; margin-top: 4px !important; font-size: 13px; }
	.tip-account i { width: 8px; height: 8px; border-radius: 50%; }
	.tip-account b { margin-left: auto; padding-left: 14px; color: var(--text); font-weight: 600; font-variant-numeric: tabular-nums; }
	@keyframes draw { to { stroke-dashoffset: 0; } }
	@keyframes fade { to { opacity: var(--to, 1); } }
	@keyframes pulse { 0% { transform: scale(1); opacity: .55; } 80%, 100% { transform: scale(3.2); opacity: 0; } }
	@keyframes pop { from { opacity: 0; transform: translateY(3px); } }
	@media (prefers-reduced-motion: reduce) { .recorded, .projected, .area, .reset { animation-duration: 1ms; animation-delay: 0s; } .pulse { animation: none; opacity: 0; } }
</style>
