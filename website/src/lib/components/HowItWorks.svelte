<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import Terminal from './Terminal.svelte';

	const steps = [
		{
			n: '01',
			title: 'Start a workspace',
			body: 'Start the coding agents you already use inside one Cyclops workspace.',
			snippet: [
				{ cls: 'dim', text: '$ cyclops start' },
				{ cls: 'ok', text: '✔ workspace ready · 3 agents' }
			]
		},
		{
			n: '02',
			title: 'Name each pane',
			body: 'Give each agent a stable address so every request reaches the intended terminal.',
			snippet: [
				{ cls: 'dim', text: '$ cyclops name %1 reviewer' },
				{ cls: 'ok', text: '✔ named reviewer · %1' }
			]
		},
		{
			n: '03',
			title: 'Send a request',
			body: 'Send structured requests between agents and verify they reach the intended terminal.',
			snippet: [
				{ cls: 'dim', text: '$ cyclops send reviewer \\' },
				{ cls: 'dim', text: '  --subject "Review the rate limiter"' },
				{ cls: 'ok', text: '✓ delivered · unverified (screen)' }
			]
		}
	];
</script>

<section class="section" id="how-it-works">
	<SectionHead title="HOW IT WORKS" index="02 / Workflow" />
	<div class="grid">
		{#each steps as step (step.n)}
			<div class="step">
				<div class="num pixel">{step.n}</div>
				<div class="title">{step.title}</div>
				<p class="body">{step.body}</p>
				<Terminal>
					{#each step.snippet as line (line.text)}
						<div class={line.cls}>{line.text}</div>
					{/each}
				</Terminal>
			</div>
		{/each}
	</div>
</section>

<style>
	.grid {
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 24px;
	}

	.step {
		display: flex;
		flex-direction: column;
	}

	.num {
		font-size: 13px;
		color: var(--sage-ink);
		margin-bottom: 16px;
	}

	.title {
		font-size: 16px;
		font-weight: 600;
		color: var(--ink);
		margin-bottom: 10px;
	}

	.body {
		font-size: 13.5px;
		line-height: 1.6;
		color: var(--muted);
		margin: 0 0 22px;
	}

	.step :global(.term) {
		margin-top: auto;
	}

	.step :global(.term-body) {
		font-size: 11.5px;
		line-height: 1.8;
		padding: 14px 16px;
	}

	@media (max-width: 900px) {
		.grid {
			grid-template-columns: 1fr;
			gap: 40px;
		}
	}
</style>
