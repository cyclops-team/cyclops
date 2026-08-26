<script lang="ts">
	import SectionHead from './SectionHead.svelte';

	const principles = [
		{
			glyph: '◇',
			title: 'Provider-independent',
			body: 'Use the right coding agent for each task without forcing your workflow into one provider.'
		},
		{
			glyph: '▤',
			title: 'Terminal-native',
			body: 'Keep every agent inside its real terminal environment with its existing tools and context.'
		},
		{
			glyph: '⇄',
			title: 'Reliable handoffs',
			body: 'Route structured requests to named agents and verify that they reach the intended terminal.'
		}
	];
</script>

<section class="section">
	<SectionHead title="WHY CYCLOPS" index="Principles" />
	<div class="grid">
		{#each principles as principle (principle.title)}
			<div class="panel card">
				<div class="glyph pixel">{principle.glyph}</div>
				<div class="title">{principle.title}</div>
				<div class="body">{principle.body}</div>
			</div>
		{/each}
	</div>
</section>

<style>
	.grid {
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 18px;
	}

	.card {
		padding: 28px;
		transition: border-color 0.15s;
	}

	.card:hover {
		border-color: var(--accent);
	}

	.glyph {
		font-size: 20px;
		color: var(--accent);
		margin-bottom: 20px;
	}

	.title {
		font-size: 15px;
		color: var(--ink);
		font-weight: 600;
		margin-bottom: 10px;
	}

	.body {
		font-size: 13.5px;
		color: var(--muted);
		line-height: 1.6;
	}

	@media (max-width: 900px) {
		.grid {
			grid-template-columns: 1fr;
		}
	}
</style>
