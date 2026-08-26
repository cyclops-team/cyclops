<script lang="ts">
	import SectionHead from './SectionHead.svelte';
	import GithubMark from './GithubMark.svelte';
	import { REPO_URL, DOCS_URL, LICENSE_URL } from '$lib/config';
</script>

<section class="section">
	<SectionHead title="OPEN SOURCE" index="Repository" />
	<div class="panel split">
		<div class="copy">
			<h3 class="statement">Built in the open.<br />Runs on your machine.</h3>
			<p class="lede">
				Inspect it, extend it, and help shape how coding-agent teams should work. Cyclops keeps
				coordination close to your code and terminal.
			</p>
			<div class="actions">
				<a class="btn btn-secondary" href={REPO_URL} target="_blank" rel="noopener noreferrer">
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
	.split {
		display: grid;
		grid-template-columns: 1.3fr 1fr;
		gap: 40px;
		padding: 40px;
	}

	.statement {
		font-size: clamp(22px, 2.8vw, 30px);
	}

	.lede {
		font-size: 14.5px;
		max-width: 440px;
		margin-bottom: 26px;
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

	.row a {
		font-size: 13.5px;
		color: var(--text);
		overflow-wrap: anywhere;
	}

	.row a:hover {
		color: var(--accent);
	}

	@media (max-width: 800px) {
		.split {
			grid-template-columns: 1fr;
			padding: 28px 24px;
		}

		.meta {
			border-left: none;
			border-top: 1px solid var(--line);
			padding-left: 0;
			padding-top: 24px;
		}
	}
</style>
