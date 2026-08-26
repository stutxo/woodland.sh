// Polling + HTTP helpers, built on Node 18+ global fetch / AbortSignal.timeout.
// These replace the curl + sleep retry loops from the bash scripts.
import { log, fail } from './log.mjs';

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Poll `check` (sync or async, truthy = ready) until it passes or attempts run
// out. Returns true on success, false on timeout. Thrown errors count as "not
// ready yet" and are swallowed.
export async function waitFor(label, check, { attempts = 30, intervalMs = 2000 } = {}) {
  for (let i = 1; i <= attempts; i++) {
    try {
      if (await check()) return true;
    } catch {
      // not ready yet
    }
    log(`Waiting for ${label}... (${i}/${attempts})`);
    await sleep(intervalMs);
  }
  return false;
}

// Same as waitFor but aborts the whole run with a clear error on timeout.
export async function waitForOrFail(label, check, opts) {
  if (!(await waitFor(label, check, opts))) {
    fail(`${label} did not become ready in time`);
  }
}

// Never throws on network errors — returns { ok: false } instead, so callers
// and waitFor() can treat "connection refused" as "not ready yet". Uses a
// manually-cleared timer (not AbortSignal.timeout) to avoid leaving a dangling
// libuv handle that crashes the process on exit (Windows).
export async function fetchText(url, { method = 'GET', body, headers, timeoutMs = 8000 } = {}) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(url, { method, body, headers, signal: ctrl.signal });
    return { ok: res.ok, status: res.status, text: await res.text() };
  } catch (error) {
    return { ok: false, status: 0, text: '', error };
  } finally {
    clearTimeout(timer);
  }
}

export async function fetchJson(url, opts = {}) {
  const { text, ok, status } = await fetchText(url, opts);
  try {
    return { ok, status, json: JSON.parse(text) };
  } catch {
    return { ok, status, json: null, text };
  }
}

export async function httpOk(url, timeoutMs = 5000) {
  const { ok } = await fetchText(url, { timeoutMs });
  return ok;
}
