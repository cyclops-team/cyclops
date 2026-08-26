<script lang="ts">
	import SectionHead from './SectionHead.svelte';

	const strip: { id: string; label?: string }[] = [
		{ id: 'claude' },
		{ id: 'codex' },
		{ id: 'cursor' },
		{ id: 'agy', label: 'Antigravity' },
		{ id: 'your-agent' }
	];
</script>

<section class="section">
	<SectionHead title="COMPATIBILITY" index="Any agent" />
	<h3 class="statement">If it runs in your terminal,<br />it can run in Cyclops.</h3>
	<div class="strip">
		{#each strip as agent, i (agent.id)}
			<span class="item">
				<span class="term-name">{agent.id}{#if agent.label}&nbsp;· {agent.label}{/if}</span>
				{#if i < strip.length - 1}<span class="dot" aria-hidden="true">·</span>{/if}
			</span>
		{/each}
	</div>
</section>

<style>
	.strip {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 8px 10px;
		font-size: 14px;
		color: var(--gray);
	}

	.item {
		display: inline-flex;
		align-items: center;
		gap: 10px;
		white-space: nowrap;
	}

	.term-name {
		color: var(--muted);
	}

	.dot {
		color: var(--faint);
	}
</style>
