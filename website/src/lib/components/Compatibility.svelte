<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import AgentMark from './AgentMark.svelte';
	import { REPO_URL } from '$lib/config';

	const MANIFESTS_URL = `${REPO_URL}/blob/main/docs/reference/MANIFESTS.md`;

	// The shipped manifests (resources/manifests/*.toml): each row is
	// the CLI's command, the way a pane names it, and the manifest's display
	// name. The next agent is the reader's, and it gets the mark.
	const detected = [
		{ cmd: 'claude', name: 'Claude Code' },
		{ cmd: 'codex', name: 'Codex CLI' },
		{ cmd: 'cursor-agent', name: 'Cursor Agent' },
		{ cmd: 'agy', name: 'Antigravity CLI' },
		{ cmd: 'kimi', name: 'Kimi Code CLI' }
	];
</script>

<section class="section">
	<SectionHead title="COMPATIBILITY" index="Any agent" />
	<div class="split">
		<div class="copy">
			<h3 class="statement">If it runs in your terminal,<br />it can run in Cyclops.</h3>
			<p class="lede">
				Cyclops detects supported agents automatically. Each one is described by a small manifest
				file: what its process is called, how it reports back, and how to tell when it's busy. 5
				agents detected out of the box. Teaching Cyclops a new agent CLI is one file.
			</p>
		</div>
		<div class="panel card">
			<ul class="list" aria-label="Agents detected out of the box">
				<li class="head label">Detected out of the box</li>
				{#each detected as agent (agent.cmd)}
					<li class="agent">
						<span class="marker" aria-hidden="true">✓</span>
						<span class="cmd">{agent.cmd}</span>
						<span class="name">{agent.name}</span>
					</li>
				{/each}
			</ul>
			<div class="yours">
				<AgentMark size={72} />
				<div class="yours-text">
					<span class="cmd">your-agent</span>
					<span class="name">Any agent you want</span>
					<a class="more" href={MANIFESTS_URL} target="_blank" rel="noopener noreferrer"
						>Write a manifest →</a
					>
				</div>
			</div>
		</div>
	</div>
</section>

<style>
	.split {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: 48px;
		align-items: center;
	}

	/* One notch under the other sections' statements: it shares the row
	   with the card and must not shout over it. */
	.statement {
		font-size: clamp(22px, 2.5vw, 30px);
		margin-bottom: calc(16px + 0.4em);
	}

	.lede {
		margin: 0;
	}

	.card {
		padding: 0;
	}

	.list {
		list-style: none;
		margin: 0;
		padding: 16px 0 12px;
	}

	.head {
		padding: 8px 28px 10px;
	}

	.agent {
		display: grid;
		grid-template-columns: 14px 110px minmax(0, 1fr);
		align-items: baseline;
		gap: 16px;
		padding: 7px 28px;
	}

	.marker {
		font-size: 12px;
		color: var(--accent);
	}

	/* The reader's agent: the mark, then the same command-and-name pair as
	   the rows above it, with the way in. */
	.yours {
		display: flex;
		align-items: center;
		gap: 22px;
		padding: 22px 28px 24px;
		border-top: 1px solid var(--line);
	}

	.yours :global(canvas) {
		flex-shrink: 0;
	}

	.yours-text {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}

	.more {
		margin-top: 8px;
		font-size: 12px;
		color: var(--accent);
	}

	.more:hover {
		text-decoration: underline;
	}

	.cmd {
		font-size: 13px;
		color: var(--ink);
	}

	.name {
		font-size: 11px;
		letter-spacing: 0.4px;
		color: var(--faint);
	}

	@media (max-width: 900px) {
		.split {
			grid-template-columns: 1fr;
			gap: 36px;
		}
	}

	@media (max-width: 400px) {
		.agent {
			grid-template-columns: 14px minmax(0, 1fr);
		}

		.agent .name {
			grid-column: 2;
		}
	}
</style>
