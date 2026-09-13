import { defineConfig } from 'vite'
import { fileURLToPath } from 'node:url'
import { TanStackRouterVite } from '@tanstack/router-plugin/vite'
import viteReact from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// Where vite dev proxies `/api` + `/health` + `/ws`. Override with
// `CS_BACKEND=http://127.0.0.1:54321 pnpm dev` so the dev server can
// point at a shell-spawned backend on a random port (Phase 1+ flow).
const BACKEND = process.env.CS_BACKEND || 'http://127.0.0.1:3001'

// We dropped `@tanstack/react-start/plugin/vite` (SSR) in favour of
// the plain router code-gen plugin. The desktop shell loads the built
// `dist/` straight from our axum binary; SSR adds a Node runtime and
// a reverse-proxy hop for zero user-visible benefit on localhost.
//
// `@tanstack/devtools-vite` is intentionally NOT in this list — see
// the historical commit; it formed a positive feedback loop with vite
// HMR logs. Re-enable only if upstream fixes it.
export default defineConfig({
  resolve: { tsconfigPaths: true },
  plugins: [
    TanStackRouterVite({
      target: 'react',
      autoCodeSplitting: true,
    }),
    tailwindcss(),
    viteReact(),
  ],
  server: {
    strictPort: true,
    proxy: {
      '/api': {
        target: BACKEND,
        changeOrigin: false,
        ws: true,
      },
      '/health': { target: BACKEND, changeOrigin: false },
    },
  },
  build: {
    // Self-contained SPA output. The axum server points its
    // `--static-dir` at this directory; client routes (e.g. `/`, any
    // future `/settings`) all resolve back to the same `index.html`
    // via axum's SPA fallback.
    //
    // Second entry: `hmi.html`, the standalone operator panel that
    // `ia2-runtime --static-dir` serves at `/hmi/<screen>` on an edge
    // box. Separate entry so an operator tablet never downloads the
    // IDE bundle.
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL('./index.html', import.meta.url)),
        hmi: fileURLToPath(new URL('./hmi.html', import.meta.url)),
      },
    },
  },
})
