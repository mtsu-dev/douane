<script lang="ts">
	import Network from '@lucide/svelte/icons/network';
	import claude from '$lib/assets/providers/claude.svg';
	import codex from '$lib/assets/providers/codex.svg';
	import openai from '$lib/assets/providers/openai.svg';
	import go from '$lib/assets/providers/opencode-go.svg';
	import xai from '$lib/assets/providers/xai.svg';
	import antigravity from '$lib/assets/providers/antigravity.svg';
	import gemini from '$lib/assets/providers/gemini.svg';
	import kimi from '$lib/assets/providers/kimi.svg';
	import qwen from '$lib/assets/providers/qwen.svg';
	import copilot from '$lib/assets/providers/copilot.svg';
	import meta from '$lib/assets/providers/meta.svg';
	let { provider, size = 20 }: { provider: string; size?: number } = $props();
	const logos: Record<string,string> = { claude,codex,openai,opencode_go:go,opencode:go,xai,grok:xai,antigravity,gemini,google:gemini,kimi,qwen,copilot,meta };
	let logo = $derived(logos[provider]);
</script>

<span class="provider-mark" class:light-mat={provider === 'qwen'} style:width="{size}px" style:height="{size}px" aria-hidden="true">
	{#if logo}<img src={logo} alt="" width={size} height={size}/>{:else}<Network size={size} strokeWidth={1.6}/>{/if}
</span>

<style>
	.provider-mark { display: inline-flex; align-items: center; justify-content: center; flex-shrink: 0; color: inherit; }
	img { display: block; width: 100%; height: 100%; object-fit: contain; }
	.light-mat { padding: 2px; border-radius: 5px; background: #eaf0ff; }
</style>
