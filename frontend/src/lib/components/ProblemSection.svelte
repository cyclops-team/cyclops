<script lang="ts">
	import SectionHead from './SectionHead.svelte';

	const before = [
		'Isolated agent terminals',
		'Manual context handoffs',
		'No direct agent messaging',
		'Unclear agent responsibilities'
	];

	const after = [
		'Named agent roles',
		'Structured agent-to-agent messages',
		'Permission-aware routing',
		'Verified terminal delivery'
	];
</script>

<section class="section">
	<SectionHead title="THE PROBLEM" index="01 / Why Cyclops" />
	<h3 class="pixel heading">Your agents are powerful.<br />Their teamwork is manual.</h3>
	<p class="body">
		Developers use different coding agents for different jobs, but coordinating them still means
		manually passing context, assigning work, and tracking progress. Cyclops gives them a shared
		coordination layer.
	</p>
	<div class="compare">
		<div class="col">
			<div class="col-label">Before Cyclops</div>
			<ul>
				{#each before as item (item)}
					<li><span class="marker before-marker">✕</span>{item}</li>
				{/each}
			</ul>
		</div>
		<div class="col">
			<div class="col-label accent">With Cyclops</div>
			<ul>
				{#each after as item (item)}
					<li><span class="marker after-marker">✓</span>{item}</li>
				{/each}
			</ul>
		</div>
	</div>
</section>

<style>
	.heading {
		font-weight: 700;
		font-size: clamp(26px, 3.6vw, 38px);
		color: var(--ink);
		line-height: 1.15;
		margin: 0 0 24px;
	}

	.body {
		max-width: 640px;
		font-size: 16px;
		line-height: 1.7;
		color: var(--muted);
		margin: 0 0 44px;
	}

	.compare {
		display: grid;
		grid-template-columns: 1fr 1fr;
		border: 1px solid var(--line);
	}

	.col {
		padding: 32px;
	}

	.col:first-child {
		border-right: 1px solid var(--line);
	}

	.col-label {
		font-size: 11px;
		letter-spacing: 2px;
		text-transform: uppercase;
		color: var(--faint);
		margin-bottom: 20px;
	}

	.col-label.accent {
		color: var(--sage-ink);
	}

	ul {
		list-style: none;
		margin: 0;
		padding: 0;
	}

	li {
		display: flex;
		align-items: baseline;
		gap: 12px;
		font-size: 14px;
		color: var(--text);
		padding: 10px 0;
	}

	li + li {
		border-top: 1px solid var(--line-soft);
	}

	.marker {
		font-size: 12px;
		flex-shrink: 0;
		width: 14px;
	}

	.before-marker {
		color: var(--mauve-ink);
	}

	.after-marker {
		color: var(--sage-ink);
	}

	@media (max-width: 720px) {
		.compare {
			grid-template-columns: 1fr;
		}

		.col:first-child {
			border-right: none;
			border-bottom: 1px solid var(--line);
		}
	}
</style>
