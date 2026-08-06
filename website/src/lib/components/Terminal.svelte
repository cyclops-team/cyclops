<script lang="ts">
	import type { Snippet } from 'svelte';
	import TerminalHeader from './TerminalHeader.svelte';

	let { title, children }: { title?: string; children: Snippet } = $props();
</script>

<div class="term">
	<TerminalHeader {title} />
	<div class="term-body">
		{@render children()}
	</div>
</div>
