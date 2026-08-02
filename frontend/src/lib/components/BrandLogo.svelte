<script lang="ts">
	import lightLogo from '$lib/assets/cyclops-logo-light-mode.png';
	import darkLogo from '$lib/assets/cyclops-logo-dark-mode-removebg.png';

	let { height, alt = 'Cyclops' }: { height: number; alt?: string } = $props();
</script>

<img class="light" src={lightLogo} {alt} style:height={`${height}px`} />
<img class="dark" src={darkLogo} {alt} style:height={`${height}px`} />

<style>
	img {
		width: auto;
		display: block;
	}

	.dark {
		display: none;
		/* Its artwork has more transparent padding than the light asset. */
		transform: scale(1.25);
	}

	:global(html[data-theme='dark']) .light {
		display: none;
	}

	:global(html[data-theme='dark']) .dark {
		display: block;
	}
</style>
