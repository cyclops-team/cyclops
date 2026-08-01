<script lang="ts">
	import BrandLogo from './BrandLogo.svelte';
	import GitHubBadge from './GitHubBadge.svelte';
	import ThemeToggle from './ThemeToggle.svelte';
	import { DOCS_URL } from '$lib/config';
</script>

<header>
	<a class="brand" href="/">
		<BrandLogo height={30} />
		<span class="pixel wordmark">CYCLOPS</span>
	</a>
	<nav aria-label="Primary">
		<a href="#how-it-works">How it works</a>
		<a href={DOCS_URL} target="_blank" rel="noopener noreferrer">Docs</a>
	</nav>
	<div class="actions">
		<ThemeToggle />
		<GitHubBadge />
		<a class="btn btn-primary" href="#install">Install</a>
	</div>
</header>

<style>
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 24px;
		padding: 18px 48px;
		border-bottom: 1px solid var(--line);
		position: sticky;
		top: 0;
		background: var(--navbar-bg);
		backdrop-filter: blur(8px);
		z-index: 50;
	}

	.brand {
		display: flex;
		align-items: center;
		gap: 16px;
		flex-shrink: 0;
	}

	.wordmark {
		font-size: 15px;
		letter-spacing: 1px;
		color: var(--ink);
	}

	nav {
		display: flex;
		gap: 28px;
		font-size: 13px;
		letter-spacing: 0.3px;
	}

	nav a {
		color: var(--gray);
	}

	nav a:hover {
		color: var(--text);
	}

	.actions {
		display: flex;
		align-items: center;
		gap: 12px;
		flex-shrink: 0;
	}

	.actions .btn-primary {
		padding: 9px 20px;
	}

	@media (max-width: 860px) {
		nav {
			display: none;
		}
	}

	@media (max-width: 560px) {
		header {
			padding: 14px 20px;
		}

		.wordmark {
			display: none;
		}
	}
</style>
