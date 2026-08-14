<script lang="ts">
	import CopyableCommand from './CopyableCommand.svelte';
	import { INSTALL_METHODS } from '$lib/config';

	let { idPrefix = 'install' }: { idPrefix?: string } = $props();

	let active = $state(0);
	let tabEls = $state<(HTMLButtonElement | null)[]>([]);

	function focusTab(index: number) {
		const next = (index + INSTALL_METHODS.length) % INSTALL_METHODS.length;
		active = next;
		tabEls[next]?.focus();
	}

	function onKeydown(event: KeyboardEvent, index: number) {
		switch (event.key) {
			case 'ArrowRight':
				event.preventDefault();
				focusTab(index + 1);
				break;
			case 'ArrowLeft':
				event.preventDefault();
				focusTab(index - 1);
				break;
			case 'Home':
				event.preventDefault();
				focusTab(0);
				break;
			case 'End':
				event.preventDefault();
				focusTab(INSTALL_METHODS.length - 1);
				break;
		}
	}
</script>

<div class="term">
	<div class="tablist" role="tablist" aria-label="Installation method">
		{#each INSTALL_METHODS as method, i (method.id)}
			<button
				bind:this={tabEls[i]}
				role="tab"
				id={`${idPrefix}-tab-${method.id}`}
				aria-selected={active === i}
				aria-controls={`${idPrefix}-panel-${method.id}`}
				tabindex={active === i ? 0 : -1}
				class:active={active === i}
				onclick={() => (active = i)}
				onkeydown={(e) => onKeydown(e, i)}
			>
				{method.label}
			</button>
		{/each}
	</div>

	{#each INSTALL_METHODS as method, i (method.id)}
		<div
			role="tabpanel"
			id={`${idPrefix}-panel-${method.id}`}
			aria-labelledby={`${idPrefix}-tab-${method.id}`}
			hidden={active !== i}
		>
			{#if method.status === 'live' && method.lines}
				<CopyableCommand lines={method.lines} />
				{#if method.note}
					<div class="prereq">{method.note}</div>
				{/if}
			{:else}
				<div class="soon"><span class="dollar">$</span>&nbsp;{method.note ?? 'Coming soon.'}</div>
			{/if}
		</div>
	{/each}
</div>

<style>
	.term {
		width: 100%;
	}

	.tablist {
		display: flex;
		border-bottom: 1px solid var(--term-line);
	}

	.tablist button {
		font-family: var(--font-mono);
		font-size: 11.5px;
		background: transparent;
		border: none;
		border-right: 1px solid var(--term-line);
		border-bottom: 2px solid transparent;
		color: var(--term-dim);
		padding: 8px 14px 6px;
		cursor: pointer;
		margin-bottom: -1px;
	}

	.tablist button.active {
		color: var(--sb-accent);
		background: var(--term-active-bg);
		border-bottom-color: var(--sb-accent);
	}

	.tablist button:hover:not(.active) {
		color: var(--term-text);
	}

	.soon {
		padding: 11px 14px;
		font-size: 13px;
		color: var(--term-dim);
	}

	/* One quiet line under the live command: the prerequisites a visitor
	   needs before pasting it. Same ink as .soon so it reads as a footnote,
	   not a second prompt. */
	.prereq {
		padding: 0 14px 10px;
		font-size: 12px;
		color: var(--term-dim);
	}

	/* Quieter than a live prompt, but still a prompt: --term-control-hover is
	   a chrome ground, not an ink, and measured 1.32:1 on the peach paper. */
	.soon .dollar {
		color: var(--term-faint);
	}

	@media (max-width: 480px) {
		.tablist button {
			padding: 7px 10px 6px;
		}
	}
</style>
