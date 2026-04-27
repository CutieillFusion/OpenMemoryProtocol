import { getToken } from './auth';
import type { WatchEvent } from './types';

export interface WatchOptions {
  onEvent: (event: WatchEvent) => void;
  onError?: (err: unknown) => void;
  onOpen?: () => void;
  /** Path defaults to `/watch`. */
  path?: string;
}

export interface WatchHandle {
  close: () => void;
}

/**
 * Subscribe to the gateway's SSE stream at `/watch`.
 *
 * Implemented over `fetch` (not `EventSource`) because EventSource cannot
 * send custom headers, and we need `Authorization: Bearer ...`.
 *
 * Auto-reconnects on transport errors with capped exponential backoff.
 */
export function watch(opts: WatchOptions): WatchHandle {
  const path = opts.path ?? '/watch';
  let aborted = false;
  let abort: AbortController | null = null;
  let backoff = 500;
  const MAX_BACKOFF = 15_000;

  const run = async () => {
    while (!aborted) {
      abort = new AbortController();
      try {
        const headers: Record<string, string> = { Accept: 'text/event-stream' };
        const tok = getToken();
        if (tok) headers['Authorization'] = `Bearer ${tok}`;
        const resp = await fetch(path, { headers, signal: abort.signal });
        if (!resp.ok || !resp.body) {
          throw new Error(`watch: HTTP ${resp.status}`);
        }
        opts.onOpen?.();
        backoff = 500; // success — reset backoff

        const reader = resp.body.getReader();
        const decoder = new TextDecoder('utf-8');
        let buf = '';

        while (!aborted) {
          const { value, done } = await reader.read();
          if (done) break;
          buf += decoder.decode(value, { stream: true });
          // SSE frames are delimited by a blank line.
          let idx;
          while ((idx = buf.indexOf('\n\n')) !== -1) {
            const frame = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            const evt = parseFrame(frame);
            if (evt) {
              try {
                opts.onEvent(evt);
              } catch (e) {
                // Don't kill the stream on a downstream handler error.
                opts.onError?.(e);
              }
            }
          }
        }
      } catch (err) {
        if (aborted) return;
        opts.onError?.(err);
      }
      if (aborted) return;
      // Reconnect after backoff. Capped exponential.
      await sleep(backoff);
      backoff = Math.min(backoff * 2, MAX_BACKOFF);
    }
  };

  void run();

  return {
    close: () => {
      aborted = true;
      abort?.abort();
    }
  };
}

/** Stream raw line-data events from any SSE endpoint, calling `onLine`
 *  with each `data:` line. Used by the build-log panel to render cargo
 *  output as it streams. Auto-reconnects with capped exponential backoff. */
export function streamLines(opts: {
  path: string;
  onLine: (line: string) => void;
  onError?: (err: unknown) => void;
  onOpen?: () => void;
  onClose?: () => void;
}): { close: () => void } {
  let aborted = false;
  let abort: AbortController | null = null;

  const run = async () => {
    abort = new AbortController();
    try {
      const headers: Record<string, string> = { Accept: 'text/event-stream' };
      const tok = getToken();
      if (tok) headers['Authorization'] = `Bearer ${tok}`;
      const resp = await fetch(opts.path, { headers, signal: abort.signal });
      if (!resp.ok || !resp.body) throw new Error(`stream: HTTP ${resp.status}`);
      opts.onOpen?.();
      const reader = resp.body.getReader();
      const decoder = new TextDecoder('utf-8');
      let buf = '';
      while (!aborted) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf('\n\n')) !== -1) {
          const frame = buf.slice(0, idx);
          buf = buf.slice(idx + 2);
          let data = '';
          for (const line of frame.split('\n')) {
            if (line.startsWith('data:')) data += line.slice(5).trimStart();
          }
          if (data) opts.onLine(data);
        }
      }
      opts.onClose?.();
    } catch (err) {
      if (!aborted) opts.onError?.(err);
    }
  };

  void run();
  return {
    close: () => {
      aborted = true;
      abort?.abort();
    }
  };
}

function parseFrame(frame: string): WatchEvent | null {
  // Per the SSE spec each line is `field: value`. We only care about `data:`.
  // The server emits one `data:` line per event with a JSON body.
  const lines = frame.split('\n');
  let data = '';
  for (const line of lines) {
    if (line.startsWith('data:')) {
      data += line.slice(5).trimStart();
    }
  }
  if (!data) return null;
  try {
    return JSON.parse(data) as WatchEvent;
  } catch {
    return null;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((res) => setTimeout(res, ms));
}
