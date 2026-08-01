<script lang="ts">
	import TerminalHeader from './TerminalHeader.svelte';

	const panes = [
		{
			role: 'implementer',
			dot: 'var(--sage)',
			lines: [
				{ cls: 'dim', text: '$ claude' },
				{ cls: 'out', text: 'Implementing rate limiter…' },
				{ cls: 'ok', text: '✓ 3 files changed' }
			]
		},
		{
			role: 'reviewer',
			dot: 'var(--mauve)',
			lines: [
				{ cls: 'dim', text: '$ codex' },
				{ cls: 'msg', text: '» implementer: review the rate limiter' },
				{ cls: 'ok', text: '✓ delivered · verified' }
			]
		},
		{
			role: 'tests',
			dot: 'var(--sage)',
			lines: [
				{ cls: 'dim', text: '$ gemini' },
				{ cls: 'out', text: 'Investigating auth.spec failure…' },
				{ cls: 'out', text: '→ root cause: stale token cache' }
			]
		},
		{
			role: 'operator',
			dot: 'var(--term-neutral)',
			lines: [
				{ cls: 'dim', text: '$ cyclops list' },
				{ cls: 'out', text: 'implementer  active   rate-limiter' },
				{ cls: 'out', text: 'reviewer     active   reviewing' },
				{ cls: 'out', text: 'tests        active   investigating' }
			]
		}
	];
</script>

<div class="term visual" role="img" aria-label="A Cyclops terminal workspace with four labeled agent panes — implementer, reviewer, tests, and operator — coordinating on a single change">
	<TerminalHeader title="cyclops — one workspace, four agents" />
	<div class="grid">
		{#each panes as pane (pane.role)}
			<div class="pane">
				<div class="pane-head">
					<span class="pane-dot" style="background: {pane.dot}"></span>
					<span class="pane-role">{pane.role}</span>
				</div>
				<div class="pane-body">
					{#each pane.lines as line (line.text)}
						<div class={line.cls}>{line.text}</div>
					{/each}
				</div>
			</div>
		{/each}
	</div>
</div>

<style>
	.visual {
		overflow: hidden;
	}

	.grid {
		display: grid;
		grid-template-columns: 1fr 1fr;
		grid-template-rows: 1fr 1fr;
	}

	.pane {
		border-right: 1px solid var(--term-line);
		border-bottom: 1px solid var(--term-line);
		padding: 14px 16px;
		min-width: 0;
	}

	.pane:nth-child(2n) {
		border-right: none;
	}

	.pane:nth-child(n + 3) {
		border-bottom: none;
	}

	.pane-head {
		display: flex;
		align-items: center;
		gap: 6px;
		margin-bottom: 10px;
	}

	.pane-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		flex-shrink: 0;
	}

	.pane-role {
		font-size: 10px;
		letter-spacing: 1px;
		text-transform: uppercase;
		color: var(--term-faint);
	}

	.pane-body {
		font-size: 11.5px;
		line-height: 1.85;
	}

	.pane-body div {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.pane-body .dim {
		color: var(--term-dim);
	}

	.pane-body .out {
		color: var(--term-text);
	}

	.pane-body .ok {
		color: var(--sage);
	}

	.pane-body .msg {
		color: var(--mauve);
	}

	@media (max-width: 560px) {
		.grid {
			grid-template-columns: 1fr;
		}

		.pane {
			border-right: none;
		}

		.pane:nth-child(n + 3) {
			border-bottom: 1px solid var(--term-line);
		}

		.pane:last-child {
			border-bottom: none;
		}
	}
</style>
