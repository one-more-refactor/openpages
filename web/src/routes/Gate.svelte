<script>
  import { api } from '../lib/api.js'

  let { onIn } = $props()
  let code = $state('')
  let error = $state('')
  let busy = $state(false)

  async function submit(e) {
    e.preventDefault()
    busy = true
    error = ''
    try {
      onIn(await api.post('/api/auth/claim', { code: code.trim() }))
    } catch (err) {
      error = err.message
    } finally {
      busy = false
    }
  }
</script>

<div class="gate">
  <span class="wordmark" style="font-size:36px">open<em>pages</em></span>
  <div class="rule-double"></div>

  <p class="soft" style="margin-top:0">
    There is no password here. Sign-in codes are minted on the server, so being
    able to run a command on the machine <em>is</em> the credential.
  </p>

  <form onsubmit={submit}>
    <label class="field">
      <span>Sign-in code</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input class="code-input mono" bind:value={code} placeholder="XXXXX-XXXXX" autofocus />
    </label>
    <div class="bar mt-sm">
      <button class="btn btn--press" disabled={busy || code.trim().length < 4}>
        {busy ? 'Checking' : 'Sign in'}
      </button>
    </div>
  </form>

  {#if error}
    <div class="pied">
      <span class="micro">Not accepted</span>
      <div class="mt-sm">{error}</div>
    </div>
  {/if}

  <p class="hint mt">
    On the server: <code>openpages invite</code>
    <br />First run ever? The code was printed to the log —
    <code>journalctl -u openpages -n 40</code>
  </p>
</div>
