#!/usr/bin/env node
// Local preview and renewal supervisor. Production static hosting is Pages.
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { readFile, stat } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const configuredSetting = (name, fallback) => process.env[name] || fallback;
const WORLD_MANIFEST = path.resolve(
  ROOT,
  configuredSetting('WOODLAND_WORLD_MANIFEST', 'regtest/_build/woodland-world.json'),
);
const WEB_ROOT = path.resolve(ROOT, process.env.WOODLAND_WEB_ROOT || 'dist');
const watcherEnvironment = { ...process.env };
delete watcherEnvironment.WOODLAND_DEPLOYER_SECRET;
for (const name of [
  'WOODLAND_DEPLOYER_SECRET',
  'WOODLAND_ROLLOVER_SECRET',
]) {
  delete process.env[name];
}

const args = process.argv.slice(2);
const valueAfter = (flag, fallback) => {
  const index = args.indexOf(flag);
  return index === -1 ? fallback : args[index + 1] || fallback;
};
const port = Number(valueAfter('--port', process.env.WOODLAND_WEB_PORT || '8000'));
const host = valueAfter('--host', process.env.WOODLAND_WEB_HOST || '127.0.0.1');
const targetDir = path.resolve(ROOT, process.env.CARGO_TARGET_DIR || 'target');
const OPERATOR_BIN = path.resolve(
  configuredSetting(
    'WOODLAND_OPERATOR_BIN',
    path.join(targetDir, 'debug', 'woodland-operator'),
  ),
);
const ARKADE_UPSTREAM = configuredSetting(
  'WOODLAND_ARKADE_SERVICE_URL',
  'http://127.0.0.1:7070',
).replace(/\/+$/, '');
const EMULATOR_UPSTREAM = configuredSetting(
  'WOODLAND_EMULATOR_URL',
  'http://127.0.0.1:7073',
).replace(/\/+$/, '');

const watcherLockId = createHash('sha256').update(WORLD_MANIFEST).digest('hex').slice(0, 16);
const WATCHER_LOCK = process.env.WOODLAND_WATCHER_LOCK_FILE
  || path.join('/tmp', `woodland-renewal-${watcherLockId}.lock`);
const WATCHER_RESTART_MAX_MS = 30_000;
let watcherReady = false;
let watcherFailure = null;
let watcher = null;
let watcherRestartTimer = null;
let watcherRestartDelayMs = 1_000;
let shuttingDown = false;

const contentTypes = new Map([
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.wasm', 'application/wasm'],
  ['.json', 'application/json; charset=utf-8'],
]);

function send(response, status, body, headers = {}) {
  response.writeHead(status, {
    'cache-control': 'no-store',
    'content-type': 'text/plain; charset=utf-8',
    ...headers,
  });
  response.end(body);
}

async function serveStatic(response, url) {
  const pathname = url.pathname === '/' ? '/index.html' : url.pathname;
  const decoded = decodeURIComponent(pathname);
  let filename = path.resolve(WEB_ROOT, `.${decoded}`);
  if (!filename.startsWith(`${WEB_ROOT}${path.sep}`)) {
    send(response, 403, 'Forbidden');
    return;
  }
  try {
    const info = await stat(filename);
    if (info.isDirectory()) filename = path.join(filename, 'index.html');
    const body = await readFile(filename);
    response.writeHead(200, {
      'cache-control': 'no-store',
      'content-type': contentTypes.get(path.extname(filename)) || 'application/octet-stream',
    });
    response.end(body);
  } catch (error) {
    send(response, error?.code === 'ENOENT' ? 404 : 500, 'Not found');
  }
}

const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url || '/', `http://${request.headers.host || `${host}:${port}`}`);
    if (url.pathname === '/health.json') {
      const healthy = watcherReady && !watcherFailure;
      send(
        response,
        healthy ? 200 : 503,
        JSON.stringify({ ready: healthy, error: watcherFailure }),
        { 'content-type': 'application/json; charset=utf-8' },
      );
      return;
    }
    if (!['GET', 'HEAD'].includes(request.method || 'GET')) {
      send(response, 405, 'Method not allowed');
      return;
    }
    await serveStatic(response, url);
  } catch (error) {
    console.error('woodland.sh request failed', error);
    send(response, 500, `Web development server failed: ${error}`);
  }
});

server.listen(port, host, () => {
  console.log(`woodland.sh: http://${host}:${port}/`);
  console.log(`Browser connects directly to ${ARKADE_UPSTREAM} and ${EMULATOR_UPSTREAM}`);
});

function readWatcherOutput(message) {
  for (const line of message.split('\n').filter(Boolean)) {
    if (
      line.includes('woodland.sh renewal watcher ready')
      || line.includes('woodland.sh renewal watcher recovered')
    ) {
      watcherReady = true;
      watcherFailure = null;
      watcherRestartDelayMs = 1_000;
    } else if (
      line.includes('woodland.sh renewal watcher:')
      || line.includes('woodland.sh renewal reconnect:')
    ) {
      watcherReady = false;
      watcherFailure = line.trim();
    }
  }
}

function scheduleWatcherRestart() {
  if (shuttingDown || watcherRestartTimer) return;
  const delay = watcherRestartDelayMs;
  watcherRestartDelayMs = Math.min(watcherRestartDelayMs * 2, WATCHER_RESTART_MAX_MS);
  watcherRestartTimer = setTimeout(() => {
    watcherRestartTimer = null;
    startWatcher();
  }, delay);
}

function startWatcher() {
  if (shuttingDown) return;
  watcherReady = false;
  const child = spawn(
    'flock',
    [
      '--exclusive',
      '--nonblock',
      '--no-fork',
      '--conflict-exit-code',
      '73',
      WATCHER_LOCK,
      OPERATOR_BIN,
      'watch',
      WORLD_MANIFEST,
    ],
    {
      cwd: ROOT,
      stdio: ['ignore', 'ignore', 'pipe'],
      env: watcherEnvironment,
    },
  );
  watcher = child;
  child.stderr.on('data', (chunk) => {
    const message = chunk.toString();
    readWatcherOutput(message);
    process.stderr.write(chunk);
  });
  child.on('error', (error) => {
    if (watcher !== child || shuttingDown) return;
    watcherReady = false;
    watcherFailure = error.message;
    console.error(`woodland.sh renewal watcher failed: ${error.message}`);
  });
  child.on('exit', (code, signal) => {
    if (watcher !== child) return;
    watcher = null;
    if (shuttingDown) return;
    watcherReady = false;
    watcherFailure = code === 73
      ? 'another renewal watcher owns this world'
      : `watcher exited with ${signal || `status ${code}`}`;
    console.error(`woodland.sh ${watcherFailure}`);
    scheduleWatcherRestart();
  });
}

startWatcher();

function shutdown(signal) {
  shuttingDown = true;
  clearTimeout(watcherRestartTimer);
  watcherRestartTimer = null;
  watcher?.kill(signal);
  server.close(() => process.exit(0));
}
process.once('SIGINT', () => shutdown('SIGINT'));
process.once('SIGTERM', () => shutdown('SIGTERM'));
