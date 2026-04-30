import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

const apiPaths = [
  '/files',
  '/bytes',
  '/tree',
  '/query',
  '/schemas',
  '/watch',
  '/commit',
  '/log',
  '/diff',
  '/branches',
  '/checkout',
  '/audit',
  '/uploads',
  '/status',
  '/init',
  '/healthz',
  '/livez',
  '/readyz',
  '/startupz',
  '/metrics',
  '/test',
  '/probes',
  // OIDC routes. Cookies set on `/auth/callback` need to land on the same
  // origin as the API calls, so proxy `/auth/*` through to the gateway in dev too.
  '/auth'
];

const target = process.env.OMP_DEV_BACKEND ?? 'http://127.0.0.1:8000';

const proxy = Object.fromEntries(
  apiPaths.map((p) => [
    p,
    {
      target,
      changeOrigin: true,
      ws: false
    }
  ])
);

export default defineConfig({
  plugins: [sveltekit()],
  server: {
    port: 5173,
    proxy
  }
});
