#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import {
  assertPortAvailable,
  saveScreenshot,
  sleep,
  startGeckodriver,
  stopProcess,
  waitFor,
  waitForHttp,
  webdriverRequest,
} from './e2e-runtime.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const WEB_URL = (process.env.WOODLAND_E2E_WEB_URL || 'http://127.0.0.1:8090').replace(/\/$/, '');
const PLAYER_COUNT = setting('WOODLAND_SOAK_PLAYERS', 12, 2, 32);
const ROUNDS = setting('WOODLAND_SOAK_ROUNDS', 30, 1, 1_000);
const ACTIVATION_CONCURRENCY = setting('WOODLAND_SOAK_ACTIVATION_CONCURRENCY', 3, 1, 8);
const RACE_CONCURRENCY = setting(
  'WOODLAND_SOAK_RACE_CONCURRENCY',
  Math.min(4, PLAYER_COUNT),
  1,
  PLAYER_COUNT,
);
const ROUND_DELAY_MS = setting('WOODLAND_SOAK_ROUND_DELAY_MS', 500, 0, 60_000);
const DRIVER_BASE_PORT = setting('WOODLAND_SOAK_DRIVER_PORT', 15_500, 1_024, 60_000);
const OPERATION_TIMEOUT_MS = setting('WOODLAND_SOAK_TIMEOUT_MS', 240_000, 30_000, 900_000);
const reportPath = path.resolve(
  ROOT,
  process.env.WOODLAND_SOAK_REPORT || 'regtest/_build/soak-report.json',
);
const FUND_COMMAND = process.env.WOODLAND_SOAK_FUND_COMMAND;

function setting(name, fallback, minimum, maximum) {
  const value = Number(process.env[name] || fallback);
  if (!Number.isInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be an integer from ${minimum} to ${maximum}`);
  }
  return value;
}

async function mapLimit(items, concurrency, operation) {
  const results = Array(items.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(concurrency, items.length) }, async () => {
    while (next < items.length) {
      const index = next;
      next += 1;
      results[index] = await operation(items[index], index);
    }
  }));
  return results;
}

async function createPlayer(driverUrl, label) {
  const session = await webdriverRequest(driverUrl, 'POST', '/session', {
    capabilities: {
      alwaysMatch: {
        browserName: 'firefox',
        unhandledPromptBehavior: 'accept',
        'moz:firefoxOptions': { args: ['-headless'] },
      },
    },
  }, OPERATION_TIMEOUT_MS);
  assert.ok(session.sessionId, `${label} WebDriver session has no ID`);
  const sessionId = session.sessionId;
  const wd = (method, suffix, body, timeout = OPERATION_TIMEOUT_MS) => (
    webdriverRequest(driverUrl, method, `/session/${sessionId}${suffix}`, body, timeout)
  );
  const execute = (script, args = []) => wd('POST', '/execute/sync', { script, args });
  const executeAsync = (script, args = []) => wd('POST', '/execute/async', { script, args });
  const inspect = () => execute(`
    return {
      ready: Boolean(globalThis.__WOODLAND_E2E_READY),
      error: globalThis.__WOODLAND_E2E_ERROR || '',
      state: globalThis.__WOODLAND_E2E_STATE || null,
      player: globalThis.__WOODLAND_E2E_PLAYER || null,
      serverRegistered: Boolean(globalThis.__WOODLAND_E2E_SERVER_REGISTERED),
      log: document.getElementById('log')?.textContent || '',
    };
  `);
  const refresh = () => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_REFRESH()
      .then((state) => done({ state }))
      .catch((error) => done({ error: String(error) }));
  `);
  const race = (snapshot, tree) => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_CHOP_EXPECTED(...Array.from(arguments).slice(0, -1)).then(done);
  `, [tree.treeId, tree.treeOutpoint, snapshot.playerStateOutpoint, tree.nextDrop]);
  await wd('POST', '/timeouts', { implicit: 0, pageLoad: 30_000, script: OPERATION_TIMEOUT_MS });
  await wd('POST', '/url', { url: `${WEB_URL}/?soak=${encodeURIComponent(label)}` });
  return { label, driverUrl, sessionId, wd, execute, executeAsync, inspect, refresh, race };
}

function treeProjection(state) {
  return state.trees.map((tree) => ({
    treeId: tree.treeId,
    health: tree.health,
    logReserveRemaining: tree.logReserveRemaining,
    xpRemaining: tree.xpRemaining,
    valueSats: tree.valueSats,
    treeOutpoint: tree.treeOutpoint,
    lastAttemptTxid: tree.lastAttemptTxid || null,
  }));
}

function assertConverged(views, label) {
  const expected = treeProjection(views[0].state);
  for (const view of views) {
    assert.equal(view.state.playerActive, true, `${label}: inactive player`);
    assert.equal(view.state.pendingChopTxid ?? null, null, `${label}: pending chop`);
    assert.deepEqual(treeProjection(view.state), expected, `${label}: divergent tree projection`);
  }
  const treeLogs = views[0].state.trees.reduce(
    (total, tree) => total + tree.logReserveRemaining,
    0,
  );
  const treeXp = views[0].state.trees.reduce((total, tree) => total + tree.xpRemaining, 0);
  const playerLogs = views.reduce((total, view) => total + view.state.playerLogs, 0);
  const playerXp = views.reduce((total, view) => total + view.state.playerXp, 0);
  assert.equal(treeLogs + playerLogs, 100, `${label}: LOG supply changed`);
  assert.equal(treeXp + playerXp, 100, `${label}: XP supply changed`);
}

function percentile(values, ratio) {
  if (!values.length) return 0;
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * ratio))];
}

const driverConfigs = Array.from({ length: PLAYER_COUNT }, (_, index) => ({
  port: DRIVER_BASE_PORT + index,
  websocketPort: DRIVER_BASE_PORT + PLAYER_COUNT + index,
}));
const drivers = [];
const players = [];
const roundReports = [];
let recoveredUnknownOutcomes = 0;
const startedAt = Date.now();

try {
  const manifestResponse = await fetch(`${WEB_URL}/world.json`, {
    signal: AbortSignal.timeout(20_000),
  });
  if (!manifestResponse.ok) {
    throw new Error(`world manifest returned ${manifestResponse.status}`);
  }
  const manifest = await manifestResponse.json();
  const arkadeHost = new URL(manifest.arkadeServiceUrl).hostname;
  const localFunding = ['127.0.0.1', 'localhost'].includes(arkadeHost);
  if (!localFunding && !FUND_COMMAND) {
    throw new Error(
      'WOODLAND_SOAK_FUND_COMMAND is required for a remote world; it receives <address> <sats>',
    );
  }

  await Promise.all([
    waitForHttp(`${WEB_URL}/health.json`, 20_000),
    waitForHttp(`${manifest.arkadeServiceUrl.replace(/\/$/, '')}/v1/info`, 20_000),
    waitForHttp(`${manifest.emulatorUrl.replace(/\/$/, '')}/v1/info`, 20_000),
    ...driverConfigs.flatMap(({ port, websocketPort }, index) => [
      assertPortAvailable(port, `soak WebDriver ${index + 1}`),
      assertPortAvailable(websocketPort, `soak WebDriver ${index + 1} WebSocket`),
    ]),
  ]);

  for (const [index, config] of driverConfigs.entries()) {
    const driverUrl = `http://127.0.0.1:${config.port}`;
    drivers.push(await startGeckodriver(
      ['--port', String(config.port), '--websocket-port', String(config.websocketPort)],
      ROOT,
      `${driverUrl}/status`,
      `soak WebDriver ${index + 1}`,
    ));
  }
  const created = await mapLimit(
    driverConfigs,
    ACTIVATION_CONCURRENCY,
    ({ port }, index) => createPlayer(`http://127.0.0.1:${port}`, `player-${index + 1}`),
  );
  players.push(...created);

  const initial = await Promise.all(players.map((player) => waitFor(
    `wallet initialization (${player.label})`,
    player.inspect,
    (value) => value.ready
      && !value.state?.playerActive
      && value.state?.fundingRequiredSats === 330,
    OPERATION_TIMEOUT_MS,
  )));
  for (const view of initial) {
    execFileSync(
      FUND_COMMAND || path.join(ROOT, 'scripts/regtest.sh'),
      FUND_COMMAND
        ? [view.state.address, String(view.state.fundingRequiredSats)]
        : ['fund', view.state.address, String(view.state.fundingRequiredSats)],
      { cwd: ROOT, stdio: 'pipe', encoding: 'utf8' },
    );
  }

  await mapLimit(players, ACTIVATION_CONCURRENCY, async (player) => {
    const funded = await player.refresh();
    assert.equal(funded.error, undefined, funded.error);
    await waitFor(
      `activation funding (${player.label})`,
      player.inspect,
      (value) => value.state?.activationReady === true,
      OPERATION_TIMEOUT_MS,
    );
    await player.execute(`document.getElementById('activate').click();`);
    return waitFor(
      `activation and registration (${player.label})`,
      player.inspect,
      (value) => value.state?.playerActive
        && value.serverRegistered
        && Boolean(value.state.playerAsset),
      OPERATION_TIMEOUT_MS,
    );
  });

  let views = await mapLimit(players, ACTIVATION_CONCURRENCY, async (player) => {
    const refreshed = await player.refresh();
    assert.equal(refreshed.error, undefined, refreshed.error);
    return player.inspect();
  });
  assert.equal(new Set(views.map((view) => view.state.playerAsset)).size, PLAYER_COUNT);
  assertConverged(views, 'post-activation');
  await waitFor(
    'server registration convergence',
    async () => {
      const [leaderboard, presence] = await Promise.all([
        fetch(`${WEB_URL}/v1/leaderboard?limit=200`).then((response) => response.json()),
        fetch(`${WEB_URL}/v1/presence?minX=0&minY=0&maxX=44&maxY=18`)
          .then((response) => response.json()),
      ]);
      return { leaderboard, presence };
    },
    (value) => value.leaderboard.total >= PLAYER_COUNT
      && value.presence.locations.length >= PLAYER_COUNT,
    OPERATION_TIMEOUT_MS,
  );

  let targetTreeId = null;
  for (let round = 1; round <= ROUNDS; round += 1) {
    const shared = views[0].state;
    let target = targetTreeId == null
      ? null
      : shared.trees.find((tree) => tree.treeId === targetTreeId && tree.health > 0);
    if (!target) {
      target = shared.trees.find((tree) => tree.health > 0 && tree.logReserveRemaining > 0);
      assert.ok(target, `round ${round}: no live tree remains`);
      targetTreeId = target.treeId;
    }
    const beforeTree = target;
    const beforePlayerOutpoints = views.map((view) => view.state.playerStateOutpoint);
    const roundStartedAt = Date.now();
    const results = await mapLimit(players, RACE_CONCURRENCY, (player, index) => {
      const playerTree = views[index].state.trees.find(
        (tree) => tree.treeId === targetTreeId,
      );
      return player.race(views[index].state, playerTree);
    });
    const reportedAccepted = results
      .map((result, index) => ({ result, index }))
      .filter(({ result }) => result.ok);
    assert.ok(
      reportedAccepted.length <= 1,
      `round ${round}: multiple clients reported an accepted swing: ${JSON.stringify(results)}`,
    );
    if (ROUND_DELAY_MS) await sleep(ROUND_DELAY_MS);
    views = await mapLimit(players, ACTIVATION_CONCURRENCY, async (player) => {
      const refreshed = await player.refresh();
      assert.equal(refreshed.error, undefined, refreshed.error);
      return player.inspect();
    });
    assertConverged(views, `round ${round}`);
    const afterTree = views[0].state.trees.find((tree) => tree.treeId === targetTreeId);
    assert.notEqual(afterTree.treeOutpoint, beforeTree.treeOutpoint, `round ${round}: tree did not rotate`);
    const changedPlayers = views
      .map((view, index) => ({ view, index }))
      .filter(
        ({ view, index }) => view.state.playerStateOutpoint !== beforePlayerOutpoints[index],
      );
    assert.equal(changedPlayers.length, 1, `round ${round}: player transition count`);
    const winner = changedPlayers[0].index;
    assert.equal(
      changedPlayers[0].view.state.playerStateOutpoint.split(':')[0],
      afterTree.treeOutpoint.split(':')[0],
      `round ${round}: player and tree transitions have different transactions`,
    );
    if (reportedAccepted.length === 1) {
      assert.equal(
        reportedAccepted[0].index,
        winner,
        `round ${round}: reported winner differs from committed winner`,
      );
    } else {
      recoveredUnknownOutcomes += 1;
      console.warn(
        `soak round ${round}: player ${winner + 1} committed despite a client error: `
          + `${results[winner].message || 'unknown error'}`,
      );
    }
    roundReports.push({
      round,
      treeId: targetTreeId,
      winner,
      reportedAccepted: reportedAccepted.length === 1,
      recoveryMessage: reportedAccepted.length === 0
        ? results[winner].message || 'unknown error'
        : null,
      drop: afterTree.health < beforeTree.health,
      conflicts: PLAYER_COUNT - 1,
      durationMs: Date.now() - roundStartedAt,
    });
    if (round % 10 === 0 || round === ROUNDS) {
      console.log(
        `soak round ${round}/${ROUNDS}: tree ${targetTreeId}, winner ${winner + 1}, `
          + `${roundReports.at(-1).durationMs}ms`,
      );
    }
  }

  const durations = roundReports.map((round) => round.durationMs);
  const health = await fetch(`${WEB_URL}/health.json`).then((response) => response.json());
  const report = {
    profile: 'soak',
    webUrl: WEB_URL,
    arkadeServiceUrl: manifest.arkadeServiceUrl,
    emulatorUrl: manifest.emulatorUrl,
    players: PLAYER_COUNT,
    rounds: ROUNDS,
    activationConcurrency: ACTIVATION_CONCURRENCY,
    raceConcurrency: RACE_CONCURRENCY,
    roundDelayMs: ROUND_DELAY_MS,
    durationMs: Date.now() - startedAt,
    p50RoundMs: percentile(durations, 0.5),
    p95RoundMs: percentile(durations, 0.95),
    acceptedSwings: ROUNDS,
    conflictedSwings: ROUNDS * (PLAYER_COUNT - 1),
    drops: roundReports.filter((round) => round.drop).length,
    reportedAcceptedSwings: ROUNDS - recoveredUnknownOutcomes,
    recoveredUnknownOutcomes,
    totalPlayerXp: views.reduce((total, view) => total + view.state.playerXp, 0),
    totalPlayerLogs: views.reduce((total, view) => total + view.state.playerLogs, 0),
    server: health,
    roundReports,
  };
  await mkdir(path.dirname(reportPath), { recursive: true });
  await writeFile(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify({ ...report, roundReports: undefined }));
} catch (error) {
  console.error(error);
  for (const player of players.slice(0, 4)) {
    try {
      console.error(`${player.label}: ${JSON.stringify(await player.inspect())}`);
      await saveScreenshot(player.driverUrl, player.sessionId, `soak-${player.label}.png`);
    } catch {}
  }
  process.exitCode = 1;
} finally {
  await Promise.all(players.map(async (player) => {
    try { await player.wd('DELETE', ''); } catch {}
  }));
  await Promise.all(drivers.map(stopProcess));
}
