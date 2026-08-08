<script>
  import { api, ago, short } from '../lib/api.js'
  import { go } from '../lib/nav.js'
  import ColorBar from '../lib/ColorBar.svelte'
  import Stamp from '../lib/Stamp.svelte'

  let { id } = $props()

  let data = $state(null)
  let traffic = $state(null)
  let env = $state([])
  let error = $state('')
  let note = $state('')
  let tab = $state('impressions')
  let draft = $state({})
  let newEnv = $state({ key: '', value: '', secret: false })

  async function load() {
    try {
      data = await api.get(`/api/sites/${id}`)
      traffic = await api.get(`/api/sites/${id}/traffic`)
      env = await api.get(`/api/sites/${id}/env`)
      if (!Object.keys(draft).length) draft = { ...data.site }
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

  async function act(fn, ok) {
    try {
      await fn()
      note = ok
      setTimeout(() => (note = ''), 4000)
      await load()
    } catch (e) {
      error = e.message
    }
  }

  const deploy = () => act(() => api.post(`/api/sites/${id}/deploy`, {}), 'Queued.')
  const rollback = () => act(() => api.post(`/api/sites/${id}/rollback`), 'Rolled back to the last working impression.')

  const save = () =>
    act(
      () =>
        api.patch(`/api/sites/${id}`, {
          branch: draft.branch,
          root_dir: draft.root_dir,
          install_cmd: draft.install_cmd || null,
          build_cmd: draft.build_cmd || null,
          output_dir: draft.output_dir || null,
          build_image: draft.build_image || null,
          runtime: draft.runtime,
          kind: draft.kind,
          swap_policy: draft.swap_policy,
          swap_max_wait_secs: +draft.swap_max_wait_secs,
          drain_secs: +draft.drain_secs,
          heal_mode: draft.heal_mode,
          preview_enabled: !!draft.preview_enabled,
        }),
      'Saved. It applies to the next build.',
    )

  async function remove() {
    if (!confirm(`Delete ${data.site.slug}? Its deployments and files go too.`)) return
    await api.del(`/api/sites/${id}`)
    go('/')
  }

  const addEnv = () =>
    act(async () => {
      await api.put(`/api/sites/${id}/env`, newEnv)
      newEnv = { key: '', value: '', secret: false }
    }, 'Set.')

  const dropEnv = (k) => act(() => api.del(`/api/sites/${id}/env/${encodeURIComponent(k)}`), 'Removed.')

  let peak = $derived(Math.max(1, ...(traffic?.series ?? []).map((p) => p.hits)))
</script>

{#if error}
  <div class="pied"><span class="micro">Trouble</span><div class="mt-sm">{error}</div></div>
{/if}

{#if data}
  <div class="section">
    <h1>{data.site.slug}</h1>
    <Stamp state={data.site.state} label={data.site.label} />
    <span class="micro">
      <a href={data.site.url} target="_blank" rel="noreferrer" class="mono">{data.site.url}</a>
    </span>
  </div>

  <div class="colophon" style="border:0;margin-bottom:14px">
    <span>{data.site.repo}</span><span>·</span>
    <span>{data.site.branch}</span><span>·</span>
    <span>{data.site.framework ?? 'not detected yet'}</span><span>·</span>
    <span>{data.site.node_id}</span><span>·</span>
    <span>{data.site.swap_policy === 'immediate' ? 'swaps immediately' : 'swaps when quiet'}</span>
  </div>

  <div class="bar">
    <button class="btn btn--press" onclick={deploy}>Build the head of {data.site.branch}</button>
    <button class="btn" onclick={rollback}>Roll back</button>
    <button class="btn btn--quiet" onclick={remove}>Delete</button>
    {#if note}<span class="micro" style="color:var(--proof)">{note}</span>{/if}
  </div>

  {#if traffic?.series?.length}
    <div class="section"><h2>Traffic, last 24 hours</h2>
      <span class="micro">{traffic.last_minute} this minute · {traffic.inflight} in flight</span>
    </div>
    <div class="trafficrule" title="Each stroke is one minute. The swap scheduler waits for the short ones.">
      {#each traffic.series as p}
        <i class:quiet={p.hits <= peak * 0.25} style="height:{Math.max(2, (p.hits / peak) * 42)}px"></i>
      {/each}
    </div>
  {/if}

  <div class="section">
    <h2>
      <button class="btn btn--sm" class:btn--quiet={tab !== 'impressions'} onclick={() => (tab = 'impressions')}>Impressions</button>
      <button class="btn btn--sm" class:btn--quiet={tab !== 'setup'} onclick={() => (tab = 'setup')}>Make-ready</button>
      <button class="btn btn--sm" class:btn--quiet={tab !== 'env'} onclick={() => (tab = 'env')}>Environment</button>
    </h2>
  </div>

  {#if tab === 'impressions'}
    <div class="ledger">
      {#each data.deployments as d (d.id)}
        <button class="row" class:row--live={d.state === 'live'} class:row--dead={d.state === 'failed'}
                style="grid-template-columns: auto 1fr auto auto" onclick={() => go(`/deployments/${d.id}`)}>
          <span class="mono dim" style="font-size:11.5px">#{d.id}</span>
          <span>
            <span class="mono">{short(d.commit_sha)}</span>
            <span class="dim"> · {d.commit_msg ?? 'no message'}</span>
            <span class="row-sub">
              {d.kind === 'preview' ? 'proof' : 'production'} · {ago(d.created_at)}
              {#if d.error_summary} · {d.error_summary}{/if}
            </span>
          </span>
          <ColorBar state={d.state} small />
          <Stamp state={d.state} />
        </button>
      {:else}
        <div class="empty">Nothing built yet.</div>
      {/each}
    </div>
  {:else if tab === 'setup'}
    <div class="grid2">
      <label class="field"><span>Branch</span><input bind:value={draft.branch} /></label>
      <label class="field"><span>Root directory</span><input bind:value={draft.root_dir} placeholder="repository root" /></label>
      <label class="field"><span>Install command</span><input bind:value={draft.install_cmd} placeholder="detected" /></label>
      <label class="field"><span>Build command</span><input bind:value={draft.build_cmd} placeholder="detected" /></label>
      <label class="field">
        <span>Output directory</span>
        <input bind:value={draft.output_dir} placeholder="detected" />
        <div class="hint">The one holding index.html after the build. Wrong value here is the usual cause of a blank site — openpages refuses to publish a directory with no HTML in it.</div>
      </label>
      <label class="field"><span>Build image</span><input bind:value={draft.build_image} placeholder="chosen from the lockfile" /></label>
      <label class="field">
        <span>Kind</span>
        <select bind:value={draft.kind}>
          <option value="static">Static — build once, serve files</option>
          <option value="container">Container — the repo has a Dockerfile</option>
        </select>
      </label>
      <label class="field">
        <span>Runtime</span>
        <select bind:value={draft.runtime}>
          <option value="auto">Auto</option>
          <option value="podman">Podman (rootless)</option>
          <option value="docker">Docker</option>
        </select>
      </label>
      <label class="field">
        <span>When to swap</span>
        <select bind:value={draft.swap_policy}>
          <option value="quiet">At the quietest moment</option>
          <option value="immediate">As soon as it is proofed</option>
        </select>
      </label>
      <label class="field">
        <span>Longest wait for quiet (seconds)</span>
        <input type="number" bind:value={draft.swap_max_wait_secs} />
        <div class="hint">After this, it goes on press regardless. A busy site still deploys.</div>
      </label>
      <label class="field">
        <span>Drain (seconds)</span>
        <input type="number" bind:value={draft.drain_secs} />
        <div class="hint">How long the previous impression stays readable for requests already under way.</div>
      </label>
      <label class="field">
        <span>Self-healing</span>
        <select bind:value={draft.heal_mode}>
          <option value="inherit">Whatever the server is set to</option>
          <option value="off">Off for this site</option>
          <option value="pr">Open a pull request</option>
          <option value="direct">Push straight to the branch</option>
        </select>
      </label>
      <label class="field">
        <span>Proof deployments for other branches</span>
        <select bind:value={draft.preview_enabled}>
          <option value={1}>Yes — served at &lt;commit&gt;--{data.site.slug}</option>
          <option value={0}>No</option>
        </select>
      </label>
    </div>
    <div class="bar mt"><button class="btn btn--press" onclick={save}>Save</button></div>
  {:else}
    <div class="ledger">
      {#each env as e (e.key)}
        <div class="row" style="grid-template-columns: 220px 1fr auto; cursor:default">
          <span class="mono">{e.key}</span>
          <span class="mono dim">{e.value}</span>
          <button class="btn btn--sm btn--quiet" onclick={() => dropEnv(e.key)}>Remove</button>
        </div>
      {:else}
        <div class="empty">No environment variables. Build commands get a sane default set anyway.</div>
      {/each}
    </div>
    <div class="grid2 mt">
      <label class="field"><span>Name</span><input bind:value={newEnv.key} placeholder="API_URL" /></label>
      <label class="field"><span>Value</span><input bind:value={newEnv.value} /></label>
    </div>
    <div class="bar mt-sm">
      <label class="micro" style="display:flex;gap:7px;align-items:center;cursor:pointer">
        <input type="checkbox" bind:checked={newEnv.secret} style="width:auto" /> Hide the value in this panel
      </label>
      <button class="btn" onclick={addEnv} disabled={!newEnv.key}>Set</button>
    </div>
    <p class="hint">
      These are passed to the build container. They are stored in the database on this
      machine in plain text — openpages does not pretend to be a secrets manager.
    </p>
  {/if}
{/if}
