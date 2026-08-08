<script lang="ts">
	import TerminalHeader from './TerminalHeader.svelte';

	// The sidebar's session tree. Workspace rows sit at the root in bold;
	// agent rows indent under an expanded one. Glyph and color come from
	// the same mapping the app uses (cyclops-theme state_token, and
	// cyclops-workspace decoration): working is state.healthy, idle is
	// state.quiet, needs-attention is state.needs_you.
	const tree = [
		{ kind: 'workspace', name: 'clops', arrow: '▾', selected: true },
		{ kind: 'agent', name: 'implementer', glyph: '●', state: 'working', role: 'coral' },
		{ kind: 'agent', name: 'reviewer', glyph: '○', state: 'quiet', role: 'blueberry' },
		{ kind: 'agent', name: 'tests', glyph: '⚠', state: 'needs', role: 'raspberry' },
		{ kind: 'workspace', name: 'website', arrow: '▸', selected: false }
	] as const;

	const tabs = [
		{ name: '1', selected: false },
		{ name: 'review', selected: true }
	];
</script>

<div
	class="term visual"
	role="img"
	aria-label="The Cyclops workspace in the sorbet theme: a sidebar listing the clops workspace with three named agents, a tab bar, and three panes whose borders carry each agent's name and state"
>
	<TerminalHeader title="cyclops — clops · sorbet" />

	<div class="shell">
		<aside class="sidebar">
			<div class="chips">
				<span class="chip on">Sessions</span>
				<span class="chip">Stream</span>
			</div>

			<div class="tree">
				{#each tree as row (row.name)}
					{#if row.kind === 'workspace'}
						<div class="row ws" class:sel={row.selected}>
							<span class="arrow">{row.arrow}</span>{row.name}
						</div>
					{:else}
						<div class="row agent">
							<span class="glyph {row.state}">{row.glyph}</span><span class="name {row.role}"
								>{row.name}</span
							>
						</div>
					{/if}
				{/each}
			</div>

			<div class="side-foot">
				<span>☰ menu</span>
				<span class="plus-sm">+</span>
			</div>
			<!-- The chevron that collapses the panel, on its outer edge. -->
			<span class="collapse">◂</span>
		</aside>

		<div class="main">
			<div class="tabbar">
				{#each tabs as tab (tab.name)}
					<span class="tab" class:sel={tab.selected}>{tab.name}</span>
				{/each}
				<span class="plus">+</span>
			</div>

			<div class="canvas">
				<!-- The focused pane: accent border, full state word, split
				     controls, the drag grip, and a copy notice on the bottom. -->
				<div class="pane focus wide">
					<span class="label">implementer<span class="sep">·</span><span class="glyph working"
							>●</span
						>working</span
					>
					<span class="ctl">[|][-]</span>
					<div class="body">
						<div class="dim">$ claude</div>
						<div>Implementing the rate limiter…</div>
						<div class="ok">✓ 3 files changed</div>
					</div>
					<span class="notice">copied 42 characters</span>
					<span class="grip">⠿</span>
				</div>

				<div class="pane">
					<span class="label">reviewer<span class="sep">·</span><span class="glyph quiet">○</span
						></span
					>
					<div class="body">
						<div class="dim">$ codex</div>
						<div class="msg">[cyclops m-2f304e]</div>
						<div class="msg">FROM: admin</div>
					</div>
					<span class="grip">⠿</span>
				</div>

				<div class="pane">
					<span class="label">tests<span class="sep">·</span><span class="glyph needs">⚠</span
						></span
					>
					<div class="body">
						<div class="dim">$ cursor-agent</div>
						<div>auth.spec failing</div>
						<div class="warn">⚠ waiting on you</div>
					</div>
					<span class="grip">⠿</span>
				</div>
			</div>
		</div>
	</div>
</div>

<style>
	.visual {
		overflow: hidden;
		box-shadow: 0 18px 44px rgba(58, 43, 38, 0.18);
	}

	.shell {
		display: grid;
		grid-template-columns: 148px 1fr;
		min-height: 268px;
	}

	/* ---- sidebar ---- */

	.sidebar {
		position: relative;
		display: flex;
		flex-direction: column;
		background: var(--sb-panel);
		border-right: 1px solid var(--term-line);
		padding: 8px 0 0;
		font-size: 11px;
	}

	.chips {
		display: flex;
		gap: 4px;
		padding: 0 8px 8px;
	}

	.chip {
		padding: 2px 7px;
		color: var(--term-dim);
		font-size: 10px;
	}

	.chip.on {
		background: var(--sb-raised);
		color: var(--term-text);
		font-weight: 600;
	}

	.tree {
		flex: 1;
		padding: 2px 0;
	}

	.row {
		padding: 3px 8px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	/* Workspace rows are bold at the root; the selected one takes the
	   raised ground, the same stronger fill the app gives it. */
	.row.ws {
		font-weight: 700;
		color: var(--term-text);
	}

	.row.ws.sel {
		background: var(--sb-raised);
	}

	.arrow {
		display: inline-block;
		width: 12px;
		color: var(--term-dim);
	}

	.row.agent {
		padding-left: 20px;
	}

	.glyph {
		display: inline-block;
		width: 13px;
	}

	.glyph.working {
		color: var(--sb-healthy);
	}

	.glyph.quiet {
		color: var(--term-dim);
	}

	.glyph.needs {
		color: var(--sb-needs);
	}

	.name.coral {
		color: var(--sb-role-1);
	}

	.name.blueberry {
		color: var(--sb-role-6);
	}

	.name.raspberry {
		color: var(--sb-role-8);
	}

	.side-foot {
		display: flex;
		justify-content: space-between;
		padding: 7px 8px;
		border-top: 1px solid var(--term-line);
		color: var(--term-dim);
		font-size: 10px;
	}

	.plus-sm {
		color: var(--sb-accent);
		font-weight: 700;
	}

	/* Bottom of the sidebar's outer edge, where the app puts it. */
	.collapse {
		position: absolute;
		right: 2px;
		bottom: 28px;
		color: var(--term-dim);
		font-size: 10px;
	}

	/* ---- tab bar ---- */

	.main {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}

	.tabbar {
		display: flex;
		align-items: center;
		gap: 2px;
		padding: 5px 8px;
		background: var(--sb-panel);
		border-bottom: 1px solid var(--term-line);
		font-size: 10.5px;
	}

	.tab {
		padding: 2px 9px;
		color: var(--term-dim);
	}

	.tab.sel {
		background: var(--sb-raised);
		color: var(--term-text);
		font-weight: 600;
	}

	/* The filled + is always on the strip, whatever the tab count. */
	.plus {
		margin-left: 4px;
		padding: 1px 7px;
		background: var(--sb-accent);
		color: #fff6ed;
		font-weight: 700;
	}

	/* ---- pane canvas ---- */

	/* The gap has to clear the labels and the notice, which hang off the
	   borders rather than sitting inside the panes. */
	.canvas {
		flex: 1;
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 15px 9px;
		padding: 10px 9px 9px;
		align-content: start;
	}

	.pane {
		position: relative;
		border: 1px solid #e0c4ae;
		padding: 11px 10px 10px;
		min-width: 0;
		min-height: 84px;
	}

	.wide {
		grid-column: 1 / -1;
	}

	/* The focused pane's border takes the accent; inactive borders stay
	   muted. Same rule the app follows. */
	.pane.focus {
		border-color: var(--sb-accent);
	}

	/* A pane carries its identity in the top border itself, so the label
	   sits on the rule with the pane's own ground knocked out behind it. */
	.label {
		position: absolute;
		z-index: 2;
		top: -7px;
		left: 9px;
		padding: 0 5px;
		background: var(--term-bg);
		font-size: 10.5px;
		color: var(--term-text);
		white-space: nowrap;
	}

	.sep {
		margin: 0 4px;
		color: var(--term-faint);
	}

	.label .glyph {
		width: auto;
		margin-right: 4px;
	}

	.ctl {
		position: absolute;
		z-index: 2;
		top: -7px;
		right: 9px;
		padding: 0 5px;
		background: var(--term-bg);
		font-size: 10.5px;
		color: var(--sb-accent);
		letter-spacing: -0.5px;
	}

	/* The one cell that picks a pane up. */
	.grip {
		position: absolute;
		z-index: 2;
		right: 4px;
		bottom: -8px;
		padding: 0 2px;
		background: var(--term-bg);
		color: var(--term-faint);
		font-size: 11px;
		line-height: 1;
	}

	/* A copy says what it took, on the focused pane's bottom border. */
	.notice {
		position: absolute;
		z-index: 2;
		bottom: -7px;
		left: 9px;
		padding: 0 5px;
		background: var(--term-bg);
		color: var(--sb-healthy);
		font-size: 10px;
		white-space: nowrap;
	}

	.body {
		font-size: 11px;
		line-height: 1.75;
		color: var(--term-text);
	}

	.body div {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.body .dim {
		color: var(--term-dim);
	}

	.body .ok {
		color: var(--sb-healthy);
	}

	.body .warn {
		color: var(--sb-needs);
	}

	.body .msg {
		color: var(--sb-role-7);
	}

	@media (max-width: 560px) {
		.shell {
			grid-template-columns: 116px 1fr;
		}

		.canvas {
			grid-template-columns: 1fr;
		}

		.wide {
			grid-column: auto;
		}
	}
</style>
