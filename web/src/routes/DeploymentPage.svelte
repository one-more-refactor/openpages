<script>
  import { api, ago, short } from '../lib/api.js'
  import { go } from '../lib/nav.js'
  import ColorBar from '../lib/ColorBar.svelte'
  import Stamp from '../lib/Stamp.svelte'

  let { id } = $props()

  let info = $state(null)
  let lines = $state([])
  let error = $state('')
  let follow = $state(true)
  let box = $state(null)

  async function refresh() {
    try {
      info = await api.get(`/api/deployments/${id}`)
    } catch (e) {
      error = e.message
    }
  }

  $effect(() => {
    refresh()
    const t = setInterval(refresh, 3000)
    return () => clearInterval(t)
  })

  /* The log arrives over server-sent events. It is a one-way stream that
     reconnects by itself; a WebSocket would be a second protocol for no gain. */
  $effect(() => {
    const es = new EventSource(`/api/deployments/${id}/stream?after=-1`)
    es.addEventListener('log', (ev) => {
      const l = JSON.parse(ev.data)
      lines = [...lines, l].slice(-4000)
      if (follow && box) queueMicrotask(() => (box.scrollTop = box.scrollHeight))
    })
    es.addEventListener('state', () => refresh())
    es.onerror = () => {}
    return () => es.close()
  })

  const promote = async () => {
    try {
      await api.post(`/api/deployments/${id}/promote`)
      await refresh()
    } catch (e) {
      error = e.message
    }
  }

  let d = $derived(info?.deployment)
</script>

{#if error}<div class="pied"><span class="micro">Trouble</span><div class="mt-sm">{error}</div></div>{/if}

{#if info}
  <div class="section">
    <h1>Impression #{d.id}</h1>
    <Stamp state={d.state} label={info.label} />
    <ColorBar state={d.state} />
    <span class="micro">
      <button class="btn btn--sm btn--quiet" onclick={() => go(`/sites/${info.site.id}`)}>{info.site.slug}</button>
    </span>
  </div>

  <div class="colophon" style="border:0">
    <span class="mono">{short(d.commit_sha)}</span><span>·</span>
    <span>{d.commit_msg ?? 'no message'}</span><span>·</span>
    <span>{d.branch}</span><span>·</span>
    <span>{d.kind === 'preview' ? 'proof' : 'production'}</span><span>·</span>
    <span>{ago(d.created_at)}</span>
  </div>

  <div class="bar">
    <a class="btn btn--sm" href={info.url} target="_blank" rel="noreferrer">Open {new URL(info.url).host}</a>
    {#if d.state === 'ready'}
      <button class="btn btn--sm btn--press" onclick={promote}>Put on press now</button>
    {/if}
    {#if d.state === 'superseded'}
      <button class="btn btn--sm" onclick={promote}>Put this one back on press</button>
    {/if}
    <a class="btn btn--sm btn--quiet" href={`/api/deployments/${id}/log.txt`} target="_blank" rel="noreferrer">Whole log</a>
    <label class="micro" style="display:flex;gap:6px;align-items:center;cursor:pointer;margin-left:auto">
      <input type="checkbox" bind:checked={follow} style="width:auto" /> Follow
    </label>
  </div>

  {#if d.state === 'ready'}
    <div class="notice">
      <span class="micro">Standing by</span>
      <span>
        This impression is built and proofed. It goes on press at the quietest minute
        this site has, or in at most {Math.round(info.site.swap_max_wait_secs / 60)} minutes,
        whichever comes first.
      </span>
    </div>
  {/if}

  {#if d.error_summary}
    <div class="pied">
      <span class="micro">Pied — nothing was published</span>
      <pre>{d.error_summary}</pre>
      <p class="hint" style="margin-bottom:0">
        The site was not touched. If self-healing is on, a pull request may already
        be open — see Press.
      </p>
    </div>
  {/if}

  <div class="section"><h2>Setting</h2><span class="micro">{lines.length} lines</span></div>

  <div class="log" bind:this={box}>
    {#each lines as l (l.seq)}
      <div class="log-line" class:log-line--err={l.stream === 'stderr'}>
        <span class="n">{l.seq + 1}</span>
        <span class="t">{l.line}</span>
      </div>
    {:else}
      <div class="log-line"><span class="n">·</span><span class="t dim">waiting for the press…</span></div>
    {/each}
  </div>
{/if}
