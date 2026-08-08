<script>
  let { state = 'queued', label = '' } = $props()

  const TEXT = {
    queued: 'QUEUED',
    building: 'SETTING',
    verifying: 'PROOFING',
    ready: 'STANDING BY',
    live: 'ON PRESS',
    superseded: 'STANDING',
    failed: 'PIED',
    cancelled: 'KILLED',
  }

  let kind = $derived(
    state === 'live' ? '' : state === 'failed' ? '' : state === 'verifying' || state === 'ready' ? 'stamp--proof' : 'stamp--muted',
  )
</script>

<span class="stamp {kind}">{label || TEXT[state] || state}</span>
