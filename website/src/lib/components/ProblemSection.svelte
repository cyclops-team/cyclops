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
	<SectionHead title="THE PROBLEM" index="Why Cyclops" />
	<h3 class="statement">Your agents are powerful.<br />Their teamwork is manual.</h3>
	<p class="lede">
		Developers use different coding agents for different jobs, but coordinating them still means
		manually passing context, assigning work, and tracking progress. Cyclops gives them a shared
		coordination layer.
	</p>
	<div class="panel compare">
		<div class="col">
			<div class="label">Before Cyclops</div>
			<ul>
				{#each before as item (item)}
					<li><span class="marker before-marker">✕</span>{item}</li>
				{/each}
			</ul>
		</div>
		<div class="col">
			<div class="label accent">With Cyclops</div>
			<ul>
				{#each after as item (item)}
					<li><span class="marker after-marker">✓</span>{item}</li>
				{/each}
			</ul>
		</div>
	</div>
</section>

<style>
	.lede {
		margin-bottom: 40px;
	}

	.compare {
		display: grid;
		grid-template-columns: 1fr 1fr;
		padding: 0;
	}

	.col {
		padding: 28px 32px;
		min-width: 0;
	}

	.col:first-child {
		border-right: 1px solid var(--line);
	}

	.label {
		display: block;
		margin-bottom: 16px;
	}

	.label.accent {
		color: var(--accent);
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
		color: var(--faint);
	}

	.after-marker {
		color: var(--accent);
	}

	@media (max-width: 720px) {
		.compare {
			grid-template-columns: 1fr;
		}

		.col {
			padding: 24px;
		}

		.col:first-child {
			border-right: none;
			border-bottom: 1px solid var(--line);
		}
	}
</style>
