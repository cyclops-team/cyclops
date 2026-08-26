<script lang="ts">
	import { onMount } from 'svelte';

	// The mark for any agent: a pane with a prompt in it, as a lattice of
	// dots on a small canvas in the page ink, the cursor blinking in the
	// accent. Procedural, in this site's own dot language.
	let { size = 64 }: { size?: number } = $props();

	// [x, y, weight, core?] in unit space, y up; weight 0..1.3.
	type Dot = [number, number, number, boolean?];
	const TAU = Math.PI * 2;

	function seg(out: Dot[], x1: number, y1: number, x2: number, y2: number, n: number, w = 0.7) {
		for (let i = 0; i < n; i++) {
			const f = n === 1 ? 0.5 : i / (n - 1);
			out.push([x1 + (x2 - x1) * f, y1 + (y2 - y1) * f, w]);
		}
	}

	// Your agent: a pane with a prompt and a cursor that blinks.
	function any(t: number): Dot[] {
		const out: Dot[] = [];
		const w = 0.82,
			h = 0.62;
		seg(out, -w, h, w, h, 9);
		seg(out, -w, -h, w, -h, 9);
		seg(out, -w, -h, -w, h, 7);
		seg(out, w, -h, w, h, 7);
		// The title rule.
		seg(out, -w, h - 0.28, w, h - 0.28, 9, 0.45);
		// The prompt chevron and the block cursor.
		seg(out, -0.5, 0.0, -0.3, -0.16, 3, 0.95);
		seg(out, -0.3, -0.16, -0.5, -0.32, 3, 0.95);
		const on = Math.sin(t * 3.2) > 0;
		out.push([-0.06, -0.16, on ? 1.3 : 0.15, true]);
		out.push([0.06, -0.16, on ? 0.9 : 0.12]);
		return out;
	}

	let canvas: HTMLCanvasElement;

	onMount(() => {
		const ctx = canvas.getContext('2d');
		if (!ctx) return;
		const style = getComputedStyle(document.documentElement);
		let ink = '';
		let accent = '';
		const readTheme = () => {
			ink = style.getPropertyValue('--ink').trim();
			accent = style.getPropertyValue('--accent').trim();
		};
		readTheme();

		const dpr = Math.min(window.devicePixelRatio || 1, 2);
		canvas.width = size * dpr;
		canvas.height = size * dpr;

		const draw = (t: number) => {
			ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
			ctx.clearRect(0, 0, size, size);
			const cx = size / 2,
				cy = size / 2,
				R = size * 0.44;
			const base = size / 60;
			for (const [x, y, w, core] of any(t)) {
				const px = cx + x * R,
					py = cy - y * R;
				const col = core ? accent : ink;
				// Halo, then the dot.
				ctx.globalAlpha = Math.min(1, w) * 0.14;
				ctx.fillStyle = col;
				ctx.beginPath();
				ctx.arc(px, py, base * (1.4 + 1.5 * w), 0, TAU);
				ctx.fill();
				ctx.globalAlpha = Math.min(1, 0.35 + 0.65 * w);
				ctx.beginPath();
				ctx.arc(px, py, base * (0.6 + 0.7 * Math.min(w, 1.3)), 0, TAU);
				ctx.fill();
			}
			ctx.globalAlpha = 1;
		};

		const still = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
		if (still) {
			draw(1.2);
		}

		let raf = 0;
		let running = false;
		const start0 = performance.now();
		const loop = () => {
			draw((performance.now() - start0) / 1000);
			raf = requestAnimationFrame(loop);
		};
		const run = () => {
			if (running || still) return;
			running = true;
			raf = requestAnimationFrame(loop);
		};
		const stop = () => {
			running = false;
			cancelAnimationFrame(raf);
		};

		const io = new IntersectionObserver(([e]) => (e.isIntersecting && !document.hidden ? run() : stop()), {
			threshold: 0.1
		});
		io.observe(canvas);
		const onVis = () => (document.hidden ? stop() : run());
		document.addEventListener('visibilitychange', onVis);
		const mo = new MutationObserver(() => {
			readTheme();
			if (still) draw(1.2);
		});
		mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

		return () => {
			stop();
			io.disconnect();
			mo.disconnect();
			document.removeEventListener('visibilitychange', onVis);
		};
	});
</script>

<canvas bind:this={canvas} style:width={`${size}px`} style:height={`${size}px`} aria-hidden="true"></canvas>

<style>
	canvas {
		display: block;
	}
</style>
