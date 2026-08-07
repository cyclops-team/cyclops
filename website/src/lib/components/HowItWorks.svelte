<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import Terminal from './Terminal.svelte';

	const steps = [
		{
			n: '01',
			title: 'Open the workspace',
			body: 'Run cyclops to open the full-screen workspace — it starts a tmux session and the daemon if none are running.',
			snippet: [{ cls: 'dim', text: '$ cyclops' }]
		},
		{
			n: '02',
			title: 'Talk to your agents',
			body: 'Start the coding agents you already use in its panes, name them, then just ask naturally.',
			snippet: [
				{ cls: 'dim', text: '$ cyclops name %1 reviewer' },
				{ cls: 'ok', text: '✔ named reviewer · %1' }
			]
		},
		{
			n: '03',
			title: 'Agents coordinate for you',
			body: 'Agents run the handoff through the cyclops CLI underneath, and the workspace shows the receipt.',
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
