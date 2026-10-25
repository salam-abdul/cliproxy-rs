<script lang="ts">
  import NativeActivity from "../NativeActivity.svelte";
  import Usage from "./Usage.svelte";
  let diagnostics = $state(false);
</script>

<div class="head"><h1>Activity</h1></div>
<NativeActivity />
<details class="section" bind:open={diagnostics}>
  <summary>Other provider usage diagnostics</summary>{#if diagnostics}<Usage
    />{/if}
</details>
