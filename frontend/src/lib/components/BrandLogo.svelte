<script lang="ts">
	import logo from '$lib/assets/cyclops-new-logo.png';

	let { height, alt = 'Cyclops' }: { height: number; alt?: string } = $props();
</script>

<img src={logo} {alt} style:height={`${height}px`} />

<style>
	img {
		width: auto;
		display: block;
	}

	:global(html[data-theme='dark']) img {
		/* The artwork is near-black; flatten it and invert to match --ink (#f2f1ec). */
		filter: brightness(0) invert(0.95);
	}
</style>
