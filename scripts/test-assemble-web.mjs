#!/usr/bin/env node
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const temporary = await mkdtemp(path.join(os.tmpdir(), 'woodland-web-test-'));
const assembler = path.join(ROOT, 'scripts/assemble-web.mjs');
const base = JSON.parse(await readFile(path.join(ROOT, 'mutinynet/woodland-world.json'), 'utf8'));

function run(manifestPath, outputPath, env = {}) {
  return spawnSync(process.execPath, [assembler, manifestPath, outputPath], {
    cwd: ROOT,
    encoding: 'utf8',
    env: { ...process.env, ...env },
  });
}

try {
  const validManifest = path.join(temporary, 'valid.json');
  const validOutput = path.join(temporary, 'valid');
  await writeFile(validManifest, JSON.stringify(base));
  const valid = run(validManifest, validOutput, {
    WOODLAND_LEADERBOARD_URL: 'https://leaderboard.example',
  });
  assert.equal(valid.status, 0, valid.stderr);

  const index = await readFile(path.join(validOutput, 'index.html'), 'utf8');
  const app = await readFile(path.join(validOutput, 'app.js'), 'utf8');
  const notFound = await readFile(path.join(validOutput, '404.html'), 'utf8');
  assert.match(index, /http-equiv="Content-Security-Policy"/);
  assert.ok(index.includes(base.arkadeServiceUrl));
  assert.ok(index.includes(base.emulatorUrl));
  assert.match(index, /name="woodland-leaderboard" content="https:\/\/leaderboard\.example"/);
  assert.match(index, /connect-src[^"]*https:\/\/leaderboard\.example/);
  assert.doesNotMatch(index, /WOODLAND_CSP/);
  assert.match(index, /src="\.\/app\.js"/);
  assert.match(app, /new URL\('\.\/world\.json', import\.meta\.url\)/);
  assert.match(notFound, /href="\.\/"/);
  assert.equal(await readFile(path.join(validOutput, '.nojekyll'), 'utf8'), '');
  assert.match(notFound, /Not found/);
  await assert.rejects(readFile(path.join(validOutput, '_headers')));

  const unconfiguredOutput = path.join(temporary, 'unconfigured');
  const unconfigured = run(validManifest, unconfiguredOutput, {
    WOODLAND_LEADERBOARD_URL: '',
  });
  assert.equal(unconfigured.status, 0, unconfigured.stderr);
  const unconfiguredIndex = await readFile(path.join(unconfiguredOutput, 'index.html'), 'utf8');
  assert.match(unconfiguredIndex, /name="woodland-leaderboard" content=""/);
  assert.doesNotMatch(unconfiguredIndex, /leaderboard\.example/);

  const insecureManifest = path.join(temporary, 'insecure-mainnet.json');
  await writeFile(insecureManifest, JSON.stringify({
    ...base,
    network: 'bitcoin',
    arkadeServiceUrl: 'http://arkade.invalid',
    emulatorUrl: 'https://emulator.invalid',
  }));
  const insecure = run(insecureManifest, path.join(temporary, 'insecure'));
  assert.notEqual(insecure.status, 0);
  assert.match(insecure.stderr, /mainnet web bundles require HTTPS service URLs/);

  const insecureLeaderboardManifest = path.join(temporary, 'insecure-leaderboard.json');
  await writeFile(insecureLeaderboardManifest, JSON.stringify({
    ...base,
    network: 'bitcoin',
    arkadeServiceUrl: 'https://arkade.invalid',
    emulatorUrl: 'https://emulator.invalid',
  }));
  const insecureLeaderboard = run(
    insecureLeaderboardManifest,
    path.join(temporary, 'insecure-leaderboard'),
    { WOODLAND_LEADERBOARD_URL: 'http://leaderboard.invalid' },
  );
  assert.notEqual(insecureLeaderboard.status, 0);
  assert.match(insecureLeaderboard.stderr, /must be a canonical HTTPS origin on mainnet/);

  const wrongProtocol = path.join(temporary, 'wrong-protocol.json');
  await writeFile(wrongProtocol, JSON.stringify({ ...base, protocolVersion: 2 }));
  const wrong = run(wrongProtocol, path.join(temporary, 'wrong'));
  assert.notEqual(wrong.status, 0);
  assert.match(wrong.stderr, /requires a woodland\.sh protocol v1 manifest/);

  console.log('GitHub Pages artifact tests passed');
} finally {
  await rm(temporary, { recursive: true, force: true });
}
