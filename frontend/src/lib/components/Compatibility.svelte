<script lang="ts">
	import SectionHead from './SectionHead.svelte';

	const strip = ['claude', 'codex', 'gemini', 'opencode', 'your-agent'];
</script>

<section class="section">
	<SectionHead title="COMPATIBILITY" index="03 / Any agent" />
	<h3 class="pixel heading">If it runs in your terminal,<br />it can run in Cyclops.</h3>
	<div class="strip">
		{#each strip as name, i (name)}
			<span class="term-name">{name}</span>{#if i < strip.length - 1}<span class="dot">·</span>{/if}
		{/each}
	</div>
</section>

<style>
	.heading {
		font-weight: 700;
		font-size: clamp(24px, 3.4vw, 34px);
		color: var(--ink);
		line-height: 1.2;
		margin: 0 0 22px;
	}

	.strip {
		font-size: 14px;
		color: var(--gray);
	}

	.term-name {
		color: var(--muted);
	}

	.dot {
		color: var(--faint);
		margin: 0 10px;
	}
</style>
