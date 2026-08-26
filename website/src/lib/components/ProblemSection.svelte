<script lang="ts">
	import SectionHead from './SectionHead.svelte';

	// One problem per row and, beside it, what Cyclops does about it. The
	// rows are the argument, so the before and after of one row must be
	// about the same thing.
	const rows = [
		{ before: 'A separate terminal window per agent', after: "Every agent's pane in one workspace" },
		{ before: 'Copy-paste to hand context between them', after: 'An agent hands off with one command' },
		{ before: 'No way for agents to reach each other', after: 'Agent-to-agent messages, with receipts' },
		{ before: 'Panes with no name and no state', after: 'Named panes that show their state' }
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
	<div class="panel compare" role="table" aria-label="Before and with Cyclops, row by row">
		<div class="row head" role="row">
			<div class="cell label" role="columnheader">Before Cyclops</div>
			<div class="cell label accent" role="columnheader">With Cyclops</div>
		</div>
		{#each rows as row (row.before)}
			<div class="row" role="row">
				<div class="cell before" role="cell">
					<span class="marker" aria-hidden="true">✕</span>{row.before}
				</div>
				<div class="cell after" role="cell">
					<span class="marker" aria-hidden="true">✓</span>{row.after}
				</div>
			</div>
		{/each}
	</div>
</section>

<style>
	.lede {
		margin-bottom: 40px;
	}

	.compare {
		padding: 0;
	}

	.row {
		display: grid;
		grid-template-columns: 1fr 1fr;
	}

	.row + .row {
		border-top: 1px solid var(--line-soft);
	}

	.row.head + .row {
		border-top: 1px solid var(--line);
	}

	.cell {
		display: flex;
		align-items: baseline;
		gap: 12px;
		min-width: 0;
		padding: 14px 32px;
		font-size: 14px;
		color: var(--text);
	}

	.cell:first-child {
		border-right: 1px solid var(--line);
	}

	.head .cell {
		padding: 28px 32px 16px;
	}

	.label.accent {
		color: var(--accent);
	}

	.row:last-child .cell {
		padding-bottom: 28px;
	}

	.marker {
		font-size: 12px;
		flex-shrink: 0;
		width: 14px;
	}

	.before .marker {
		color: var(--faint);
	}

	.after .marker {
		color: var(--accent);
	}

	@media (max-width: 720px) {
		/* Narrow: each row stacks its own before over its after, so the
		   pairing survives; the column heads become a legend. */
		.row {
			grid-template-columns: 1fr;
		}

		.cell {
			padding: 12px 24px;
		}

		.cell:first-child {
			border-right: none;
		}

		/* The heads sit together as a legend for the markers below; no
		   divider, since there are no columns to divide. */
		.head {
			grid-template-columns: auto auto;
			justify-content: start;
			column-gap: 20px;
		}

		.head .cell {
			padding: 20px 0 12px 24px;
		}

		.head .cell:first-child {
			border-right: none;
		}

		.head .cell + .cell {
			padding-left: 0;
		}

		.row:not(.head) .before {
			padding-bottom: 4px;
			color: var(--muted);
		}

		.row:not(.head) .after {
			padding-top: 4px;
		}

		.row:last-child .before {
			padding-bottom: 4px;
		}

		.row:last-child .after {
			padding-bottom: 20px;
		}
	}
</style>
