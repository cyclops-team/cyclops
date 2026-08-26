<script lang="ts">
	import { onMount } from 'svelte';
	import { emitSignal } from '$lib/signal';

	// The workspace, drawn the way the app draws it: the sidebar's session
	// tree over its files browser and footer, the tab strip, and a pane
	// canvas whose frames carry each agent's name and state in the top
	// border. The focused frame takes the double rule in the accent, the
	// others a rounded single rule, exactly as cyclops-workspace paints
	// them (render/canvas.rs, render/sidebar.rs, render/tab_bar.rs).
	//
	// It is live: the sidebar rows, the frames and the tabs answer a click,
	// and a scripted handoff runs on a loop — implementer sends reviewer a
	// message through the cyclops CLI, the receipt lands, reviewer wakes and
	// replies. The script moves focus only until the visitor takes it.

	type AgentId = 'planner' | 'implementer' | 'reviewer' | 'tests';
	type AgentState = 'working' | 'idle' | 'needs';
	type LineClass = 'dim' | 'cmd' | 'ok' | 'warn' | 'msg' | 'out';
	interface Line {
		cls: LineClass;
		text: string;
	}

	interface Agent {
		id: AgentId;
		role: number;
		cli: string;
		/** Index of this agent's node in the signal field, if it has one. */
		node?: number;
	}

	// Role colors come off the theme's wheel the way the app assigns them.
	const AGENTS: Agent[] = [
		{ id: 'planner', role: 6, cli: 'claude' },
		{ id: 'implementer', role: 1, cli: 'claude', node: 0 },
		{ id: 'reviewer', role: 4, cli: 'codex', node: 1 },
		{ id: 'tests', role: 3, cli: 'cursor-agent', node: 2 }
	];

	const TABS: { name: string; panes: AgentId[] }[] = [
		{ name: '1', panes: ['planner'] },
		{ name: 'review', panes: ['implementer', 'reviewer', 'tests'] }
	];

	// decoration::primary_status, verbatim.
	const STATUS: Record<AgentState, { glyph: string; word: string }> = {
		working: { glyph: '●', word: 'working' },
		idle: { glyph: '○', word: 'idle' },
		needs: { glyph: '⚠', word: 'needs attention' }
	};

	const MAX_LINES: Record<AgentId, number> = { planner: 5, implementer: 6, reviewer: 3, tests: 3 };

	function initialLines(): Record<AgentId, Line[]> {
		return {
			planner: [
				{ cls: 'dim', text: '$ claude' },
				{ cls: 'out', text: 'Sprint plan written to PLAN.md' },
				{ cls: 'out', text: 'Handed the rate limiter to implementer.' }
			],
			implementer: [
				{ cls: 'dim', text: '$ claude' },
				{ cls: 'out', text: 'Implementing the rate limiter…' },
				{ cls: 'ok', text: '✓ 3 files changed' }
			],
			reviewer: [{ cls: 'dim', text: '$ codex' }],
			tests: [
				{ cls: 'dim', text: '$ cursor-agent' },
				{ cls: 'out', text: 'auth.spec failing' },
				{ cls: 'warn', text: '⚠ waiting on you' }
			]
		};
	}

	let tab = $state('review');
	let focused = $state<AgentId>('implementer');
	let autopilot = $state(true);
	let states = $state<Record<AgentId, AgentState>>({
		planner: 'idle',
		implementer: 'working',
		reviewer: 'idle',
		tests: 'needs'
	});
	let lines = $state<Record<AgentId, Line[]>>(initialLines());
	let notice = $state<string | null>(null);

	const panes = $derived(TABS.find((t) => t.name === tab)?.panes ?? []);

	function agentOf(id: AgentId): Agent {
		return AGENTS.find((a) => a.id === id) ?? AGENTS[0];
	}

	function tabOf(id: AgentId): string {
		return TABS.find((t) => t.panes.includes(id))?.name ?? tab;
	}

	// ---- the visitor's hand ----

	function select(id: AgentId) {
		autopilot = false;
		focused = id;
		tab = tabOf(id);
	}

	function selectTab(name: string) {
		autopilot = false;
		tab = name;
		const inTab = TABS.find((t) => t.name === name)?.panes ?? [];
		if (!inTab.includes(focused)) focused = inTab[0] ?? focused;
	}

	// ---- the script ----

	function push(id: AgentId, line: Line) {
		lines[id] = [...lines[id], line].slice(-MAX_LINES[id]);
	}

	function replaceLast(id: AgentId, line: Line) {
		lines[id] = [...lines[id].slice(0, -1), line];
	}

	function focus(id: AgentId) {
		if (!autopilot) return;
		focused = id;
		tab = tabOf(id);
	}

	function sleep(ms: number, signal: AbortSignal): Promise<void> {
		return new Promise((resolve) => {
			if (signal.aborted) return resolve();
			const timer = setTimeout(done, ms);
			function done() {
				signal.removeEventListener('abort', done);
				clearTimeout(timer);
				resolve();
			}
			signal.addEventListener('abort', done, { once: true });
		});
	}

	async function type(id: AgentId, text: string, signal: AbortSignal) {
		push(id, { cls: 'cmd', text: '' });
		for (let i = 1; i <= text.length; i++) {
			if (signal.aborted) return;
			replaceLast(id, { cls: 'cmd', text: text.slice(0, i) });
			await sleep(22 + Math.random() * 26, signal);
		}
	}

	function reset() {
		lines = initialLines();
		states.reviewer = 'idle';
		states.implementer = 'working';
		notice = null;
	}

	async function run(signal: AbortSignal) {
		while (!signal.aborted) {
			reset();
			await sleep(1800, signal);
			focus('implementer');

			await type('implementer', '$ cyclops send reviewer --subject "Review the rate limiter"', signal);
			await sleep(420, signal);
			push('implementer', { cls: 'ok', text: '✔ delivered · verified' });
			notice = 'delivered · m-2f304e';
			emitSignal(0, 1);
			await sleep(520, signal);
			lines.reviewer = [
				{ cls: 'dim', text: '$ codex' },
				{ cls: 'msg', text: '[cyclops m-2f304e] FROM: implementer' },
				{ cls: 'msg', text: 'SUBJECT: Review the rate limiter' }
			];
			states.reviewer = 'working';
			await sleep(1500, signal);
			notice = null;
			focus('reviewer');

			await sleep(900, signal);
			push('reviewer', { cls: 'out', text: 'Reading src/limiter.rs…' });
			await sleep(1500, signal);
			push('reviewer', { cls: 'ok', text: '✓ LGTM · one nit on naming' });
			await sleep(800, signal);
			await type('reviewer', '$ cyclops send --reply-to m-2f304e', signal);
			await sleep(420, signal);
			push('reviewer', { cls: 'ok', text: '✔ delivered · verified' });
			emitSignal(1, 0);
			await sleep(520, signal);
			push('implementer', { cls: 'msg', text: '[cyclops m-91ab07] FROM: reviewer' });
			push('implementer', { cls: 'msg', text: 'SUBJECT: Re: Review the rate limiter' });
			states.reviewer = 'idle';
			await sleep(1200, signal);
			focus('implementer');
			await sleep(3600, signal);
		}
	}

	onMount(() => {
		if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
			// The moment after the handoff, held still.
			push('implementer', { cls: 'cmd', text: '$ cyclops send reviewer --subject "Review the rate limiter"' });
			push('implementer', { cls: 'ok', text: '✔ delivered · verified' });
			lines.reviewer = [
				{ cls: 'dim', text: '$ codex' },
				{ cls: 'msg', text: '[cyclops m-2f304e] FROM: implementer' },
				{ cls: 'msg', text: 'SUBJECT: Review the rate limiter' }
			];
			states.reviewer = 'working';
			notice = 'delivered · m-2f304e';
			return;
		}
		const controller = new AbortController();
		run(controller.signal);
		return () => controller.abort();
	});
</script>

<div
	class="term visual"
	role="group"
	aria-label="A live mock of the Cyclops workspace: a sidebar listing the cyclops session with four named agents, a tab strip, and pane frames whose borders carry each agent's name and state"
>
	<div class="shell">
		<aside class="sidebar">
			<div class="sb-head">
				<span class="sb-tab">Sessions</span>
				<span class="wordmark" aria-hidden="true">[<span class="eye">></span> -]</span>
			</div>

			<div class="tree">
				<div class="row ws sel"><span class="arrow">▾</span>cyclops</div>
				{#each AGENTS as agent (agent.id)}
					<button
						type="button"
						class="row agent"
						class:sel={focused === agent.id}
						onclick={() => select(agent.id)}
						aria-pressed={focused === agent.id}
						aria-label={`Focus ${agent.id}, ${STATUS[states[agent.id]].word}`}
					>
						<span class="glyph {states[agent.id]}" aria-hidden="true"
							>{STATUS[states[agent.id]].glyph}</span
						><span class="name" style="color: var(--sb-role-{agent.role})">{agent.id}</span>
					</button>
				{/each}
				<div class="row ws"><span class="arrow">▸</span>website</div>
			</div>

			<div class="files" aria-hidden="true">
				<div class="rule"></div>
				<div class="files-head"><span class="dir">cyclops</span><span class="pin">[pin]</span></div>
				<div class="files-up"><span>..</span><span class="arrows">◂ ▸</span></div>
				<div class="file"><span class="ext">(rs)</span> limiter.rs</div>
				<div class="file"><span class="ext">(md)</span> PLAN.md</div>
				<div class="more">+6 more</div>
				<div class="rule"></div>
			</div>

			<div class="side-foot" aria-hidden="true">
				<span>☰menu</span>
				<span class="foot-ctl"><span class="at">@</span> <span class="plus">+</span></span>
			</div>
			<span class="collapse" aria-hidden="true">◂</span>
		</aside>

		<div class="main">
			<div class="tabbar" role="tablist" aria-label="Tabs">
				{#each TABS as t (t.name)}
					<button
						type="button"
						role="tab"
						class="tab"
						class:sel={tab === t.name}
						aria-selected={tab === t.name}
						onclick={() => selectTab(t.name)}>{t.name}</button
					>
				{/each}
				<span class="tab new" aria-hidden="true">+</span>
			</div>

			<div class="canvas" class:single={panes.length === 1}>
				{#each panes as id, i (id)}
					{@const agent = agentOf(id)}
					{@const status = STATUS[states[id]]}
					{@const isFocused = focused === id}
					{@const stacked = panes.length === 3 && i > 0}
					{@const wide = !stacked}
					<div
						class="frame"
						class:focus={isFocused}
						class:tall={panes.length === 3 && i === 0}
						onclick={() => select(id)}
						role="presentation"
					>
						{#if stacked && !isFocused}
							<span class="frame-min" aria-hidden="true">[▴]</span>
						{/if}
						<span class="frame-title">
							<span style="color: var(--sb-role-{agent.role})">{id}</span><span class="sep">&nbsp;·&nbsp;</span><span class="state {states[id]}"
								>{status.glyph}{#if isFocused && wide}<span class="word">&nbsp;{status.word}</span>{/if}</span
							>
						</span>
						<span class="frame-ctl" aria-hidden="true">[⠿][|][-]</span>
						<div class="frame-body">
							{#each lines[id] as line, n (n)}
								<div class={line.cls}>{line.text}</div>
							{/each}
						</div>
						{#if isFocused && notice}
							<span class="frame-notice">{notice}</span>
						{/if}
					</div>
				{/each}
			</div>
		</div>
	</div>
</div>

<style>
	.visual {
		overflow: hidden;
		box-shadow: 0 24px 56px var(--term-shadow);
		font-size: 10.5px;
		line-height: 17px;
	}

	.shell {
		display: grid;
		grid-template-columns: 126px 1fr;
	}

	/* ---- sidebar ---- */

	.sidebar {
		position: relative;
		display: flex;
		flex-direction: column;
		background: var(--sb-panel);
		padding: 6px 0 0;
		min-width: 0;
	}

	.sb-head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		padding: 0 10px 6px;
		color: var(--term-dim);
	}

	.wordmark {
		color: var(--term-dim);
		letter-spacing: -0.3px;
	}

	.wordmark .eye {
		color: var(--sb-accent);
		font-weight: 700;
	}

	.tree {
		padding: 4px 0 6px;
	}

	.row {
		display: block;
		width: 100%;
		padding: 0 10px;
		text-align: left;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		font: inherit;
		line-height: 18px;
		border: none;
		background: transparent;
		color: var(--term-dim);
	}

	/* Workspace rows are bold at the root; the open one takes the raised
	   ground, and so does the agent row that holds focus. */
	.row.ws {
		font-weight: 700;
		color: var(--term-text);
	}

	.row.sel {
		background: var(--sb-raised);
	}

	.arrow {
		display: inline-block;
		width: 14px;
		font-weight: 400;
		color: var(--term-dim);
	}

	.row.agent {
		padding-left: 24px;
		cursor: pointer;
	}

	.row.agent:hover:not(.sel) {
		background: color-mix(in srgb, var(--sb-raised) 50%, transparent);
	}

	.row.agent:focus-visible {
		outline: 2px solid var(--sb-accent);
		outline-offset: -2px;
	}

	.glyph {
		display: inline-block;
		width: 14px;
	}

	.glyph.working,
	.state.working {
		color: var(--sb-healthy);
	}

	.glyph.idle,
	.state.idle {
		color: var(--term-dim);
	}

	/* Attention is painted in the eye's alert color: the accent. */
	.glyph.needs,
	.state.needs {
		color: var(--sb-accent);
	}

	.files {
		margin-top: auto;
		padding: 0 10px;
		color: var(--term-dim);
		line-height: 17px;
	}

	.rule {
		border-top: 1px solid var(--term-pane-line);
		margin: 4px 0;
	}

	.files-head,
	.files-up {
		display: flex;
		justify-content: space-between;
	}

	.dir {
		font-weight: 700;
		color: var(--term-text);
	}

	.pin,
	.arrows {
		color: var(--term-faint);
	}

	.file {
		padding-left: 10px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.ext {
		color: var(--term-faint);
	}

	.more {
		text-align: right;
		color: var(--term-faint);
	}

	.side-foot {
		display: flex;
		justify-content: space-between;
		padding: 2px 10px 6px;
		color: var(--term-dim);
	}

	.side-foot .at,
	.side-foot .plus {
		color: var(--sb-accent);
		font-weight: 700;
	}

	.foot-ctl {
		letter-spacing: 2px;
	}

	/* The chevron that collapses the panel, on its outer edge. */
	.collapse {
		position: absolute;
		right: 0;
		top: 50%;
		transform: translateY(-50%);
		color: var(--sb-accent);
		font-size: 9px;
	}

	/* ---- tab strip ---- */

	.main {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}

	.tabbar {
		display: flex;
		align-items: center;
		gap: 5px;
		padding: 4px 8px 4px 10px;
		background: var(--sb-panel);
	}

	.tab {
		padding: 0 7px;
		line-height: 17px;
		font: inherit;
		line-height: 17px;
		border: none;
		background: transparent;
		color: var(--term-dim);
		cursor: pointer;
	}

	.tab.sel {
		background: var(--sb-accent);
		color: var(--term-bg);
		font-weight: 700;
	}

	.tab:focus-visible {
		outline: 2px solid var(--sb-accent);
		outline-offset: 1px;
	}

	/* The new-tab chip: the accent on the raised ground. */
	.tab.new {
		background: var(--sb-raised);
		color: var(--sb-accent);
		font-weight: 700;
		cursor: default;
	}

	/* ---- pane canvas ---- */

	/* The gaps have to clear the titles and the notice, which hang off the
	   borders rather than sitting inside the frames. */
	.canvas {
		flex: 1;
		display: grid;
		grid-template-columns: 1.25fr 1fr;
		grid-template-rows: 1fr 1fr;
		gap: 18px 8px;
		padding: 13px 6px 12px;
		min-height: 340px;
	}

	.canvas.single {
		grid-template-columns: 1fr;
		grid-template-rows: 1fr;
	}

	.frame {
		min-width: 0;
		min-height: 0;
		cursor: pointer;
	}

	.frame.tall {
		grid-row: 1 / -1;
	}

	/* One extra pixel of padding on a resting frame so its body does not
	   shift when the double rule arrives. */
	.frame:not(.focus) {
		padding: 2px;
	}

	.frame-body {
		padding: 10px 10px 8px;
		font-size: 10.5px;
		line-height: 1.7;
		overflow: hidden;
	}

	/* A terminal wraps; it does not ellipsize. */
	.frame-body div {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}

	/* The typed command is the pane's own ink; the prompt lines are dim. */
	.frame-body .cmd {
		color: var(--term-text);
	}

	.frame-title .state {
		font-weight: 400;
	}

	@media (max-width: 560px) {
		.shell {
			grid-template-columns: 118px 1fr;
		}

		.files {
			display: none;
		}

		.canvas {
			grid-template-columns: 1fr;
			grid-template-rows: auto;
		}

		.frame.tall {
			grid-row: auto;
		}

		/* A phone-width pane has no room for the word, only the glyph. */
		.frame-title .word {
			display: none;
		}
	}
</style>
