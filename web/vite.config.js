import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // The whole panel is embedded in the Rust binary, so fewer, larger files
    // beat many small ones — there is no CDN to parallelise against.
    rollupOptions: { output: { manualChunks: undefined } },
  },
  server: {
    port: 5273,
    proxy: {
      '/api': 'http://127.0.0.1:8788',
      '/healthz': 'http://127.0.0.1:8788',
    },
  },
})
