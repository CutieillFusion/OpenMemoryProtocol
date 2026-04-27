// SPA mode: no SSR, no prerendering. Every route is client-rendered from
// the SPA fallback HTML emitted by adapter-static (`200.html`). The gateway
// serves that fallback for unknown paths under /ui/*, which is what makes
// dynamic routes like /file/[...path] resolve client-side.
export const ssr = false;
export const prerender = false;
