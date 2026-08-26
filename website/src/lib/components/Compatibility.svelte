<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import AgentMark, { type MarkKind } from './AgentMark.svelte';
	import { REPO_URL } from '$lib/config';

	const MANIFESTS_URL = `${REPO_URL}/blob/main/docs/reference/MANIFESTS.md`;

	// The four manifests that ship (resources/manifests/*.toml), then the
	// one you write. Each row is the CLI's command, the way a pane names it,
	// with the manifest's display name under it.
	const agents: { kind: MarkKind; cmd: string; name: string }[] = [
		{ kind: 'claude', cmd: 'claude', name: 'Claude Code' },
		{ kind: 'codex', cmd: 'codex', name: 'Codex CLI' },
		{ kind: 'cursor', cmd: 'cursor-agent', name: 'Cursor Agent' },
		{ kind: 'agy', cmd: 'agy', name: 'Antigravity CLI' },
		{ kind: 'any', cmd: 'your-agent', name: 'One manifest file' }
	];
</script>

<section class="section">
	<SectionHead title="COMPATIBILITY" index="Any agent" />
	<div class="split">
		<div class="copy">
			<h3 class="statement">If it runs in your terminal,<br />it can run in Cyclops.</h3>
			<p class="lede">
				Cyclops recognises an agent by a manifest: the process it runs as, the hooks it can
				report through, and what its title says while it is busy. Four ship today. Teaching it a
				new CLI is one file.
			</p>
		</div>
		<ul class="marks" aria-label="Agents Cyclops recognises">
			{#each agents as agent (agent.kind)}
				{#if agent.kind === 'any'}
					<!-- The fifth card takes the rest of the row: it is the one that
					     is about the reader's agent, so it gets the room and the link. -->
					<li class="mark yours">
						<AgentMark kind={agent.kind} size={64} />
						<div class="yours-text">
							<span class="cmd">{agent.cmd}</span>
							<span class="name">{agent.name}</span>
							<a class="more" href={MANIFESTS_URL} target="_blank" rel="noopener noreferrer"
								>Write a manifest →</a
							>
						</div>
					</li>
				{:else}
					<li class="mark">
						<AgentMark kind={agent.kind} size={64} />
						<span class="cmd">{agent.cmd}</span>
						<span class="name">{agent.name}</span>
					</li>
				{/if}
			{/each}
		</ul>
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
	   with the marks and must not shout over them. */
	.statement {
		font-size: clamp(22px, 2.5vw, 30px);
		margin-bottom: calc(16px + 0.4em);
	}

	.lede {
		margin: 0;
	}

	.marks {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 8px;
	}

	.mark {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 4px;
		padding: 22px 12px 18px;
		border: 1px solid var(--line);
		background: var(--surface);
		transition: border-color 0.15s;
	}

	.mark:hover {
		border-color: var(--accent);
	}

	.mark :global(canvas) {
		margin-bottom: 8px;
	}

	.yours {
		grid-column: span 2;
		flex-direction: row;
		justify-content: center;
		gap: 20px;
		padding: 22px 24px 18px;
	}

	.yours :global(canvas) {
		margin-bottom: 0;
	}

	.yours-text {
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}

	.more {
		margin-top: 6px;
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

	@media (max-width: 480px) {
		.marks {
			grid-template-columns: repeat(2, 1fr);
		}

		.yours {
			flex-direction: column;
			gap: 4px;
			text-align: center;
		}

		.yours :global(canvas) {
			margin-bottom: 8px;
		}

		.yours-text {
			align-items: center;
		}
	}
</style>
