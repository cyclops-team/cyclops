<script lang="ts">
	import { onMount } from 'svelte';
	import { onSignal } from '$lib/signal';
	import type { Field, FieldTheme } from '$lib/field';

	let canvas: HTMLCanvasElement;
	let ready = $state(false);

	onMount(() => {
		let field: Field | null = null;
		let cancelled = false;
		const cleanups: (() => void)[] = [];
		const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

		// The field takes its colors from the same tokens the page paints
		// with, read live so the theme toggle re-colors it in place.
		function readTheme(): FieldTheme {
			const style = getComputedStyle(document.documentElement);
			const token = (name: string) => style.getPropertyValue(name).trim();
			return {
				bg: token('--paper'),
				line: token('--field-line'),
				node: token('--field-node'),
				agents: [token('--sb-role-1'), token('--sb-role-4'), token('--sb-role-3')],
				pulse: token('--sb-accent')
			};
		}

		import('$lib/field')
			.then(({ createField }) => {
				if (cancelled) return;
				field = createField(canvas, readTheme(), { reducedMotion });
				if (!field) return;

				const themeWatch = new MutationObserver(() => field?.setTheme(readTheme()));
				themeWatch.observe(document.documentElement, {
					attributes: true,
					attributeFilter: ['data-theme']
				});
				cleanups.push(() => themeWatch.disconnect());

				let inView = false;
				const sync = () => field?.setActive(inView && !document.hidden);
				const io = new IntersectionObserver(([entry]) => {
					inView = entry.isIntersecting;
					sync();
				});
				io.observe(canvas);
				cleanups.push(() => io.disconnect());
				document.addEventListener('visibilitychange', sync);
				cleanups.push(() => document.removeEventListener('visibilitychange', sync));

				cleanups.push(onSignal((from, to) => field?.pulse(from, to)));

				const onScroll = () => field?.setScroll(window.scrollY);
				onScroll();
				window.addEventListener('scroll', onScroll, { passive: true });
				cleanups.push(() => window.removeEventListener('scroll', onScroll));
				ready = true;
			})
			.catch(() => {
				// The chunk failed to load; the hero keeps its plain ground.
			});

		return () => {
			cancelled = true;
			for (const cleanup of cleanups) cleanup();
			field?.dispose();
		};
	});
</script>

<div class="field" class:ready aria-hidden="true">
	<canvas bind:this={canvas}></canvas>
</div>

<style>
	/* The one layer under the whole page. Everything else is stacked above
	   it (see app.css), and the panels' own grounds cover it where there is
	   reading to do. */
	.field {
		position: fixed;
		inset: 0;
		z-index: 0;
		opacity: 0;
		transition: opacity 1.2s ease;
		pointer-events: none;
	}

	.field.ready {
		opacity: 1;
	}

	canvas {
		display: block;
		width: 100%;
		height: 100%;
	}
</style>
