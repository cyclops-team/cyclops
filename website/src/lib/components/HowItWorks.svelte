<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import Terminal from './Terminal.svelte';
	import NamePaneMock from './NamePaneMock.svelte';

	// Three steps in the order they happen, so the numbers carry meaning.
	// Each snippet is shown in the pane it would run in. Step 02 is a live
	// mock rather than a snippet: naming happens in the workspace's own
	// dialog, and a pane id in a command line shows none of that.
	const steps = [
		{
			n: '01',
			title: 'Open the workspace',
			body: 'Run cyclops to open the full-screen workspace — it starts a tmux session and the daemon if none are running.',
			pane: 'terminal',
			snippet: [{ cls: 'dim', text: '$ cyclops' }]
		},
		{
			n: '02',
			title: 'Talk to your agents',
			body: 'Start the coding agents you already use in its panes, name them, then just ask naturally.',
			pane: 'reviewer',
			snippet: []
		},
		{
			n: '03',
			title: 'Agents coordinate for you',
			body: 'Agents run the handoff through the cyclops CLI underneath, and the workspace shows the receipt.',
			pane: 'implementer',
			snippet: [
				{ cls: 'dim', text: '$ claude' },
				{ cls: 'ask', text: '> send the rate limiter to reviewer for review' },
				{ cls: 'dim', text: '$ cyclops send reviewer \\' },
				{ cls: 'dim', text: '  --subject "Review the rate limiter"' },
				{ cls: 'ok', text: '✓ delivered · unverified (screen)' }
			]
		}
	];
</script>

<section class="section" id="how-it-works">
	<SectionHead title="HOW IT WORKS" index="Workflow" />
	<div class="grid">
		{#each steps as step (step.n)}
			<div class="step">
				<div class="num pixel">{step.n}</div>
				<div class="title">{step.title}</div>
				<p class="body">{step.body}</p>
				{#if step.n === '02'}
					<NamePaneMock />
				{:else}
					<Terminal title={step.pane}>
						{#each step.snippet as line (line.text)}
							<div class={line.cls}>{line.text}</div>
						{/each}
					</Terminal>
				{/if}
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
		min-width: 0;
	}

	.num {
		font-size: 13px;
		color: var(--counter);
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
		margin: 0 0 24px;
	}

	.step :global(.frame) {
		margin-top: auto;
	}

	.step :global(.frame-body) {
		font-size: 11.5px;
		line-height: 1.8;
	}

	/* What you typed at the agent's prompt, before it ran the command. */
	.step :global(.frame-body .ask) {
		color: var(--term-text);
	}

	@media (max-width: 900px) {
		.grid {
			grid-template-columns: 1fr;
			gap: 40px;
		}
	}
</style>
