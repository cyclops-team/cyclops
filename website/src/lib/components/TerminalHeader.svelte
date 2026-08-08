<script lang="ts">
	let { title }: { title?: string } = $props();
</script>

<div class="term-bar">
	<span class="term-dot raspberry"></span>
	<span class="term-dot coral"></span>
	<span class="term-dot mint"></span>
	{#if title}
		<span class="term-title">{title}</span>
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
</style>
