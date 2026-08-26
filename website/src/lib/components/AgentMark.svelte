<script lang="ts">
	import { onMount } from 'svelte';

	// An agent's mark as a lattice of dots on a small canvas, in the page
	// ink with one accent dot at the core, breathing gently. Each mark is
	// procedural: a shape that reads as the agent's, drawn in this site's
	// own dot language rather than the vendor's artwork.
	export type MarkKind = 'claude' | 'codex' | 'cursor' | 'agy' | 'any';

	let { kind, size = 64 }: { kind: MarkKind; size?: number } = $props();

	// [x, y, weight, core?] in unit space, y up; weight 0..1.3.
	type Dot = [number, number, number, boolean?];
	const TAU = Math.PI * 2;

	function hash(a: number, b: number) {
		const s = Math.sin(a * 127.1 + b * 311.7) * 43758.5453;
		return s - Math.floor(s);
	}

	function seg(out: Dot[], x1: number, y1: number, x2: number, y2: number, n: number, w = 0.7) {
		for (let i = 0; i < n; i++) {
			const f = n === 1 ? 0.5 : i / (n - 1);
			out.push([x1 + (x2 - x1) * f, y1 + (y2 - y1) * f, w]);
		}
	}

	function arc(out: Dot[], cx: number, cy: number, r: number, a0: number, a1: number, n: number, w = 0.7) {
		for (let i = 0; i < n; i++) {
			const a = a0 + ((a1 - a0) * i) / (n - 1);
			out.push([cx + Math.cos(a) * r, cy + Math.sin(a) * r, w]);
		}
	}

	// Claude: the spark burst, rays breathing out of one bright core.
	function claude(t: number): Dot[] {
		const out: Dot[] = [];
		const rays = 11;
		for (let k = 0; k < rays; k++) {
			const a = (k / rays) * TAU + t * 0.06 + (hash(k, 3.1) - 0.5) * 0.3;
			const pulse = 0.5 + 0.5 * Math.sin(t * 1.6 - k * 1.13);
			const len = (0.58 + 0.34 * hash(k, 7.7)) * (0.84 + 0.2 * pulse * pulse);
			const per = 5;
			for (let j = 0; j < per; j++) {
				const f = (j + 0.8) / per;
				out.push([Math.cos(a) * len * f, Math.sin(a) * len * f, 0.5 + 0.35 * f + 0.2 * pulse]);
			}
		}
		out.push([0, 0, 1.3, true]);
		return out;
	}

	// Codex: the prompt in its rounded card, the underscore blinking.
	function codex(t: number): Dot[] {
		const out: Dot[] = [];
		const h = 0.78,
			r = 0.3;
		// Rounded square: four sides and four corner arcs.
		seg(out, -h + r, h, h - r, h, 6);
		seg(out, -h + r, -h, h - r, -h, 6);
		seg(out, h, -h + r, h, h - r, 6);
		seg(out, -h, -h + r, -h, h - r, 6);
		arc(out, h - r, h - r, r, 0, Math.PI / 2, 3);
		arc(out, -h + r, h - r, r, Math.PI / 2, Math.PI, 3);
		arc(out, -h + r, -h + r, r, Math.PI, Math.PI * 1.5, 3);
		arc(out, h - r, -h + r, r, Math.PI * 1.5, TAU, 3);
		// The chevron.
		seg(out, -0.42, 0.3, -0.08, 0, 4, 0.95);
		seg(out, -0.08, 0, -0.42, -0.3, 4, 0.95);
		// The blinking underscore: on for half of each second.
		const on = Math.sin(t * 3.2) > 0 ? 1 : 0.18;
		seg(out, 0.06, -0.3, 0.42, -0.3, 4, on);
		out.push([-0.08, 0, 1.15, true]);
		return out;
	}

	// Cursor: the cube, a highlight sweeping face to face.
	function cursor(t: number): Dot[] {
		const out: Dot[] = [];
		const R = 0.8;
		const v: [number, number][] = [];
		for (let k = 0; k < 6; k++) {
			const a = -Math.PI / 2 + (k / 6) * TAU;
			v.push([Math.cos(a) * R, Math.sin(a) * R]);
		}
		// Which of the three faces is lit right now.
		const lit = Math.floor(((t * 0.35) % 3 + 3) % 3);
		const faceW = (f: number) => (f === lit ? 1 : 0.55);
		// Outer hexagon: faces 0 (top) own edges 5-0-1, 1 (right) own 1-2-3, 2 (left) own 3-4-5.
		const edgeFace = [0, 1, 1, 2, 2, 0];
		for (let k = 0; k < 6; k++) {
			const [x1, y1] = v[k];
			const [x2, y2] = v[(k + 1) % 6];
			seg(out, x1, y1, x2, y2, 5, faceW(edgeFace[k]));
		}
		// Inner spokes to the top, lower-right and lower-left corners.
		seg(out, 0, 0, v[0][0], v[0][1], 4, Math.max(faceW(0), faceW(1)) * 0.9);
		seg(out, 0, 0, v[2][0], v[2][1], 4, Math.max(faceW(1), faceW(2)) * 0.9);
		seg(out, 0, 0, v[4][0], v[4][1], 4, Math.max(faceW(2), faceW(0)) * 0.9);
		out.push([0, 0, 1.15, true]);
		return out;
	}

	// Antigravity: a ring, and one dot that will not stay down.
	function agy(t: number): Dot[] {
		const out: Dot[] = [];
		const n = 22,
			r = 0.72;
		const phase = (t * 0.45) % 1;
		// The riser climbs from below the ring, through it, and out the top.
		const ry = -1.15 + 2.3 * phase;
		for (let k = 0; k < n; k++) {
			const a = (k / n) * TAU;
			const y = Math.sin(a) * r;
			const near = Math.exp(-Math.pow((y - ry) * 3.2, 2));
			out.push([Math.cos(a) * r, y, 0.5 + 0.5 * near]);
		}
		const fade = phase < 0.1 ? phase / 0.1 : phase > 0.9 ? (1 - phase) / 0.1 : 1;
		out.push([0, ry, 1.3 * fade, true]);
		out.push([0, ry - 0.14, 0.5 * fade]);
		out.push([0, ry - 0.26, 0.28 * fade]);
		return out;
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

	const MARKS: Record<MarkKind, (t: number) => Dot[]> = { claude, codex, cursor, agy, any };

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
			for (const [x, y, w, core] of MARKS[kind](t)) {
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
