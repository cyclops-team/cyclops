<script lang="ts">
	import { onDestroy, onMount } from 'svelte';

	let { lines }: { lines: string[] } = $props();

	// The fade at the row's end is only for a line that does not fit; a
	// line that fits shows whole, to the last character.
	let linesEl: HTMLDivElement;
	let overflowing = $state(false);

	onMount(() => {
		const measure = () => (overflowing = linesEl.scrollWidth > linesEl.clientWidth + 1);
		measure();
		const ro = new ResizeObserver(measure);
		ro.observe(linesEl);
		document.fonts?.ready.then(measure);
		return () => ro.disconnect();
	});

	type CopyState = 'idle' | 'copied' | 'failed';
	const COPY_LABEL: Record<CopyState, string> = {
		idle: 'copy',
		copied: 'copied',
		failed: 'failed'
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
	<div class="lines" class:overflowing bind:this={linesEl}>
		{#each lines as line (line)}
			<div class="line"><span class="dollar">$</span>&nbsp;{line}</div>
		{/each}
	</div>
	<!-- The control sits on the frame's top rule, at the end where the
	     workspace keeps a pane's own controls, so the line has the row. -->
	<button
		class:copied={copyState === 'copied'}
		class:failed={copyState === 'failed'}
		onclick={copy}
		aria-label="Copy install command"
		aria-live="polite"
	>
		[{COPY_LABEL[copyState]}]
	</button>
</div>

<style>
	.row {
		width: 100%;
		min-width: 0;
		padding: 13px 10px 11px 14px;
	}

	.lines {
		min-width: 0;
		flex: 1;
		overflow-x: auto;
		scrollbar-width: none;
	}

	.lines.overflowing {
		mask-image: linear-gradient(to right, black calc(100% - 18px), transparent);
		-webkit-mask-image: linear-gradient(to right, black calc(100% - 18px), transparent);
	}

	.lines::-webkit-scrollbar {
		display: none;
	}

	.line {
		font-size: 11.5px;
		line-height: 1.5;
		color: var(--term-text);
		white-space: pre;
	}

	.line + .line {
		margin-top: 3px;
	}

	/* The prompt takes the theme accent, not the site's brand green, which
	   is a light sage and does not hold on the peach terminal ground. */
	.dollar {
		color: var(--sb-accent);
	}

	button {
		position: absolute;
		z-index: 2;
		top: -8px;
		right: 6px;
		font-family: var(--font-mono);
		font-size: 11px;
		line-height: 15px;
		letter-spacing: -0.4px;
		background: var(--term-bg);
		border: none;
		color: var(--term-dim);
		padding: 0 3px;
		cursor: pointer;
	}

	button:hover {
		color: var(--term-text);
	}

	button:focus-visible {
		outline: 2px solid var(--sb-accent);
		outline-offset: 1px;
	}

	/* A completed copy is state.healthy, the same green a receipt uses. */
	button.copied {
		color: var(--sb-healthy);
		border-color: var(--sb-healthy);
	}

	button.failed {
		color: var(--accent-2);
		border-color: var(--accent-2);
	}
</style>
