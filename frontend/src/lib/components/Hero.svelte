<script lang="ts">
	import InstallSelector from './InstallSelector.svelte';
	import HeroVisual from './HeroVisual.svelte';
	import GithubMark from './GithubMark.svelte';
	import { REPO_URL } from '$lib/config';
</script>

<section class="section hero">
	<div class="copy">
		<div class="eyebrow">// Open source coordination for coding agents</div>
		<h1 class="pixel">ONE TEAM.<br />ANY AGENT.</h1>
		<p>Coordinate and track coding agents across providers in one terminal workspace.</p>
		<div class="actions">
			<a class="btn btn-primary" href="#install">Install Cyclops</a>
			<a class="btn btn-secondary with-icon" href={REPO_URL} target="_blank" rel="noopener noreferrer">
				<GithubMark size={14} />
				View on GitHub
			</a>
		</div>
		<div class="install" id="install" tabindex="-1">
			<InstallSelector />
		</div>
	</div>
	<HeroVisual />
</section>

<style>
	.hero {
		display: grid;
		grid-template-columns: minmax(0, 1.05fr) minmax(0, 1fr);
		gap: 40px;
		align-items: start;
		padding-top: 40px;
		padding-bottom: 56px;
	}

	.copy {
		min-width: 0;
	}

	.eyebrow {
		margin-bottom: 16px;
	}

	h1 {
		font-weight: 700;
		font-size: clamp(34px, 5.2vw, 62px);
		line-height: 0.98;
		color: var(--ink);
		margin: 0;
		text-shadow: 5px 5px 0 var(--mist);
	}

	p {
		max-width: 460px;
		margin: 18px 0 0;
		font-size: 16px;
		line-height: 1.5;
		color: var(--muted);
	}

	.actions {
		display: flex;
		gap: 14px;
		flex-wrap: wrap;
		margin: 22px 0 20px;
	}

	.actions a {
		display: inline-flex;
		align-items: center;
	}

	.with-icon {
		gap: 8px;
	}

	.install {
		width: 100%;
		min-width: 0;
		max-width: 560px;
	}

	.install:focus {
		outline: 2px solid var(--sage-ink);
		outline-offset: 6px;
	}

	@media (max-width: 960px) {
		.hero {
			grid-template-columns: minmax(0, 1fr);
			padding-top: 48px;
		}
	}

	@media (max-width: 420px) {
		.actions {
			align-items: stretch;
			flex-direction: column;
		}

		.actions a {
			justify-content: center;
		}
	}
</style>
