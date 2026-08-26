<script lang="ts">
	import Navbar from '$lib/components/Navbar.svelte';
	import Hero from '$lib/components/Hero.svelte';
	import ProblemSection from '$lib/components/ProblemSection.svelte';
	import HowItWorks from '$lib/components/HowItWorks.svelte';
	import Compatibility from '$lib/components/Compatibility.svelte';
	import WhyCyclops from '$lib/components/WhyCyclops.svelte';
	import OpenSource from '$lib/components/OpenSource.svelte';
	import FinalCta from '$lib/components/FinalCta.svelte';
	import Footer from '$lib/components/Footer.svelte';
	import SignalField from '$lib/components/SignalField.svelte';
</script>

<svelte:head>
	<title>Cyclops · One eye. Multiple agents. A single coordinated team.</title>
	<meta
		name="description"
		content="Cyclops is an open-source coordination layer for coding agents running in the terminal. Coordinate and track coding agents across providers in one terminal workspace."
	/>
</svelte:head>

<SignalField />
<Navbar />
<main>
	<Hero />
	<ProblemSection />
	<HowItWorks />
	<Compatibility />
	<WhyCyclops />
	<OpenSource />
	<FinalCta />
</main>
<Footer />
