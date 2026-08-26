<script lang="ts">
	import type { Snippet } from 'svelte';

	// A pane frame at rest, the way the workspace draws one that is not
	// focused: a rounded rule with the pane's name in the top border and
	// the split controls at its right end.
	let { title, children }: { title?: string; children: Snippet } = $props();
</script>

<div class="frame">
	{#if title}
		<span class="frame-title">{title}</span>
	{/if}
	<span class="frame-ctl" aria-hidden="true">[⠿][|][-]</span>
	<div class="frame-body">
		{@render children()}
	</div>
</div>
