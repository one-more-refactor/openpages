export class Unauthorized extends Error {}

async function request(path, method = 'GET', body) {
  const res = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (res.status === 401) throw new Unauthorized('not signed in')

  const text = await res.text()
  let data = null
  try {
    data = text ? JSON.parse(text) : null
  } catch {
    data = text
  }
  if (!res.ok) throw new Error(data?.error || `HTTP ${res.status}`)
  return data
}

export const api = {
  get: (p) => request(p),
  post: (p, b) => request(p, 'POST', b ?? {}),
  patch: (p, b) => request(p, 'PATCH', b),
  put: (p, b) => request(p, 'PUT', b),
  del: (p) => request(p, 'DELETE'),
}

/* Relative time, in the smallest unit that still says something useful. */
export function ago(iso) {
  if (!iso) return '—'
  const secs = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000)
  if (secs < 60) return `${Math.round(secs)}s ago`
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`
  if (secs < 86400) return `${Math.round(secs / 3600)}h ago`
  return `${Math.round(secs / 86400)}d ago`
}

export function short(sha) {
  return (sha || '').slice(0, 8)
}
