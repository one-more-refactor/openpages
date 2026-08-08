<script>
  import { api, ago, short } from '../lib/api.js'
  import { go } from '../lib/nav.js'
  import ColorBar from '../lib/ColorBar.svelte'
  import Stamp from '../lib/Stamp.svelte'

  let data = $state(null)
  let error = $state('')
  let adding = $state(false)
  let repos = $state([])
  let form = $state({ repo: '', slug: '', branch: '' })
  let busy = $state(false)

  async function load() {
    try {
      data = await api.get('/api/overview')
      error = ''
    } catch (e) {
      error = e.message
    }
  }

  $effect(() => {
    load()
    const t = setInterval(load, 5000)
    return () => clearInterval(t)
  })

  async function openAdd() {
    adding = true
    try {
      repos = await api.get('/api/repos')
    } catch (e) {
      error = `${e.message} — run \`openpages login\` on the server`
    }
  }

  async function create(e) {
    e.preventDefault()
    busy = true
    try {
      const site = await api.post('/api/sites', {
        repo: form.repo,
        slug: form.slug || undefined,
        branch: form.branch || undefined,
      })
      adding = false
      form = { repo: '', slug: '', branch: '' }
      go(`/sites/${site.id}`)
    } catch (e) {
      error = e.message
    } finally {
      busy = false
    }
  }
</script>

<div class="section">
  <h1>Composing room</h1>
  <span class="micro">{data?.sites?.length ?? 0} titles</span>
</div>

{#if error}
  <div class="pied"><span class="micro">Trouble</span><div class="mt-sm">{error}</div></div>
{/if}

<div class="ledger">
  {#each data?.sites ?? [] as s (s.id)}
    <button class="row" class:row--live={s.state === 'live'} class:row--dead={s.state === 'failed'}
            style="grid-template-columns: 1fr auto auto auto" onclick={() => go(`/sites/${s.id}`)}>
      <span>
        <span class="row-title">{s.slug}</span>
        <span class="row-sub">{s.repo} · {s.branch}{s.framework ? ` · ${s.framework}` : ''}</span>
      </span>
      <span class="mono dim" style="font-size:11.5px">
        {s.hits_24h.toLocaleString()} hits / 24h
      </span>
      <ColorBar state={s.state} small />
      <Stamp state={s.state} label={s.label} />
    </button>
  {:else}
    {#if data}
      <div class="empty">
        Nothing is set. Add a repository and openpages will build it, prove it works,
        and put it on press at the quietest moment it can find.
      </div>
    {/if}
  {/each}
</div>

<div class="bar mt">
  {#if !adding}
    <button class="btn btn--press" onclick={openAdd}>Set a new title</button>
  {/if}
</div>

{#if adding}
  <form onsubmit={create} class="mt">
    <div class="grid2">
      <label class="field">
        <span>Repository</span>
        <input list="repos" bind:value={form.repo} placeholder="owner/name" required />
        <datalist id="repos">
          {#each repos as r}<option value={r.repo}>{r.branch}{r.private ? ' · private' : ''}</option>{/each}
        </datalist>
        <div class="hint">Anything the GitHub CLI on the server can read.</div>
      </label>
      <label class="field">
        <span>Name</span>
        <input bind:value={form.slug} placeholder="taken from the repository" />
        <div class="hint">Served at <span class="mono">{(form.slug || 'name')}.{data?.base_domain}</span></div>
      </label>
    </div>
    <label class="field" style="max-width:280px">
      <span>Branch</span>
      <input bind:value={form.branch} placeholder="the repository default" />
    </label>
    <div class="bar mt-sm">
      <button class="btn btn--press" disabled={busy || !form.repo}>{busy ? 'Setting' : 'Add and build'}</button>
      <button type="button" class="btn btn--quiet" onclick={() => (adding = false)}>Cancel</button>
    </div>
  </form>
{/if}

<div class="section">
  <h2>Recent impressions</h2>
</div>

<div class="ledger">
  {#each data?.recent ?? [] as d (d.id)}
    <button class="row" style="grid-template-columns: auto 1fr auto auto" onclick={() => go(`/deployments/${d.id}`)}>
      <span class="mono dim" style="font-size:11.5px">#{d.id}</span>
      <span>
        <span class="mono">{short(d.commit_sha)}</span>
        <span class="dim"> · {d.commit_msg ?? 'no message'}</span>
        <span class="row-sub">{d.branch} · {d.kind} · {ago(d.created_at)}</span>
      </span>
      <ColorBar state={d.state} small />
      <Stamp state={d.state} />
    </button>
  {:else}
    {#if data}<div class="empty">No impressions yet.</div>{/if}
  {/each}
</div>

{#if data?.pending_update}
  <div class="notice">
    <span class="micro">New version standing by</span>
    <span>{data.pending_update} is compiled and verified. It takes the press at the next quiet moment.</span>
  </div>
{/if}
