<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import AgentMark from './AgentMark.svelte';
	import { REPO_URL } from '$lib/config';

	const MANIFESTS_URL = `${REPO_URL}/blob/main/docs/reference/MANIFESTS.md`;

	// The supported agents, in the three tiers docs/guides/install.md
	// documents. Each detected row is the CLI's command, the way a pane
	// names it, and the manifest's display name. Five manifests are measured
	// against a live CLI; the rest are written from vendor documentation and
	// say so in the file (version_tested = "unverified"). Skill-only products
	// run in no pane: they read the Cyclops skill and nothing else. The next
	// agent is the reader's, and it gets the mark.
	const verified = [
		{ cmd: 'claude', name: 'Claude Code' },
		{ cmd: 'codex', name: 'Codex CLI' },
		{ cmd: 'cursor-agent', name: 'Cursor Agent' },
		{ cmd: 'agy', name: 'Antigravity CLI' },
		{ cmd: 'kimi', name: 'Kimi Code CLI' }
	];
	const unverified = [
		{ cmd: 'gemini', name: 'Gemini CLI' },
		{ cmd: 'qwen', name: 'Qwen Code' },
		{ cmd: 'goose', name: 'goose' },
		{ cmd: 'opencode', name: 'OpenCode' },
		{ cmd: 'amp', name: 'Amp' },
		{ cmd: 'crush', name: 'Crush' },
		{ cmd: 'aider', name: 'aider' },
		{ cmd: 'adal', name: 'AdaL' },
		{ cmd: 'auggie', name: 'Auggie' },
		{ cmd: 'autohand', name: 'Autohand Code' },
		{ cmd: 'bob', name: 'IBM Bob Shell' },
		{ cmd: 'cline', name: 'Cline CLI' },
		{ cmd: 'codearts', name: 'CodeArts Agent' },
		{ cmd: 'codebuddy', name: 'CodeBuddy Code' },
		{ cmd: 'cmd', name: 'Command Code' },
		{ cmd: 'cn', name: 'Continue CLI' },
		{ cmd: 'copilot', name: 'GitHub Copilot CLI' },
		{ cmd: 'cortex', name: 'Cortex Code' },
		{ cmd: 'dcode', name: 'Deep Agents Code' },
		{ cmd: 'devin', name: 'Devin for Terminal' },
		{ cmd: 'dexto', name: 'Dexto' },
		{ cmd: 'droid', name: 'Droid' },
		{ cmd: 'forge', name: 'ForgeCode' },
		{ cmd: 'grok', name: 'Grok Build' },
		{ cmd: 'hermes', name: 'Hermes Agent' },
		{ cmd: 'iflow', name: 'iFlow CLI' },
		{ cmd: 'jazz', name: 'Jazz' },
		{ cmd: 'junie', name: 'Junie CLI' },
		{ cmd: 'kilo', name: 'Kilo CLI' },
		{ cmd: 'kimchi', name: 'Kimchi' },
		{ cmd: 'kiro-cli', name: 'Kiro CLI' },
		{ cmd: 'kode', name: 'Kode' },
		{ cmd: 'loaf', name: 'Loaf' },
		{ cmd: 'mcode', name: 'MiniMax Code CLI' },
		{ cmd: 'neovate', name: 'Neovate' },
		{ cmd: 'openclaw', name: 'OpenClaw' },
		{ cmd: 'openhands', name: 'OpenHands CLI' },
		{ cmd: 'pa', name: 'Posit Assistant TUI' },
		{ cmd: 'pi', name: 'Pi' },
		{ cmd: 'qoder', name: 'Qoder CLI' },
		{ cmd: 'qoderclicn', name: 'Qoder CN CLI' },
		{ cmd: 'reasonix', name: 'Reasonix' },
		{ cmd: 'acli', name: 'Rovo Dev' },
		{ cmd: 'tabnine', name: 'Tabnine CLI' },
		{ cmd: 'traecli', name: 'TraeCode CLI' },
		{ cmd: 'vibe', name: 'Mistral Vibe' },
		{ cmd: 'warp', name: 'Warp Agent CLI' }
	];
	const skillOnly = [
		'AiderDesk',
		'AstrBot',
		'Codemaker',
		'Code Studio',
		'Firebender',
		'inference.sh',
		'Lingma',
		'MCPJam',
		'Moxby',
		'Mux',
		'Ona',
		'Pochi',
		'Terramind',
		'Trae',
		'Windsurf',
		'ZCode',
		'Zencoder',
		'Zed'
	];
	const detectedCount = verified.length + unverified.length;
</script>

<section class="section">
	<SectionHead title="COMPATIBILITY" index="Any agent" />
	<div class="split">
		<div class="copy">
			<h3 class="statement">If it runs in your terminal,<br />it can run in Cyclops.</h3>
			<p class="lede">
				Cyclops detects supported agents automatically. Each one is described by a small manifest
				file: what its process is called, how it reports back, and how to tell when it's busy.
				{detectedCount} agents detected out of the box, five of them measured against a live CLI;
				{skillOnly.length} more IDEs and desktop apps get the Cyclops skill. Teaching Cyclops a new agent
				CLI is one file.
			</p>
		</div>
		<div class="panel card">
			<ul class="list" aria-label="Agents detected out of the box">
				<li class="head label">Verified against a live CLI</li>
				{#each verified as agent (agent.cmd)}
					<li class="agent">
						<span class="marker" aria-hidden="true">✓</span>
						<span class="cmd">{agent.cmd}</span>
						<span class="name">{agent.name}</span>
					</li>
				{/each}
				<li class="head label">Detected, wired from vendor docs (unverified)</li>
				{#each unverified as agent (agent.cmd)}
					<li class="agent">
						<span class="marker" aria-hidden="true">✓</span>
						<span class="cmd">{agent.cmd}</span>
						<span class="name">{agent.name}</span>
					</li>
				{/each}
				<li class="head label">Skill only, no pane</li>
				<li class="products">
					{#each skillOnly as product (product)}
						<span class="name">{product}</span>
					{/each}
				</li>
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

	.head + .head,
	.agent + .head {
		margin-top: 10px;
		border-top: 1px solid var(--line);
		padding-top: 16px;
	}

	/* The skill-only products run in no pane, so they get no command
	   column: one wrapped row of names under their own head. */
	.products {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 14px;
		padding: 4px 28px 10px;
	}

	/* The unverified tier is long; keep the card scannable. */
	.list {
		max-height: 520px;
		overflow-y: auto;
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
