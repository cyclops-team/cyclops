<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import GithubMark from './GithubMark.svelte';
	import { REPO_URL, DOCS_URL, LICENSE_URL } from '$lib/config';
</script>

<section class="section">
	<SectionHead title="OPEN SOURCE" index="05 / Repository" />
	<div class="panel">
		<div class="copy">
			<h3 class="pixel heading">Built in the open.<br />Runs on your machine.</h3>
			<p>
				Inspect it, extend it, and help shape how coding-agent teams should work. Cyclops keeps
				coordination close to your code and terminal.
			</p>
			<div class="actions">
				<a class="btn btn-secondary with-icon" href={REPO_URL} target="_blank" rel="noopener noreferrer">
					<GithubMark size={14} />
					Star on GitHub
				</a>
				<a class="btn btn-ghost" href={DOCS_URL} target="_blank" rel="noopener noreferrer"
					>Read the docs →</a
				>
			</div>
		</div>
		<div class="meta">
			<div class="row">
				<span class="label">Repository</span>
				<a href={REPO_URL} target="_blank" rel="noopener noreferrer">github.com/cyclops-team/cyclops</a>
			</div>
			<div class="row">
				<span class="label">License</span>
				<a href={LICENSE_URL} target="_blank" rel="noopener noreferrer">View LICENSE →</a>
			</div>
		</div>
	</div>
</section>

<style>
	.panel {
		border: 1px solid var(--line);
		background: var(--surface);
		padding: 44px 40px;
		display: grid;
		grid-template-columns: 1.3fr 1fr;
		gap: 40px;
	}

	.heading {
		font-weight: 700;
		font-size: clamp(22px, 3vw, 30px);
		color: var(--ink);
		line-height: 1.2;
		margin: 0 0 18px;
	}

	.copy p {
		font-size: 14.5px;
		line-height: 1.65;
		color: var(--muted);
		margin: 0 0 26px;
		max-width: 440px;
	}

	.copy,
	.meta,
	.row {
		min-width: 0;
	}

	.actions {
		display: flex;
		gap: 14px;
		flex-wrap: wrap;
	}

	.actions a {
		display: inline-flex;
		align-items: center;
		gap: 8px;
	}

	.meta {
		border-left: 1px solid var(--line);
		padding-left: 40px;
		display: flex;
		flex-direction: column;
		gap: 20px;
		justify-content: center;
	}

	.row {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}

	.label {
		font-size: 11px;
		letter-spacing: 1.5px;
		text-transform: uppercase;
		color: var(--faint);
	}

	.row a {
		font-size: 13.5px;
		color: var(--text);
		overflow-wrap: anywhere;
	}

	.row a:hover {
		color: var(--sage-ink);
	}

	@media (max-width: 800px) {
		.panel {
			grid-template-columns: 1fr;
			padding: 32px 24px;
		}

		.meta {
			border-left: none;
			border-top: 1px solid var(--line);
			padding-left: 0;
			padding-top: 24px;
		}
	}
</style>
