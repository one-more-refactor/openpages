<script>
  import { api, ago } from '../lib/api.js'

  let sys = $state(null)
  let heals = $state([])
  let error = $state('')
  let note = $state('')
  let busy = $state(false)

  async function load() {
    try {
      sys = await api.get('/api/system')
      heals = await api.get('/api/heals')
    } catch (e) {
      error = e.message
    }
  }

  $effect(() => {
    load()
    const t = setInterval(load, 10000)
    return () => clearInterval(t)
  })

  async function check() {
    busy = true
    try {
      const r = await api.post('/api/system/update/check')
      note = r.available ? `${r.available} is available.` : `${sys.version} is the latest.`
    } catch (e) {
      error = e.message
    } finally {
      busy = false
    }
  }

  async function apply() {
    busy = true
    try {
      await api.post('/api/system/update/apply')
      note = 'Switching. The panel will drop for a moment and come back on the new version.'
    } catch (e) {
      error = e.message
    } finally {
      busy = false
    }
  }

  const hours = (s) => `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`
</script>

<div class="section">
  <h1>The press</h1>
  {#if sys}<span class="micro">v{sys.version} · up {hours(sys.uptime_secs)}</span>{/if}
</div>

{#if error}<div class="pied"><span class="micro">Trouble</span><div class="mt-sm">{error}</div></div>{/if}

{#if sys}
  <div class="ledger">
    <div class="row" style="grid-template-columns: 240px 1fr; cursor:default">
      <span class="micro">Update channel</span>
      <span class="mono">
        {sys.channel === 'source' ? 'source — compiled here when the machine is idle' : 'release — the published binary, checksum verified'}
        {sys.auto_update ? '' : ' · automatic updates are off'}
      </span>
    </div>
    <div class="row" style="grid-template-columns: 240px 1fr; cursor:default">
      <span class="micro">Load per core</span>
      <span class="mono">{sys.load_per_core.toFixed(2)}</span>
    </div>
    <div class="row" style="grid-template-columns: 240px 1fr; cursor:default">
      <span class="micro">Runtime</span>
      <span class="mono">{sys.runtime}</span>
    </div>
    <div class="row" style="grid-template-columns: 240px 1fr; cursor:default">
      <span class="micro">Self-healing</span>
      <span class="mono">
        {sys.heal.enabled ? `${sys.heal.mode} · at most ${sys.heal.budget_per_repo_per_day} a day per repository` : 'off'}
      </span>
    </div>
    <div class="row" style="grid-template-columns: 240px 1fr; cursor:default">
      <span class="micro">Reachable at</span>
      <span class="mono">{sys.public_url ?? 'not set — GitHub is polled instead of pushing'}</span>
    </div>
  </div>

  <div class="bar mt">
    <button class="btn" onclick={check} disabled={busy}>Check for a new version</button>
    {#if sys.pending_update}
      <button class="btn btn--press" onclick={apply} disabled={busy}>Put {sys.pending_update} on press now</button>
    {/if}
    {#if note}<span class="micro" style="color:var(--proof)">{note}</span>{/if}
  </div>

  {#if sys.pending_update}
    <div class="notice">
      <span class="micro">Standing by</span>
      <span>
        {sys.pending_update} is compiled and has passed a full boot against a throwaway
        database. It swaps at a quiet moment; if it then fails to start twice, the
        previous binary is put back automatically.
      </span>
    </div>
  {/if}

  <div class="section"><h2>Version history</h2></div>
  <div class="ledger">
    {#each sys.updates as u (u.id)}
      <div class="row" style="grid-template-columns: 130px 120px 1fr; cursor:default"
           class:row--dead={u.state === 'failed' || u.state === 'rolled_back'}>
        <span class="micro">{u.state.replace('_', ' ')}</span>
        <span class="mono dim">{u.from_version ?? '—'} → {u.to_version ?? '—'}</span>
        <span class="dim" style="font-size:12.5px">{u.detail ?? ''} · {ago(u.created_at)}</span>
      </div>
    {:else}
      <div class="empty">No updates recorded yet.</div>
    {/each}
  </div>

  <div class="section">
    <h2>Repairs</h2>
    <span class="micro">rollback first, always</span>
  </div>

  <p class="soft" style="max-width:64ch">
    When a build breaks, openpages puts the last working impression back and then
    hands a short summary of the failure — not the log — to the Claude CLI on this
    machine. The same failure is never attempted twice.
  </p>

  <div class="ledger mt">
    {#each heals as h (h.id)}
      <div class="row" style="grid-template-columns: 110px 1fr auto; cursor:default"
           class:row--dead={h.state === 'error' || h.state === 'no_fix'}>
        <span class="micro">{h.state.replace('_', ' ')}</span>
        <span>
          <span class="mono">{h.target_repo}</span>
          <span class="row-sub">{h.summary ?? ''}{h.detail ? ` — ${h.detail}` : ''}</span>
        </span>
        {#if h.pr_url}
          <a class="btn btn--sm" href={h.pr_url} target="_blank" rel="noreferrer">Pull request</a>
        {:else}
          <span class="micro dim">{ago(h.created_at)}</span>
        {/if}
      </div>
    {:else}
      <div class="empty">
        {sys.heal.enabled ? 'Nothing has needed repairing.' : 'Self-healing is off.'}
      </div>
    {/each}
  </div>
{/if}
