<script lang="ts">
	import { onMount } from 'svelte';

	// Step 02 as it actually happens in the workspace: an agent you already
	// use runs in a focused pane, Ctrl+B m opens the Name pane dialog, the
	// name lands in the pane's border, and then you talk to it in plain
	// words. Copy in the dialog is the workspace's own (copy.rs).
	type Beat = 'start' | 'dialog' | 'named' | 'ask';

	const NAME = 'reviewer';
	const ASK = "You're the reviewer. When implementer sends a diff, review it and reply.";

	let beat = $state<Beat>('start');
	let typed = $state('');
	let ask = $state('');
	let saving = $state(false);

	const named = $derived(beat === 'named' || beat === 'ask');

	function sleep(ms: number, signal: AbortSignal) {
		return new Promise<void>((resolve, reject) => {
			const t = setTimeout(resolve, ms);
			signal.addEventListener('abort', () => {
				clearTimeout(t);
				reject(signal.reason);
			});
		});
	}

	async function type(text: string, ms: number, into: (s: string) => void, signal: AbortSignal) {
		for (let i = 1; i <= text.length; i++) {
			into(text.slice(0, i));
			await sleep(ms, signal);
		}
	}

	async function run(signal: AbortSignal) {
		try {
			while (!signal.aborted) {
				beat = 'start';
				typed = '';
				ask = '';
				saving = false;
				await sleep(1600, signal);
				beat = 'dialog';
				await sleep(700, signal);
				await type(NAME, 95, (s) => (typed = s), signal);
				await sleep(650, signal);
				saving = true;
				await sleep(220, signal);
				beat = 'named';
				await sleep(1100, signal);
				beat = 'ask';
				await type(ASK, 26, (s) => (ask = s), signal);
				await sleep(4200, signal);
			}
		} catch {
			// aborted: unmounted or scrolled away
		}
	}

	function still() {
		beat = 'ask';
		typed = NAME;
		ask = ASK;
	}

	let root: HTMLDivElement;

	onMount(() => {
		if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
			still();
			return;
		}
		let controller: AbortController | null = null;
		const start = () => {
			if (controller) return;
			controller = new AbortController();
			run(controller.signal);
		};
		const stop = () => {
			controller?.abort();
			controller = null;
		};
		let visible = false;
		const io = new IntersectionObserver(
			([entry]) => {
				visible = entry.isIntersecting;
				if (visible && !document.hidden) start();
				else stop();
			},
			{ threshold: 0.2 }
		);
		io.observe(root);
		const onVis = () => {
			if (document.hidden) stop();
			else if (visible) start();
		};
		document.addEventListener('visibilitychange', onVis);
		return () => {
			stop();
			io.disconnect();
			document.removeEventListener('visibilitychange', onVis);
		};
	});
</script>

<div class="mock" bind:this={root}>
	<div
		class="frame focus"
		role="img"
		aria-label="A focused workspace pane running claude. The Name pane dialog names it reviewer; the name appears in the pane's border, and a plain-English instruction is typed at the agent's prompt."
	>
		{#if named}
			<span class="frame-title">
				<span class="name">{NAME}</span><span class="sep">&nbsp;·&nbsp;</span><span class="idle"
					>○<span class="word">&nbsp;idle</span></span
				>
			</span>
		{/if}
		<span class="frame-ctl" aria-hidden="true">[⠿][|][-]</span>
		<div class="frame-body">
			<div class="dim">$ claude</div>
			<div class="line">
				<span class="prompt">&gt;</span>
				{ask}{#if beat === 'ask'}<span class="cursor">▏</span>{/if}
			</div>
		</div>

		{#if beat === 'dialog'}
			<div class="dialog" aria-hidden="true">
				<span class="grip">[⠿]</span>
				<div class="dtitle">Name pane</div>
				<div class="hint">Used to identify and message this agent, e.g. reviewer.</div>
				<div class="input">{typed}<span class="cursor">▏</span></div>
				<div class="buttons">
					<span class="primary" class:pressed={saving}>↵ Save</span>
					<span class="secondary">Esc Cancel</span>
				</div>
			</div>
		{/if}
	</div>
	<div class="cap">
		<kbd>Ctrl+B</kbd> <kbd>m</kbd> · Name pane
	</div>
</div>

<style>
	.mock {
		display: flex;
		flex-direction: column;
	}

	.frame-body {
		min-height: 168px;
	}

	.frame-title .name {
		color: var(--sb-role-4);
	}

	.frame-title .idle {
		color: var(--term-dim);
	}

	.line {
		/* The ask wraps like real prompt input; the shared rule ellipsizes. */
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		text-overflow: clip;
		overflow: visible;
	}

	.prompt {
		color: var(--sb-accent);
	}

	/* The workspace's plain dialog: a rounded box in the focus color on the
	   raised chrome, its drag grip in the top rule, the field one level
	   down on the panel, and two keycap buttons on the last row. */
	.dialog {
		position: absolute;
		left: 12px;
		right: 12px;
		top: 50%;
		transform: translateY(-50%);
		z-index: 3;
		border: 1px solid var(--sb-accent);
		border-radius: 6px;
		background: var(--sb-raised);
		color: var(--term-text);
		padding: 8px 10px 10px;
		font-size: 11.5px;
		line-height: 1.8;
	}

	.grip {
		position: absolute;
		top: -10px;
		right: 8px;
		padding: 0 2px;
		line-height: 18px;
		font-size: 11px;
		font-weight: 700;
		color: var(--sb-accent);
		background: var(--sb-raised);
	}

	.hint {
		color: var(--term-dim);
	}

	.input {
		margin-top: 2px;
		padding: 0 5px;
		background: var(--sb-panel);
		white-space: pre;
	}

	.buttons {
		margin-top: 20px;
		display: flex;
		gap: 12px;
		font-weight: 700;
	}

	.primary {
		padding: 0 6px;
		background: var(--sb-accent);
		color: var(--accent-ink);
		transition: filter 120ms;
	}

	.primary.pressed {
		filter: brightness(1.18);
	}

	.secondary {
		padding: 0 6px;
		background: var(--sb-panel);
	}

	.cap {
		margin-top: 14px;
		font-family: var(--font-mono);
		font-size: 11px;
		letter-spacing: 0.4px;
		color: var(--faint);
	}

	kbd {
		font-family: var(--font-mono);
		font-size: 10.5px;
		padding: 0 5px;
		border: 1px solid var(--hairline);
		border-bottom-width: 2px;
		border-radius: 3px;
		color: var(--muted);
	}

	@media (max-width: 400px) {
		.frame-title .word {
			display: none;
		}
	}
</style>
