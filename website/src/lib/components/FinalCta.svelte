<script lang="ts">
	import InstallSelector from './InstallSelector.svelte';
	import GithubMark from './GithubMark.svelte';
	import { REPO_URL } from '$lib/config';
</script>

<section class="section">
	<div class="panel">
		<div class="eyebrow">// Get started</div>
		<h2 class="pixel">BUILD WITH YOUR<br />WHOLE AGENT TEAM</h2>
		<p>Install Cyclops and coordinate any agents you can run from the terminal.</p>
		<div class="install">
			<InstallSelector idPrefix="final-install" />
		</div>
		<a class="github-link" href={REPO_URL} target="_blank" rel="noopener noreferrer">
			<GithubMark size={14} />
			<span>github.com/cyclops-team/cyclops</span>
		</a>
	</div>
</section>

<style>
	.panel {
		border: 1px solid var(--line);
		background: var(--surface-deep);
		padding: 72px 56px;
		text-align: center;
	}

	.eyebrow {
		margin-bottom: 24px;
	}

	h2 {
		font-weight: 700;
		font-size: clamp(28px, 4.6vw, 48px);
		color: var(--ink);
		line-height: 1.05;
		margin: 0;
		text-shadow: 5px 5px 0 var(--mist);
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
		margin: 0 auto 24px;
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
		color: var(--sage-ink);
	}

	@media (max-width: 720px) {
		.panel {
			padding: 48px 24px;
		}
	}
</style>
