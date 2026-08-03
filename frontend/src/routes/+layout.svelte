<script lang="ts">
	import { onMount } from 'svelte';
	import '../app.css';
	import favicon from '$lib/assets/favicon.png';

	let { children } = $props();

	onMount(() => {
		// The pixel/mono display fonts load async and reflow the page, which can
		// leave an initial hash-anchor scroll (e.g. a nav link to #agents)
		// pointing at the wrong position once layout settles.
		if (!location.hash) return;
		document.fonts?.ready.then(() => {
			document.getElementById(location.hash.slice(1))?.scrollIntoView();
		});
	});
</script>

<svelte:head>
	<link rel="icon" type="image/png" href={favicon} />
</svelte:head>

{@render children()}
