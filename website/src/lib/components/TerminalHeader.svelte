<script lang="ts">
	// `showTheme` appends the name of the theme the mock is actually painted
	// in. It follows the page's light/dark toggle, so the title never claims
	// a theme the pixels are not showing.
	let { title, showTheme = false }: { title?: string; showTheme?: boolean } = $props();
</script>

<div class="term-bar">
	<span class="term-dot raspberry"></span>
	<span class="term-dot coral"></span>
	<span class="term-dot mint"></span>
	{#if title}
		<span class="term-title">{title}{#if showTheme}<span class="theme-name"></span>{/if}</span>
	{/if}
</div>

<style>
	/* Three slots off sorbet's role wheel, spread around it the way the
	   theme spreads eight agents so they stay apart at a glance. */
	.raspberry {
		background: var(--sb-role-8);
	}

	.coral {
		background: var(--sb-role-1);
	}

	.mint {
		background: var(--sb-role-5);
	}

	.theme-name::after {
		content: ' · sorbet';
	}

	:global(html[data-theme='dark']) .theme-name::after {
		content: ' · ember';
	}
</style>
