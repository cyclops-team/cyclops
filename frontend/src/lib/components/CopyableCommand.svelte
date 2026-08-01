<script lang="ts">
	import { onDestroy } from 'svelte';

	let { lines }: { lines: string[] } = $props();

	type CopyState = 'idle' | 'copied' | 'failed';
	const COPY_LABEL: Record<CopyState, string> = {
		idle: 'Copy',
		copied: 'Copied',
		failed: 'Failed'
	};

	let copyState = $state<CopyState>('idle');
	let resetTimer: ReturnType<typeof setTimeout> | undefined;

	async function copy() {
		clearTimeout(resetTimer);
		try {
			if (!navigator.clipboard) throw new Error('Clipboard API unavailable');
			await navigator.clipboard.writeText(lines.join('\n'));
			copyState = 'copied';
		} catch {
			copyState = 'failed';
		}
		resetTimer = setTimeout(() => (copyState = 'idle'), 1500);
	}

	onDestroy(() => clearTimeout(resetTimer));
</script>

<div class="row">
	<div class="lines">
		{#each lines as line (line)}
			<div class="line"><span class="dollar">$</span>&nbsp;{line}</div>
		{/each}
	</div>
	<button
		class:copied={copyState === 'copied'}
		class:failed={copyState === 'failed'}
		onclick={copy}
		aria-label="Copy install command"
		aria-live="polite"
	>
		{COPY_LABEL[copyState]}
	</button>
</div>

<style>
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 14px;
		padding: 11px 12px 11px 14px;
	}

	.lines {
		min-width: 0;
		flex: 1;
		overflow-x: auto;
		scrollbar-width: none;
		mask-image: linear-gradient(to right, black calc(100% - 18px), transparent);
		-webkit-mask-image: linear-gradient(to right, black calc(100% - 18px), transparent);
	}

	.lines::-webkit-scrollbar {
		display: none;
	}

	.line {
		font-size: 13px;
		line-height: 1.5;
		color: var(--term-text);
		white-space: pre;
	}

	.line + .line {
		margin-top: 3px;
	}

	.dollar {
		color: var(--sage);
	}

	button {
		font-family: var(--font-mono);
		font-size: 11.5px;
		background: var(--term-control);
		border: 1px solid var(--term-line);
		color: var(--term-dim);
		padding: 5px 10px;
		cursor: pointer;
		flex-shrink: 0;
		line-height: 1.4;
	}

	button:hover {
		border-color: var(--term-control-hover);
		color: var(--term-text);
	}

	button.copied {
		color: var(--sage);
		border-color: var(--sage);
	}

	button.failed {
		color: var(--mauve);
		border-color: var(--mauve);
	}
</style>
