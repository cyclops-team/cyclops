// The signal field behind the hero: a drifting constellation of nodes, the
// short wires between the ones that are near each other, and the pulses
// that run along those wires. Three nodes stand for the three agents in
// the workspace mock beside it, and when the mock delivers a message the
// field runs the same pulse between the same two nodes.
//
// Vanilla three.js, loaded on demand from the component so the page paints
// before the chunk arrives and the server never sees it. Every color comes
// from the page's CSS tokens, so the field follows the theme toggle
// between sorbet and forest the way the rest of the page does.

import * as THREE from 'three';

export interface FieldTheme {
	/** The page ground; wires fade into it rather than to transparent. */
	bg: string;
	line: string;
	node: string;
	/** One color per agent node, in agent order. */
	agents: string[];
	pulse: string;
}

export interface FieldOptions {
	reducedMotion: boolean;
}

export interface Field {
	setTheme(theme: FieldTheme): void;
	/** Run a pulse from one agent node to another (indices into `agents`). */
	pulse(from: number, to: number): void;
	/** Pause or resume the frame loop, e.g. when the hero scrolls away. */
	setActive(active: boolean): void;
	dispose(): void;
}

// Colors are passed straight through: CSS hex in, the same hex on screen.
THREE.ColorManagement.enabled = false;

const POINT_VERT = /* glsl */ `
	attribute float aSize;
	attribute float aAlpha;
	attribute vec3 aColor;
	uniform float uPixelRatio;
	varying float vAlpha;
	varying vec3 vColor;
	void main() {
		vec4 mv = modelViewMatrix * vec4(position, 1.0);
		gl_Position = projectionMatrix * mv;
		gl_PointSize = aSize * uPixelRatio * (28.0 / -mv.z);
		vAlpha = aAlpha;
		vColor = aColor;
	}
`;

const POINT_FRAG = /* glsl */ `
	varying float vAlpha;
	varying vec3 vColor;
	void main() {
		float d = length(gl_PointCoord - 0.5);
		float a = smoothstep(0.5, 0.32, d) * vAlpha;
		if (a < 0.01) discard;
		gl_FragColor = vec4(vColor, a);
	}
`;

const PULSE_POOL = 12;
const PULSE_MS = 1100;
const AGENTS = 3;

function easeInOut(t: number): number {
	return t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
}

interface Pulse {
	active: boolean;
	from: number;
	to: number;
	start: number;
	a: THREE.Vector3;
	b: THREE.Vector3;
}

export function createField(
	canvas: HTMLCanvasElement,
	initialTheme: FieldTheme,
	options: FieldOptions
): Field | null {
	let renderer: THREE.WebGLRenderer;
	try {
		renderer = new THREE.WebGLRenderer({
			canvas,
			alpha: true,
			antialias: true,
			powerPreference: 'low-power'
		});
	} catch {
		// No WebGL: the hero keeps its paper ground and nothing else changes.
		return null;
	}
	renderer.outputColorSpace = THREE.LinearSRGBColorSpace;
	renderer.setClearColor(0x000000, 0);

	const narrow = window.innerWidth < 720;
	const COUNT = narrow ? 48 : 120;
	const MAX_EDGES = COUNT * 6;
	const pixelRatio = Math.min(window.devicePixelRatio || 1, narrow ? 1.5 : 2);
	renderer.setPixelRatio(pixelRatio);

	const scene = new THREE.Scene();
	const camera = new THREE.PerspectiveCamera(38, 1, 1, 100);
	camera.position.set(0, 0, 30);
	const group = new THREE.Group();
	scene.add(group);

	// ---- theme ----
	const theme = {
		bg: new THREE.Color(),
		line: new THREE.Color(),
		node: new THREE.Color(),
		agents: [] as THREE.Color[],
		pulse: new THREE.Color()
	};
	function baseColorOf(i: number): THREE.Color {
		return i < AGENTS ? (theme.agents[i] ?? theme.node) : theme.node;
	}

	// ---- nodes ----
	// Positions are kept normalised to [-1, 1] and scaled into the camera's
	// view on every frame, so a resize reshapes the field instead of
	// leaving it cropped or floating in a corner.
	const norm = new Float32Array(COUNT * 3);
	const vel = new Float32Array(COUNT * 3);
	const flash = new Float32Array(COUNT);
	for (let i = 0; i < COUNT; i++) {
		norm[i * 3] = Math.random() * 2 - 1;
		norm[i * 3 + 1] = Math.random() * 2 - 1;
		norm[i * 3 + 2] = Math.random() * 2 - 1;
		vel[i * 3] = (Math.random() - 0.5) * 0.05;
		vel[i * 3 + 1] = (Math.random() - 0.5) * 0.05;
		vel[i * 3 + 2] = (Math.random() - 0.5) * 0.03;
	}
	// The agent nodes start near the middle, where the eye already is.
	for (let i = 0; i < AGENTS; i++) {
		norm[i * 3] = (i - 1) * 0.35 + (Math.random() - 0.5) * 0.1;
		norm[i * 3 + 1] = (Math.random() - 0.5) * 0.4;
		norm[i * 3 + 2] = (Math.random() - 0.5) * 0.3;
	}

	const world = new Float32Array(COUNT * 3);
	const nodeSizes = new Float32Array(COUNT);
	const nodeAlphas = new Float32Array(COUNT);
	const nodeColors = new Float32Array(COUNT * 3);
	for (let i = 0; i < COUNT; i++) {
		nodeSizes[i] = i < AGENTS ? 10 : 4 + Math.random() * 2.6;
		nodeAlphas[i] = i < AGENTS ? 0.95 : 0.55 + Math.random() * 0.3;
	}
	const nodeGeo = new THREE.BufferGeometry();
	nodeGeo.setAttribute('position', new THREE.BufferAttribute(world, 3));
	nodeGeo.setAttribute('aSize', new THREE.BufferAttribute(nodeSizes, 1));
	nodeGeo.setAttribute('aAlpha', new THREE.BufferAttribute(nodeAlphas, 1));
	nodeGeo.setAttribute('aColor', new THREE.BufferAttribute(nodeColors, 3));
	const pointMat = new THREE.ShaderMaterial({
		vertexShader: POINT_VERT,
		fragmentShader: POINT_FRAG,
		uniforms: { uPixelRatio: { value: pixelRatio } },
		transparent: true,
		depthWrite: false
	});
	const nodes = new THREE.Points(nodeGeo, pointMat);
	group.add(nodes);

	// ---- wires ----
	const edgePos = new Float32Array(MAX_EDGES * 6);
	const edgeCol = new Float32Array(MAX_EDGES * 6);
	const edgeGeo = new THREE.BufferGeometry();
	edgeGeo.setAttribute('position', new THREE.BufferAttribute(edgePos, 3));
	edgeGeo.setAttribute('color', new THREE.BufferAttribute(edgeCol, 3));
	const edgeMat = new THREE.LineBasicMaterial({ vertexColors: true });
	const edges = new THREE.LineSegments(edgeGeo, edgeMat);
	group.add(edges);
	// The pairs currently wired, refreshed each frame, so an ambient pulse
	// always runs along a wire that is really there.
	const edgeIndex = new Int32Array(MAX_EDGES * 2);
	let edgeCount = 0;

	// ---- pulses ----
	const pulses: Pulse[] = Array.from({ length: PULSE_POOL }, () => ({
		active: false,
		from: 0,
		to: 0,
		start: 0,
		a: new THREE.Vector3(),
		b: new THREE.Vector3()
	}));
	const pulsePos = new Float32Array(PULSE_POOL * 3);
	const pulseSizes = new Float32Array(PULSE_POOL).fill(7);
	const pulseAlphas = new Float32Array(PULSE_POOL);
	const pulseColors = new Float32Array(PULSE_POOL * 3);
	const pulseGeo = new THREE.BufferGeometry();
	pulseGeo.setAttribute('position', new THREE.BufferAttribute(pulsePos, 3));
	pulseGeo.setAttribute('aSize', new THREE.BufferAttribute(pulseSizes, 1));
	pulseGeo.setAttribute('aAlpha', new THREE.BufferAttribute(pulseAlphas, 1));
	pulseGeo.setAttribute('aColor', new THREE.BufferAttribute(pulseColors, 3));
	const pulsePoints = new THREE.Points(pulseGeo, pointMat);
	group.add(pulsePoints);
	// A pulse between two nodes that are not wired brings its own wire for
	// as long as it is in flight.
	const wirePos = new Float32Array(PULSE_POOL * 6);
	const wireCol = new Float32Array(PULSE_POOL * 6);
	const wireGeo = new THREE.BufferGeometry();
	wireGeo.setAttribute('position', new THREE.BufferAttribute(wirePos, 3));
	wireGeo.setAttribute('color', new THREE.BufferAttribute(wireCol, 3));
	const wires = new THREE.LineSegments(wireGeo, edgeMat);
	group.add(wires);

	function fire(from: number, to: number, now: number) {
		const p = pulses.find((p) => !p.active) ?? pulses[0];
		p.active = true;
		p.from = from;
		p.to = to;
		p.start = now;
	}

	// ---- geometry of the view ----
	let spreadX = 1;
	let spreadY = 1;
	const spreadZ = 5;
	let link = 4;
	function resize() {
		const w = canvas.clientWidth || 1;
		const h = canvas.clientHeight || 1;
		renderer.setSize(w, h, false);
		camera.aspect = w / h;
		camera.updateProjectionMatrix();
		const halfH = camera.position.z * Math.tan((camera.fov * Math.PI) / 360);
		spreadY = halfH * 1.15;
		spreadX = halfH * camera.aspect * 1.15;
		// Wire reach scales with the mean spacing so density reads the
		// same on a phone and on a wide desktop.
		const spacing = Math.sqrt((4 * spreadX * spreadY) / COUNT);
		link = spacing * 1.25;
		if (!running) stillFrame();
	}

	// ---- pointer ----
	let targetRX = 0;
	let targetRY = 0;
	function onPointer(event: PointerEvent) {
		const rect = canvas.getBoundingClientRect();
		if (rect.height === 0) return;
		const x = (event.clientX - rect.left) / rect.width - 0.5;
		const y = (event.clientY - rect.top) / rect.height - 0.5;
		targetRY = x * 0.22;
		targetRX = y * 0.12;
	}

	// ---- frame ----
	const tmp = new THREE.Color();
	let last = 0;
	let nextAmbient = 0;
	let clock = 0;
	function step(now: number) {
		const dt = Math.min((now - (last || now)) / 1000, 0.05);
		last = now;
		clock += dt;

		for (let i = 0; i < COUNT; i++) {
			for (let k = 0; k < 3; k++) {
				const idx = i * 3 + k;
				norm[idx] += vel[idx] * dt;
				if (norm[idx] > 1) {
					norm[idx] = 1;
					vel[idx] = -Math.abs(vel[idx]);
				} else if (norm[idx] < -1) {
					norm[idx] = -1;
					vel[idx] = Math.abs(vel[idx]);
				}
			}
			flash[i] = Math.max(0, flash[i] - dt * 1.6);
		}
		project();

		// Wires between near neighbours, coloured toward the ground as
		// they stretch so they fade in and out instead of popping.
		edgeCount = 0;
		for (let i = 0; i < COUNT && edgeCount < MAX_EDGES; i++) {
			const ax = world[i * 3];
			const ay = world[i * 3 + 1];
			const az = world[i * 3 + 2];
			for (let j = i + 1; j < COUNT && edgeCount < MAX_EDGES; j++) {
				const dx = ax - world[j * 3];
				const dy = ay - world[j * 3 + 1];
				const dz = az - world[j * 3 + 2];
				const d = Math.sqrt(dx * dx + dy * dy + dz * dz);
				if (d > link) continue;
				const s = Math.pow(1 - d / link, 1.6);
				tmp.copy(theme.bg).lerp(theme.line, s);
				const o = edgeCount * 6;
				edgePos[o] = ax;
				edgePos[o + 1] = ay;
				edgePos[o + 2] = az;
				edgePos[o + 3] = world[j * 3];
				edgePos[o + 4] = world[j * 3 + 1];
				edgePos[o + 5] = world[j * 3 + 2];
				tmp.toArray(edgeCol, o);
				tmp.toArray(edgeCol, o + 3);
				edgeIndex[edgeCount * 2] = i;
				edgeIndex[edgeCount * 2 + 1] = j;
				edgeCount++;
			}
		}
		edgeGeo.setDrawRange(0, edgeCount * 2);
		edgeGeo.getAttribute('position').needsUpdate = true;
		edgeGeo.getAttribute('color').needsUpdate = true;

		if (!options.reducedMotion && now >= nextAmbient && edgeCount > 0) {
			const e = Math.floor(Math.random() * edgeCount);
			fire(edgeIndex[e * 2], edgeIndex[e * 2 + 1], now);
			nextAmbient = now + 1400 + Math.random() * 1800;
		}

		for (let i = 0; i < PULSE_POOL; i++) {
			const p = pulses[i];
			let alpha = 0;
			if (p.active) {
				const t = (now - p.start) / PULSE_MS;
				if (t >= 1) {
					p.active = false;
					flash[p.to] = 1;
				} else {
					p.a.fromArray(world, p.from * 3);
					p.b.fromArray(world, p.to * 3);
					const e = easeInOut(t);
					pulsePos[i * 3] = p.a.x + (p.b.x - p.a.x) * e;
					pulsePos[i * 3 + 1] = p.a.y + (p.b.y - p.a.y) * e;
					pulsePos[i * 3 + 2] = p.a.z + (p.b.z - p.a.z) * e;
					alpha = Math.sin(t * Math.PI);
					p.a.toArray(wirePos, i * 6);
					p.b.toArray(wirePos, i * 6 + 3);
					tmp.copy(theme.bg).lerp(theme.pulse, alpha * 0.55);
					tmp.toArray(wireCol, i * 6);
					tmp.toArray(wireCol, i * 6 + 3);
				}
			}
			pulseAlphas[i] = alpha;
			if (alpha === 0) {
				// Parked behind the camera; a zero-alpha point still costs a fragment.
				pulsePos[i * 3 + 2] = 1000;
				wirePos[i * 6 + 2] = 1000;
				wirePos[i * 6 + 5] = 1000;
			}
			theme.pulse.toArray(pulseColors, i * 3);
		}
		pulseGeo.getAttribute('position').needsUpdate = true;
		pulseGeo.getAttribute('aAlpha').needsUpdate = true;
		pulseGeo.getAttribute('aColor').needsUpdate = true;
		wireGeo.getAttribute('position').needsUpdate = true;
		wireGeo.getAttribute('color').needsUpdate = true;

		for (let i = 0; i < COUNT; i++) {
			const f = flash[i];
			tmp.copy(baseColorOf(i)).lerp(theme.pulse, f);
			tmp.toArray(nodeColors, i * 3);
			nodeAlphas[i] = Math.min(1, (i < AGENTS ? 0.95 : 0.55 + (i % 7) * 0.05) + f * 0.5);
		}
		nodeGeo.getAttribute('aColor').needsUpdate = true;
		nodeGeo.getAttribute('aAlpha').needsUpdate = true;

		group.rotation.y += (Math.sin(clock * 0.07) * 0.08 + targetRY - group.rotation.y) * 0.04;
		group.rotation.x += (targetRX - group.rotation.x) * 0.04;
		renderer.render(scene, camera);
	}

	function project() {
		for (let i = 0; i < COUNT; i++) {
			world[i * 3] = norm[i * 3] * spreadX;
			world[i * 3 + 1] = norm[i * 3 + 1] * spreadY;
			world[i * 3 + 2] = norm[i * 3 + 2] * spreadZ;
		}
		nodeGeo.getAttribute('position').needsUpdate = true;
	}

	// One frame with nothing in flight: what a resize, a theme change or
	// reduced motion paints while the loop is not running.
	function stillFrame() {
		last = performance.now();
		step(last);
	}

	function setTheme(next: FieldTheme) {
		theme.bg.set(next.bg);
		theme.line.set(next.line);
		theme.node.set(next.node);
		theme.pulse.set(next.pulse);
		theme.agents = next.agents.map((c) => new THREE.Color(c));
		if (!running) stillFrame();
	}

	// ---- loop ----
	let running = false;
	let active = false;
	let raf = 0;
	function loop(now: number) {
		if (!running) return;
		step(now);
		raf = requestAnimationFrame(loop);
	}
	function setActive(next: boolean) {
		active = next;
		if (options.reducedMotion) return;
		if (active && !running) {
			running = true;
			last = 0;
			raf = requestAnimationFrame(loop);
		} else if (!active && running) {
			running = false;
			cancelAnimationFrame(raf);
		}
	}

	const ro = new ResizeObserver(resize);
	ro.observe(canvas);
	window.addEventListener('pointermove', onPointer, { passive: true });

	setTheme(initialTheme);
	resize();

	return {
		setTheme,
		pulse(from, to) {
			if (options.reducedMotion || !active) return;
			fire(
				Math.max(0, Math.min(AGENTS - 1, from)),
				Math.max(0, Math.min(AGENTS - 1, to)),
				performance.now()
			);
		},
		setActive,
		dispose() {
			running = false;
			cancelAnimationFrame(raf);
			ro.disconnect();
			window.removeEventListener('pointermove', onPointer);
			nodeGeo.dispose();
			edgeGeo.dispose();
			pulseGeo.dispose();
			wireGeo.dispose();
			pointMat.dispose();
			edgeMat.dispose();
			renderer.dispose();
		}
	};
}
