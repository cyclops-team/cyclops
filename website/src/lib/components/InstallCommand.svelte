<script lang="ts">
	import CopyableCommand from './CopyableCommand.svelte';
	import { INSTALL_COMMAND, INSTALL_NOTE } from '$lib/config';
</script>

<div class="frame">
	<span class="frame-title">terminal</span>
	<CopyableCommand lines={[INSTALL_COMMAND]} />
	<div class="prereq">{INSTALL_NOTE}</div>
</div>

<style>
	/* One quiet line under the command: the prerequisites a visitor needs
	   before pasting it. A footnote, not a second prompt. */
	.prereq {
		padding: 0 14px 12px;
		font-size: 11.5px;
		line-height: 1.5;
		color: var(--term-dim);
	}
</style>
