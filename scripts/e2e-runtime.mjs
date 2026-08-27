import { spawn } from 'node:child_process';
import { mkdir, writeFile } from 'node:fs/promises';
import net from 'node:net';
import path from 'node:path';

export const E2E_PROFILE = process.env.WOODLAND_E2E_PROFILE || 'full';
if (!['smoke', 'full', 'soak'].includes(E2E_PROFILE)) {
  throw new Error(`WOODLAND_E2E_PROFILE must be smoke, full, or soak; got ${E2E_PROFILE}`);
}
export const FULL_E2E = E2E_PROFILE === 'full';
export const SOAK_E2E = E2E_PROFILE === 'soak';

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export function startProcess(command, args, cwd) {
  // A dedicated process group lets stopProcess signal descendants too:
  // geckodriver's Firefox children and the web server's maintenance watcher
  // must not survive a failed run.
  const child = spawn(command, args, {
    cwd,
    detached: process.platform !== 'win32',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  let spawnError = null;
  const collect = (chunk) => {
    output += chunk.toString();
    if (output.length > 20_000) output = output.slice(-20_000);
  };
  child.stdout.on('data', collect);
  child.stderr.on('data', collect);
  child.on('error', (error) => {
    spawnError = error;
    collect(`${command} failed to start: ${error.message}\n`);
  });
  return { child, output: () => output, spawnError: () => spawnError };
}

export function assertPortAvailable(port, label) {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.once('error', (error) => reject(
      new Error(`${label} port ${port} is unavailable: ${error.message}`),
    ));
    server.listen({ host: '127.0.0.1', port, exclusive: true }, () => server.close(resolve));
  });
}

export async function waitForHttp(url, timeoutMs = 20_000, process = null) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (process?.spawnError()) throw process.spawnError();
    if (process && process.child.exitCode !== null) {
      throw new Error(`process exited while waiting for ${url}:\n${process.output()}`);
    }
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(5_000) });
      if (response.ok) return;
    } catch {}
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${url}`);
}

export async function startGeckodriver(args, cwd, statusUrl, label) {
  const failures = [];
  for (let attempt = 1; attempt <= 2; attempt += 1) {
    const process = startProcess('geckodriver', args, cwd);
    try {
      await waitForHttp(statusUrl, 20_000, process);
      return process;
    } catch (error) {
      failures.push(
        `attempt ${attempt}: ${error instanceof Error ? error.message : error}`
          + (process.output() ? `\n${process.output()}` : ''),
      );
      await stopProcess(process);
      if (attempt < 2) await sleep(500);
    }
  }
  throw new Error(`${label} failed to start twice:\n${failures.join('\n')}`);
}

export async function webdriverRequest(
  driverUrl,
  method,
  pathName,
  body,
  timeoutMs = 130_000,
) {
  const response = await fetch(`${driverUrl}${pathName}`, {
    method,
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  const payload = await response.json();
  if (!response.ok || payload.value?.error) throw new Error(JSON.stringify(payload));
  return payload.value;
}

export async function saveScreenshot(driverUrl, sessionId, filename) {
  const artifactDir = process.env.WOODLAND_E2E_ARTIFACT_DIR;
  if (!artifactDir || !sessionId) return;
  try {
    const encoded = await webdriverRequest(
      driverUrl,
      'GET',
      `/session/${sessionId}/screenshot`,
      undefined,
      10_000,
    );
    if (typeof encoded !== 'string') throw new Error('WebDriver returned no image');
    await mkdir(artifactDir, { recursive: true });
    await writeFile(path.join(artifactDir, filename), Buffer.from(encoded, 'base64'));
  } catch (error) {
    console.error(`failed to save ${filename}: ${error instanceof Error ? error.message : error}`);
  }
}

export async function waitFor(label, inspect, accept, timeoutMs = 120_000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = await inspect();
    if (last?.error) throw new Error(`${label} failed: ${last.error}\n${last.log || ''}`);
    if (accept(last)) return last;
    await sleep(250);
  }
  throw new Error(`${label} timed out: ${JSON.stringify(last)}`);
}

function waitForExit(child, timeoutMs) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve(true);
  return new Promise((resolve) => {
    let timer;
    const finish = (exited) => {
      clearTimeout(timer);
      child.removeListener('exit', onExit);
      resolve(exited);
    };
    const onExit = () => finish(true);
    child.once('exit', onExit);
    timer = setTimeout(() => finish(false), timeoutMs);
  });
}

function signalProcess(child, signal) {
  if (process.platform !== 'win32' && child.pid) {
    try {
      process.kill(-child.pid, signal);
      return;
    } catch {}
  }
  try { child.kill(signal); } catch {}
}

export async function stopProcess(process) {
  if (!process || process.child.exitCode !== null || process.child.signalCode !== null) return;
  signalProcess(process.child, 'SIGTERM');
  if (!(await waitForExit(process.child, 5_000))) {
    signalProcess(process.child, 'SIGKILL');
    await waitForExit(process.child, 2_000);
  }
}
