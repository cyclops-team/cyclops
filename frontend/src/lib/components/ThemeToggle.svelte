<script lang="ts">
	import { onMount } from 'svelte';

	type Theme = 'light' | 'dark';

	const STORAGE_KEY = 'cyclops-theme';
	const DARK_MODE_QUERY = '(prefers-color-scheme: dark)';

	let theme = $state<Theme>('light');
	let followsSystem = true;

	function isTheme(value: string | null | undefined): value is Theme {
		return value === 'light' || value === 'dark';
	}

	function readStoredTheme(): Theme | null {
		try {
			const stored = localStorage.getItem(STORAGE_KEY);
			return isTheme(stored) ? stored : null;
		} catch {
			return null;
		}
	}

	function applyTheme(nextTheme: Theme) {
		theme = nextTheme;
		document.documentElement.dataset.theme = nextTheme;
	}

	function persistTheme(nextTheme: Theme) {
		applyTheme(nextTheme);
		followsSystem = false;
		try {
			localStorage.setItem(STORAGE_KEY, nextTheme);
		} catch {
			// The selected theme still applies for this page when storage is unavailable.
		}
	}

	onMount(() => {
		const colorScheme = matchMedia(DARK_MODE_QUERY);
		const storedTheme = readStoredTheme();
		followsSystem = storedTheme === null;
		applyTheme(storedTheme ?? (colorScheme.matches ? 'dark' : 'light'));

		function handleSystemChange(event: MediaQueryListEvent) {
			if (followsSystem) applyTheme(event.matches ? 'dark' : 'light');
		}

		function handleStorage(event: StorageEvent) {
			if (event.key !== STORAGE_KEY) return;
			const nextTheme = isTheme(event.newValue) ? event.newValue : null;
			followsSystem = nextTheme === null;
			applyTheme(nextTheme ?? (colorScheme.matches ? 'dark' : 'light'));
		}

		colorScheme.addEventListener('change', handleSystemChange);
		window.addEventListener('storage', handleStorage);

		return () => {
			colorScheme.removeEventListener('change', handleSystemChange);
			window.removeEventListener('storage', handleStorage);
		};
	});
</script>

<button
	type="button"
	class="theme-toggle"
	onclick={() => persistTheme(theme === 'dark' ? 'light' : 'dark')}
	aria-label={`Switch to ${theme === 'dark' ? 'light' : 'dark'} mode`}
	aria-pressed={theme === 'dark'}
	title={`Switch to ${theme === 'dark' ? 'light' : 'dark'} mode`}
>
	<span class="sun" aria-hidden="true">
		<svg viewBox="0 0 24 24">
			<circle cx="12" cy="12" r="4"></circle>
			<path d="M12 2v2M12 20v2M4.93 4.93l1.42 1.42M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.42-1.42M17.66 6.34l1.41-1.41"></path>
		</svg>
	</span>
	<span class="moon" aria-hidden="true">
		<svg viewBox="0 0 24 24">
			<path d="M20.3 15.3A9 9 0 0 1 8.7 3.7a9 9 0 1 0 11.6 11.6Z"></path>
		</svg>
	</span>
</button>

<style>
	.theme-toggle {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 34px;
		height: 34px;
		padding: 0;
		border: 1px solid var(--hairline-btn);
		background: transparent;
		color: var(--gray);
		cursor: pointer;
	}

	.theme-toggle:hover {
		border-color: var(--sage);
		color: var(--sage-ink);
	}

	span,
	svg {
		display: block;
		width: 16px;
		height: 16px;
	}

	.sun {
		display: none;
	}

	:global(html[data-theme='dark']) .sun {
		display: block;
	}

	:global(html[data-theme='dark']) .moon {
		display: none;
	}

	svg {
		fill: none;
		stroke: currentColor;
		stroke-width: 1.75;
		stroke-linecap: round;
		stroke-linejoin: round;
	}
</style>
