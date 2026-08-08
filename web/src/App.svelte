<script>
  import { api, Unauthorized } from './lib/api.js'
  import { go, onNavigate } from './lib/nav.js'
  import Gate from './routes/Gate.svelte'
  import Composing from './routes/Composing.svelte'
  import SitePage from './routes/SitePage.svelte'
  import DeploymentPage from './routes/DeploymentPage.svelte'
  import Nodes from './routes/Nodes.svelte'
  import System from './routes/System.svelte'

  let path = $state(location.pathname)
  let user = $state(null)
  let checked = $state(false)
  let meta = $state({ version: '', base_domain: '', runtime: '' })

  $effect(() => onNavigate((p) => (path = p)))

  $effect(() => {
    ;(async () => {
      try {
        const [me, version] = await Promise.all([api.get('/api/auth/me'), api.get('/api/version')])
        user = me.user
        meta = version
      } catch {
        user = null
      } finally {
        checked = true
      }
    })()
  })

  function link(e) {
    const a = e.target.closest('a[data-nav]')
    if (!a) return
    e.preventDefault()
    go(a.getAttribute('href'))
  }

  async function signOut() {
    try {
      await api.post('/api/auth/logout')
    } catch (e) {
      if (!(e instanceof Unauthorized)) console.warn(e)
    }
    user = null
    go('/')
  }

  let route = $derived.by(() => {
    const m = path.match(/^\/sites\/(\d+)/)
    if (m) return { name: 'site', id: +m[1] }
    const d = path.match(/^\/deployments\/(\d+)/)
    if (d) return { name: 'deployment', id: +d[1] }
    if (path.startsWith('/nodes')) return { name: 'nodes' }
    if (path.startsWith('/system')) return { name: 'system' }
    return { name: 'composing' }
  })
</script>

<span class="reg reg--tl"></span>
<span class="reg reg--tr"></span>
<span class="reg reg--bl"></span>
<span class="reg reg--br"></span>

{#if !checked}
  <div class="sheet"></div>
{:else if !user}
  <Gate onIn={(u) => (user = u)} />
{:else}
  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
  <div class="sheet" onclick={link}>
    <header class="masthead">
      <span class="wordmark">open<em>pages</em></span>
      <nav>
        <a href="/" data-nav aria-current={route.name === 'composing' ? 'page' : undefined}>Composing</a>
        <a href="/nodes" data-nav aria-current={route.name === 'nodes' ? 'page' : undefined}>Nodes</a>
        <a href="/system" data-nav aria-current={route.name === 'system' ? 'page' : undefined}>Press</a>
      </nav>
    </header>

    <div class="colophon">
      <span>v{meta.version}</span>
      <span>·</span>
      <span>*.{meta.base_domain}</span>
      <span>·</span>
      <span>{meta.runtime}</span>
      <span>·</span>
      <span>{user.gh_login}</span>
      <button class="btn btn--sm btn--quiet" style="margin-left:auto" onclick={signOut}>Sign out</button>
    </div>

    {#if route.name === 'composing'}
      <Composing />
    {:else if route.name === 'site'}
      <SitePage id={route.id} />
    {:else if route.name === 'deployment'}
      <DeploymentPage id={route.id} />
    {:else if route.name === 'nodes'}
      <Nodes />
    {:else}
      <System />
    {/if}
  </div>
{/if}
