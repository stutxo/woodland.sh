#!/usr/bin/env node
import http from 'node:http';

const HOST = process.env.WOODLAND_CHAOS_PROXY_HOST || '127.0.0.1';
const PORT = Number(process.env.WOODLAND_CHAOS_PROXY_PORT || 7075);
const UPSTREAM = new URL(process.env.WOODLAND_CHAOS_UPSTREAM_URL || 'http://127.0.0.1:7073');
const MAX_CONTROL_BODY_BYTES = 16 * 1024;
const UPSTREAM_TIMEOUT_MS = 180_000;
const TX_PATH = '/v1/tx';
const CONTROL_PATH = '/__chaos';
const MODES = new Set(['pass', 'fail-before', 'fail-after-success']);

if (!Number.isInteger(PORT) || PORT < 1 || PORT > 65_535) {
  throw new Error('WOODLAND_CHAOS_PROXY_PORT must be an integer from 1 to 65535');
}
if (!['http:', 'https:'].includes(UPSTREAM.protocol)) {
  throw new Error('WOODLAND_CHAOS_UPSTREAM_URL must use http or https');
}

let nextEventId = 1;
let event = newEvent('pass', 0);
const totals = {
  requests: 0,
  txRequests: 0,
  forwarded: 0,
  failedBefore: 0,
  maskedSuccesses: 0,
  upstreamFailures: 0,
};

function newEvent(mode, remaining) {
  return {
    id: nextEventId++,
    mode,
    configuredRemaining: remaining,
    remaining,
    txRequests: 0,
    forwarded: 0,
    failedBefore: 0,
    maskedSuccesses: 0,
    upstreamFailures: 0,
  };
}

function snapshot() {
  return {
    ready: true,
    upstream: UPSTREAM.toString().replace(/\/$/, ''),
    event: { ...event },
    totals: { ...totals },
  };
}

function sendJson(response, status, payload) {
  const body = Buffer.from(`${JSON.stringify(payload)}\n`);
  response.writeHead(status, {
    'access-control-allow-origin': '*',
    'cache-control': 'no-store',
    'content-length': String(body.length),
    'content-type': 'application/json',
  });
  response.end(body);
}

function sendInternalError(response) {
  sendJson(response, 500, { code: 13, message: 'internal error', details: [] });
}

async function readBody(request, maximum = Number.POSITIVE_INFINITY) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of request) {
    bytes += chunk.length;
    if (bytes > maximum) throw new Error(`request body exceeds ${maximum} bytes`);
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

function consumeFault() {
  if (event.remaining === 0) return false;
  if (event.remaining !== null) event.remaining -= 1;
  return true;
}

function recordFault(kind) {
  event[kind] += 1;
  totals[kind] += 1;
  const count = event[kind];
  if (count <= 3 || count % 10 === 0) {
    console.log(`chaos event ${event.id}: ${kind} ${count}, remaining ${event.remaining ?? 'unlimited'}`);
  }
}

function configure(payload) {
  const mode = payload?.mode;
  if (!MODES.has(mode)) {
    throw new Error(`mode must be one of ${[...MODES].join(', ')}`);
  }
  let remaining = payload.remaining;
  if (mode === 'pass') {
    remaining = 0;
  } else if (remaining !== null && (!Number.isInteger(remaining) || remaining < 1)) {
    throw new Error('remaining must be null or a positive integer for a fault mode');
  }
  if (mode === 'fail-after-success' && remaining === null) {
    throw new Error('fail-after-success requires a finite remaining count');
  }
  event = newEvent(mode, remaining);
  console.log(`chaos event ${event.id}: mode ${mode}, remaining ${remaining ?? 'unlimited'}`);
  return snapshot();
}

async function proxyRequest(request, response, requestUrl) {
  const body = ['GET', 'HEAD'].includes(request.method) ? undefined : await readBody(request);
  const target = new URL(`${requestUrl.pathname}${requestUrl.search}`, UPSTREAM);
  const headers = new Headers();
  for (const [name, value] of Object.entries(request.headers)) {
    if (value == null || ['connection', 'content-length', 'host', 'transfer-encoding'].includes(name)) {
      continue;
    }
    if (Array.isArray(value)) {
      for (const item of value) headers.append(name, item);
    } else {
      headers.set(name, value);
    }
  }
  const upstreamResponse = await fetch(target, {
    method: request.method,
    headers,
    body,
    redirect: 'manual',
    signal: AbortSignal.timeout(UPSTREAM_TIMEOUT_MS),
  });
  const responseBody = Buffer.from(await upstreamResponse.arrayBuffer());
  event.forwarded += 1;
  totals.forwarded += 1;

  if (
    request.method === 'POST'
    && requestUrl.pathname === TX_PATH
    && event.mode === 'fail-after-success'
    && upstreamResponse.ok
    && consumeFault()
  ) {
    recordFault('maskedSuccesses');
    sendInternalError(response);
    return;
  }

  const responseHeaders = {};
  for (const [name, value] of upstreamResponse.headers.entries()) {
    if (['connection', 'content-encoding', 'content-length', 'transfer-encoding'].includes(name)) continue;
    responseHeaders[name] = value;
  }
  responseHeaders['content-length'] = String(responseBody.length);
  response.writeHead(upstreamResponse.status, responseHeaders);
  response.end(responseBody);
}

const server = http.createServer(async (request, response) => {
  totals.requests += 1;
  const requestUrl = new URL(request.url || '/', `http://${request.headers.host || `${HOST}:${PORT}`}`);
  try {
    if (requestUrl.pathname === `${CONTROL_PATH}/status` && request.method === 'GET') {
      sendJson(response, 200, snapshot());
      return;
    }
    if (requestUrl.pathname === `${CONTROL_PATH}/config` && request.method === 'POST') {
      const body = await readBody(request, MAX_CONTROL_BODY_BYTES);
      sendJson(response, 200, configure(JSON.parse(body.toString('utf8'))));
      return;
    }
    if (requestUrl.pathname.startsWith(`${CONTROL_PATH}/`)) {
      sendJson(response, 404, { error: 'unknown chaos control route' });
      return;
    }

    if (request.method === 'POST' && requestUrl.pathname === TX_PATH) {
      event.txRequests += 1;
      totals.txRequests += 1;
      if (event.mode === 'fail-before' && consumeFault()) {
        recordFault('failedBefore');
        sendInternalError(response);
        return;
      }
    }
    await proxyRequest(request, response, requestUrl);
  } catch (error) {
    event.upstreamFailures += 1;
    totals.upstreamFailures += 1;
    sendJson(response, 502, {
      code: 13,
      message: 'chaos proxy upstream failure',
      details: [error instanceof Error ? error.message : String(error)],
    });
  }
});

server.listen(PORT, HOST, () => {
  console.log(`woodland emulator chaos proxy ready at http://${HOST}:${PORT} -> ${UPSTREAM}`);
});

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
