<script lang="ts">
	let { title }: { title?: string } = $props();
</script>

<div class="term-bar">
	<span class="term-dot mauve"></span>
	<span class="term-dot sage"></span>
	<span class="term-dot neutral"></span>
	{#if title}
		<span class="term-title">{title}</span>
	{/if}
</div>

<style>
	.mauve {
		background: var(--mauve);
	}

	.sage {
		background: var(--sage);
	}

	.neutral {
		background: var(--term-neutral);
	}
</style>
