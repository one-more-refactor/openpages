<script>
  import { api, ago } from '../lib/api.js'

  let nodes = $state([])
  let error = $state('')
  let name = $state('')
  let minted = $state(null)

  async function load() {
    try {
      nodes = await api.get('/api/nodes')
    } catch (e) {
      error = e.message
    }
  }

  $effect(() => {
    load()
    const t = setInterval(load, 8000)
    return () => clearInterval(t)
  })

  async function mint() {
    try {
      minted = await api.post('/api/nodes', { name })
      name = ''
      await load()
    } catch (e) {
      error = e.message
    }
  }

  const fresh = (seen) => seen && Date.now() - new Date(seen).getTime() < 120000
</script>

<div class="section">
  <h1>Nodes</h1>
  <span class="micro">{nodes.length} machines</span>
</div>

<p class="soft" style="max-width:62ch">
  A node builds and serves. Nodes dial the panel — they never wait for an
  incoming connection — so a machine behind NAT or on a laptop works with
  nothing forwarded. The panel reaches a node's edge directly, so give it an
  address it can use.
</p>

{#if error}<div class="pied"><span class="micro">Trouble</span><div class="mt-sm">{error}</div></div>{/if}

<div class="ledger mt">
  {#each nodes as n (n.id)}
    <div class="row" style="grid-template-columns: 1fr auto auto auto; cursor:default"
         class:row--live={n.is_local || fresh(n.last_seen)}>
      <span>
        <span class="row-title">{n.name}</span>
        <span class="row-sub">
          {n.id}{n.is_local ? ' · this machine' : ''}{n.edge_url ? ` · ${n.edge_url}` : ''}
        </span>
      </span>
      <span class="mono dim" style="font-size:11.5px">{n.runtime}</span>
      <span class="mono dim" style="font-size:11.5px">{n.version ?? '—'}</span>
      <span class="micro" style="color:{n.is_local || fresh(n.last_seen) ? 'var(--vermilion)' : 'var(--ink-faint)'}">
        {n.is_local ? 'HERE' : fresh(n.last_seen) ? 'UP' : `LAST SEEN ${ago(n.last_seen).toUpperCase()}`}
      </span>
    </div>
  {/each}
</div>

<div class="section"><h2>Enrol another machine</h2></div>

<div class="bar">
  <input bind:value={name} placeholder="builder-2" style="max-width:240px" />
  <button class="btn btn--press" onclick={mint} disabled={!name.trim()}>Mint a token</button>
</div>

{#if minted}
  <div class="notice" style="flex-direction:column;align-items:flex-start;gap:8px">
    <span class="micro">Run this on the other machine — the token is shown once</span>
    <code class="mono" style="display:block;width:100%;padding:12px;background:var(--sheet);border:1px solid var(--rule);overflow-x:auto">{minted.command}</code>
    <span class="hint">Only a hash of it is kept here. Lose it and you mint another.</span>
  </div>
{/if}
