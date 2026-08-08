<script>
  /* The press colour bar. Five patches, one per phase, read left to right.
     Filled means passed. Blue means happening now. Vermilion means on press.
     A barred patch is where it went wrong. */
  let { state = 'queued', small = false } = $props()

  const PHASES = ['queued', 'building', 'verifying', 'ready', 'live']

  const LABELS = {
    queued: 'Queued — waiting for a node to take it',
    building: 'Setting — installing and building in a container',
    verifying: 'Proofing — checking the result actually serves',
    ready: 'Standing by — proofed, waiting for a quiet moment',
    live: 'On press — this is what visitors get',
    superseded: 'Standing — kept as the rollback',
    failed: 'Pied — it broke; nothing was published',
    cancelled: 'Killed',
  }

  let patches = $derived.by(() => {
    if (state === 'failed' || state === 'cancelled') {
      // Failure is shown at the phase it reached, not at the end.
      return PHASES.map((_, i) => (i === 0 ? 'done' : i === 1 ? 'pied' : ''))
    }
    if (state === 'superseded') return PHASES.map((_, i) => (i < 4 ? 'done' : 'muted'))
    const at = PHASES.indexOf(state)
    return PHASES.map((p, i) => {
      if (i < at) return 'done'
      if (i > at) return ''
      return p === 'live' ? 'live' : 'now'
    })
  })
</script>

<span
  class="colorbar"
  class:colorbar--sm={small}
  title={LABELS[state] ?? state}
  aria-label={LABELS[state] ?? state}
>
  {#each patches as p}
    <i class="patch" class:patch--done={p === 'done'} class:patch--now={p === 'now'}
       class:patch--live={p === 'live'} class:patch--pied={p === 'pied'}></i>
  {/each}
</span>
