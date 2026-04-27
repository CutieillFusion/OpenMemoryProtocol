import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

const apiPaths = [
  '/files',
  '/bytes',
  '/tree',
  '/query',
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
  '/probes'
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
