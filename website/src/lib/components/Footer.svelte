<script lang="ts">
	import BrandLogo from './BrandLogo.svelte';
	import { REPO_URL, DOCS_URL, LICENSE_URL } from '$lib/config';
</script>

<footer class="section">
	<a class="brand" href="/">
		<BrandLogo height={21} />
		<span class="pixel wordmark">CYCLOPS</span>
	</a>
	<nav aria-label="Footer">
		<a href={REPO_URL} target="_blank" rel="noopener noreferrer">GitHub</a>
		<a href={DOCS_URL} target="_blank" rel="noopener noreferrer">Docs</a>
		<a href={LICENSE_URL} target="_blank" rel="noopener noreferrer">License</a>
	</nav>
	<span class="tag">Open source · run locally · coordinate the agents you already use</span>
</footer>

<style>
	footer {
		margin-top: 40px;
		padding-top: 40px;
		padding-bottom: 48px;
		border-top: 1px solid var(--line);
		display: flex;
		justify-content: space-between;
		align-items: center;
		flex-wrap: wrap;
		gap: 20px;
	}

	.brand {
		display: flex;
		align-items: center;
		gap: 10px;
	}

	.wordmark {
		font-size: 12px;
		letter-spacing: 1px;
		color: var(--faint);
	}

	nav {
		display: flex;
		gap: 24px;
		font-size: 12px;
	}

	nav a {
		color: var(--gray);
	}

	nav a:hover {
		color: var(--accent);
	}

	.tag {
		font-size: 11.5px;
		color: var(--faint);
		letter-spacing: 0.3px;
	}

	@media (max-width: 720px) {
		.tag {
			order: 3;
			width: 100%;
		}
	}
</style>
