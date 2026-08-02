<script lang="ts">
	import { onMount } from 'svelte';
	import GithubMark from './GithubMark.svelte';
	import { GITHUB_API_URL, GITHUB_STAR_THRESHOLD, REPO_URL } from '$lib/config';

	function formatStars(n: number): string {
		if (n < 1000) return String(n);
		const units: [number, string][] = [
			[1_000_000_000, 'b'],
			[1_000_000, 'm'],
			[1_000, 'k']
		];
		for (const [value, suffix] of units) {
			if (n >= value) {
				const scaled = n / value;
				const rounded = scaled >= 100 ? Math.round(scaled) : Math.round(scaled * 10) / 10;
				return `${rounded}${suffix}`;
			}
		}
		return String(n);
	}

	let starText = $state<string | null>(null);

	onMount(() => {
		let cancelled = false;
		fetch(GITHUB_API_URL, { signal: AbortSignal.timeout(4000) })
			.then((res) => (res.ok ? res.json() : null))
			.then((data) => {
				if (cancelled || !data || typeof data.stargazers_count !== 'number') return;
				if (data.stargazers_count >= GITHUB_STAR_THRESHOLD) {
					starText = formatStars(data.stargazers_count);
				}
			})
			.catch(() => {
				// Network failure, rate limit, or timeout — keep the "Star" fallback.
			});
		return () => {
			cancelled = true;
		};
	});
</script>

<a
	class="badge"
	href={REPO_URL}
	target="_blank"
	rel="noopener noreferrer"
	aria-label={starText ? `View Cyclops on GitHub — ${starText} stars` : 'View Cyclops on GitHub'}
>
	<GithubMark size={15} />
	<span class="count">{starText ?? 'Star'}</span>
</a>

<style>
	.badge {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		min-width: 52px;
		justify-content: center;
		border: 1px solid var(--hairline-btn);
		color: var(--text);
		padding: 8px 12px;
		font-size: 12px;
		font-family: var(--font-mono);
	}

	.badge:hover {
		border-color: var(--sage);
		color: var(--sage-ink);
	}

	.count {
		letter-spacing: 0.3px;
	}
</style>
