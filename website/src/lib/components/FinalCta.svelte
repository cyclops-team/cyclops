<script lang="ts">
	import InstallCommand from './InstallCommand.svelte';
	import GithubMark from './GithubMark.svelte';
	import { REPO_URL } from '$lib/config';
</script>

<section class="section">
	<div class="panel deep cta invert">
		<div class="eyebrow">// Get started</div>
		<h2 class="pixel">BUILD WITH YOUR<br />WHOLE AGENT TEAM</h2>
		<p>Install Cyclops and coordinate any agents you can run from the terminal.</p>
		<div class="install">
			<InstallCommand />
		</div>
		<a class="github-link" href={REPO_URL} target="_blank" rel="noopener noreferrer">
			<GithubMark size={14} />
			<span>github.com/cyclops-team/cyclops</span>
		</a>
	</div>
</section>

<style>
	.cta {
		padding: 72px 56px;
		text-align: center;
	}

	.eyebrow {
		margin-bottom: 24px;
	}

	h2 {
		font-weight: 700;
		font-size: clamp(28px, 4.6vw, 48px);
		letter-spacing: -0.04em;
		word-spacing: -0.2em;
		color: var(--ink);
		line-height: 1.05;
		margin: 0;
		text-shadow: 5px 5px 0 var(--counter-shadow);
	}

	p {
		max-width: 440px;
		color: var(--muted);
		font-size: 15px;
		line-height: 1.6;
		margin: 24px auto 32px;
	}

	.install {
		max-width: 560px;
		margin: 0 auto 28px;
		text-align: left;
	}

	.github-link {
		display: inline-flex;
		align-items: center;
		max-width: 100%;
		gap: 8px;
		font-size: 13px;
		color: var(--gray);
	}

	.github-link span {
		min-width: 0;
		overflow-wrap: anywhere;
	}

	.github-link:hover {
		color: var(--accent);
	}

	@media (max-width: 720px) {
		.cta {
			padding: 48px 24px;
		}
	}
</style>
