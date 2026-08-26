<script lang="ts">
	let { title, index }: { title: string; index: string } = $props();
</script>

<div class="head">
	<h2 class="pixel">{title}</h2>
	<span class="label">{index}</span>
</div>

<style>
	.head {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		border-bottom: 1px solid var(--line);
		padding-bottom: 16px;
		margin-bottom: 40px;
	}

	h2 {
		font-size: 20px;
		letter-spacing: 1px;
		color: var(--ink);
		margin: 0;
		font-weight: 700;
	}

	@media (max-width: 420px) {
		.head {
			align-items: flex-start;
			flex-direction: column;
			gap: 8px;
			margin-bottom: 32px;
		}
	}
</style>
