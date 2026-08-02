<script lang="ts">
	let { title, index }: { title: string; index: string } = $props();
</script>

<div class="head">
	<h2 class="pixel">{title}</h2>
	<span>{index}</span>
</div>

<style>
	.head {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		border-bottom: 1px solid var(--line);
		padding-bottom: 18px;
		margin-bottom: 44px;
	}

	h2 {
		font-size: 22px;
		color: var(--ink);
		margin: 0;
		font-weight: 700;
	}

	span {
		font-size: 12px;
		letter-spacing: 2px;
		color: var(--faint);
		text-transform: uppercase;
	}

	@media (max-width: 420px) {
		.head {
			align-items: flex-start;
			flex-direction: column;
			gap: 10px;
			margin-bottom: 32px;
		}
	}
</style>
