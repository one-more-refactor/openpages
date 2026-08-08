/* A router in fourteen lines. The panel has six screens; anything larger
   would be more code than the screens it routes to. */

let listeners = new Set()

export function go(path) {
  if (path === location.pathname) return
  history.pushState({}, '', path)
  listeners.forEach((fn) => fn(path))
}

export function onNavigate(fn) {
  listeners.add(fn)
  const pop = () => fn(location.pathname)
  addEventListener('popstate', pop)
  return () => {
    listeners.delete(fn)
    removeEventListener('popstate', pop)
  }
}
