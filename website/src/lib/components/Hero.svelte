<script lang="ts">
	import InstallCommand from './InstallCommand.svelte';
	import HeroVisual from './HeroVisual.svelte';
	import GithubMark from './GithubMark.svelte';
	import { REPO_URL } from '$lib/config';
</script>

<section class="section hero">
		<div class="copy">
			<div class="eyebrow">// Open source coordination for coding agents</div>
			<h1 class="pixel">CYCLOPS</h1>
			<p class="tagline">One eye. Multiple agents. A single coordinated team.</p>
			<p>Coordinate and track coding agents across providers in one terminal workspace.</p>
			<div class="actions">
				<a class="btn btn-primary" href="#install">Install Cyclops</a>
				<a class="btn btn-secondary" href={REPO_URL} target="_blank" rel="noopener noreferrer">
					<GithubMark size={14} />
					View on GitHub
				</a>
			</div>
			<div class="install" id="install" tabindex="-1">
				<InstallCommand />
			</div>
		</div>
		<div class="visual">
			<HeroVisual />
			<p class="hint">
				<span class="live" aria-hidden="true"></span>
				Live mock · click an agent, a pane, or a tab
			</p>
		</div>
</section>

<style>
	.hero {
		display: grid;
		grid-template-columns: minmax(0, 0.82fr) minmax(0, 1.18fr);
		gap: 48px;
		align-items: center;
		padding-top: 56px;
		padding-bottom: 40px;
	}

	.copy,
	.visual {
		min-width: 0;
	}

	.eyebrow {
		margin-bottom: 18px;
	}

	h1 {
		font-weight: 700;
		font-size: clamp(34px, 5.2vw, 62px);
		line-height: 0.98;
		color: var(--ink);
		margin: 0;
		text-shadow: 5px 5px 0 var(--accent-soft);
	}

	p {
		max-width: 460px;
		margin: 18px 0 0;
		font-size: 15.5px;
		line-height: 1.6;
		color: var(--muted);
	}

	.tagline {
		margin: 12px 0 0;
		font-size: 18px;
		font-weight: 600;
		color: var(--ink);
	}

	.tagline + p {
		margin-top: 10px;
	}

	.actions {
		display: flex;
		gap: 14px;
		flex-wrap: wrap;
		margin: 24px 0 22px;
	}

	.install {
		width: 100%;
		min-width: 0;
		max-width: 560px;
	}

	.install:focus {
		outline: 2px solid var(--accent);
		outline-offset: 6px;
	}

	.hint {
		display: flex;
		align-items: center;
		gap: 8px;
		max-width: none;
		margin: 14px 0 0 2px;
		font-size: 11.5px;
		letter-spacing: 0.2px;
		color: var(--faint);
	}

	/* The live dot is state.healthy, the same green an active agent shows. */
	.live {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--sb-healthy);
		flex-shrink: 0;
	}

	@media (max-width: 960px) {
		.hero {
			grid-template-columns: minmax(0, 1fr);
			padding-top: 48px;
			gap: 40px;
		}
	}

	@media (max-width: 420px) {
		.actions {
			align-items: stretch;
			flex-direction: column;
		}
	}
</style>
