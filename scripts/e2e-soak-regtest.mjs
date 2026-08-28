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
const TREES_PER_ROUND = setting(
  'WOODLAND_SOAK_TREES_PER_ROUND',
  1,
  1,
  PLAYER_COUNT,
);
const RELOAD_EVERY = setting('WOODLAND_SOAK_RELOAD_EVERY', 0, 0, 1_000);
const RELOAD_COUNT = setting(
  'WOODLAND_SOAK_RELOAD_COUNT',
  Math.min(4, PLAYER_COUNT),
  1,
  PLAYER_COUNT,
);
const SOAK_VIEWPORT_MAX = 64;
let expectedLogSupply = 0;
let expectedXpSupply = 0;
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
  const reload = () => wd('POST', '/refresh', {});
  await wd('POST', '/timeouts', { implicit: 0, pageLoad: 30_000, script: OPERATION_TIMEOUT_MS });
  await wd('POST', '/url', { url: `${WEB_URL}/?soak=${encodeURIComponent(label)}` });
  return {
    label,
    driverUrl,
    sessionId,
    wd,
    execute,
    executeAsync,
    inspect,
    refresh,
    race,
    reload,
  };
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
  assert.equal(treeLogs + playerLogs, expectedLogSupply, `${label}: LOG supply changed`);
  assert.equal(treeXp + playerXp, expectedXpSupply, `${label}: XP supply changed`);
}

async function refreshUntilConverged(players, label) {
  let attempts = 0;
  const result = await waitFor(
    `${label} convergence`,
    async () => {
      attempts += 1;
      const views = await mapLimit(players, ACTIVATION_CONCURRENCY, async (player) => {
        const refreshed = await player.refresh();
        if (refreshed.error) throw new Error(refreshed.error);
        return player.inspect();
      });
      try {
        assertConverged(views, label);
        return { views, converged: true, divergence: null };
      } catch (error) {
        return {
          views,
          converged: false,
          divergence: error instanceof Error ? error.message : String(error),
        };
      }
    },
    (value) => value.converged,
    OPERATION_TIMEOUT_MS,
  );
  return { views: result.views, retries: attempts - 1 };
}

function percentile(values, ratio) {
  if (!values.length) return 0;
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * ratio))];
}

async function fetchAssetSupply(baseUrl, assetId) {
  const response = await fetch(
    `${baseUrl.replace(/\/$/, '')}/v1/indexer/asset/${assetId}`,
    { signal: AbortSignal.timeout(20_000) },
  );
  assert.equal(response.ok, true, `asset ${assetId} returned ${response.status}`);
  const details = await response.json();
  assert.equal(details.assetId, assetId, 'asset-details response changed asset ID');
  const supply = Number(details.supply);
  assert.ok(Number.isSafeInteger(supply) && supply >= 0, `asset ${assetId} has invalid supply`);
  return supply;
}

function selectRoundTargets(shared, stickyTargetTreeId, round) {
  if (TREES_PER_ROUND === 1) {
    let target = stickyTargetTreeId == null
      ? null
      : shared.trees.find(
        (tree) => tree.treeId === stickyTargetTreeId
          && tree.health > 0
          && tree.logReserveRemaining > 0,
      );
    if (!target) {
      target = shared.trees.find((tree) => tree.health > 0 && tree.logReserveRemaining > 0);
      assert.ok(target, `round ${round}: no live tree remains`);
    }
    return { targetTrees: [target], stickyTargetTreeId: target.treeId };
  }
  const liveTrees = shared.trees.filter(
    (tree) => tree.x <= SOAK_VIEWPORT_MAX
      && tree.y <= SOAK_VIEWPORT_MAX
      && tree.health > 0
      && tree.logReserveRemaining > 0,
  );
  assert.ok(
    liveTrees.length >= TREES_PER_ROUND,
    `round ${round}: only ${liveTrees.length} live trees for ${TREES_PER_ROUND} groups`,
  );
  const start = ((round - 1) * TREES_PER_ROUND) % liveTrees.length;
  return {
    targetTrees: Array.from(
      { length: TREES_PER_ROUND },
      (_, offset) => liveTrees[(start + offset) % liveTrees.length],
    ),
    stickyTargetTreeId,
  };
}

function classifyRoundTransitions(round, targetTrees, beforePlayerOutpoints, views, results) {
  const targetTreeIds = targetTrees.map((tree) => tree.treeId);
  const beforeTrees = new Map(targetTrees.map((tree) => [tree.treeId, tree]));
  const reportedAccepted = results
    .map((result, index) => ({ result, index }))
    .filter(({ result }) => result.ok);
  assert.ok(
    reportedAccepted.length <= targetTrees.length,
    `round ${round}: too many accepted reports: ${JSON.stringify(results)}`,
  );
  const changedPlayers = views
    .map((view, index) => ({ view, index }))
    .filter(
      ({ view, index }) => view.state.playerStateOutpoint !== beforePlayerOutpoints[index],
    );
  assert.equal(
    changedPlayers.length,
    targetTrees.length,
    `round ${round}: player transition count`,
  );
  const winners = [];
  let drops = 0;
  for (const treeId of targetTreeIds) {
    const beforeTree = beforeTrees.get(treeId);
    const afterTree = views[0].state.trees.find((tree) => tree.treeId === treeId);
    assert.notEqual(
      afterTree.treeOutpoint,
      beforeTree.treeOutpoint,
      `round ${round}: tree ${treeId} did not rotate`,
    );
    const treeTxid = afterTree.treeOutpoint.split(':')[0];
    const matchingPlayers = changedPlayers.filter(
      ({ view }) => view.state.playerStateOutpoint.split(':')[0] === treeTxid,
    );
    assert.equal(
      matchingPlayers.length,
      1,
      `round ${round}: tree ${treeId} has ${matchingPlayers.length} player transitions`,
    );
    winners.push({ treeId, index: matchingPlayers[0].index });
    drops += Number(afterTree.health < beforeTree.health);
  }
  const winnerIndexes = new Set(winners.map((winner) => winner.index));
  for (const reported of reportedAccepted) {
    assert.equal(
      winnerIndexes.has(reported.index),
      true,
      `round ${round}: reported winner ${reported.index + 1} did not commit`,
    );
  }
  const reportedIndexes = new Set(reportedAccepted.map((winner) => winner.index));
  const recoveredWinners = winners.filter((winner) => !reportedIndexes.has(winner.index));
  for (const winner of recoveredWinners) {
    console.warn(
      `soak round ${round}: player ${winner.index + 1} committed tree ${winner.treeId} `
        + `despite a client error: ${results[winner.index].message || 'unknown error'}`,
    );
  }
  const recoveryMessages = recoveredWinners.map((winner) => ({
    treeId: winner.treeId,
    player: winner.index,
    message: results[winner.index].message || 'unknown error',
  }));
  return {
    round,
    treeId: targetTreeIds.length === 1 ? targetTreeIds[0] : null,
    treeIds: targetTreeIds,
    winner: winners.length === 1 ? winners[0].index : null,
    winners,
    reportedAccepted: reportedAccepted.length === targetTrees.length,
    reportedAcceptedCount: reportedAccepted.length,
    recoveryMessage: recoveryMessages.length === 1 ? recoveryMessages[0].message : null,
    recoveryMessages,
    drop: drops > 0,
    drops,
    conflicts: PLAYER_COUNT - targetTrees.length,
  };
}

async function reloadPlayersForRound(players, views, round) {
  if (RELOAD_EVERY === 0 || round % RELOAD_EVERY !== 0) {
    return { views, retries: 0, reloads: 0 };
  }
  const reloadEvent = round / RELOAD_EVERY - 1;
  const start = (reloadEvent * RELOAD_COUNT) % PLAYER_COUNT;
  const reloadIndexes = Array.from(
    { length: RELOAD_COUNT },
    (_, offset) => (start + offset) % PLAYER_COUNT,
  );
  const expectedAssets = new Map(
    reloadIndexes.map((index) => [index, views[index].state.playerAsset]),
  );
  await mapLimit(reloadIndexes, ACTIVATION_CONCURRENCY, async (index) => {
    const player = players[index];
    await player.reload();
    const reloaded = await waitFor(
      `browser reload (${player.label})`,
      player.inspect,
      (value) => value.ready
        && value.state?.playerActive
        && value.serverRegistered
        && value.state.playerAsset === expectedAssets.get(index),
      OPERATION_TIMEOUT_MS,
    );
    assert.equal(reloaded.state.pendingChopTxid ?? null, null, `${player.label}: pending chop`);
  });
  const convergence = await refreshUntilConverged(players, `round ${round} post-reload`);
  return { views: convergence.views, retries: convergence.retries, reloads: reloadIndexes.length };
}

const driverConfigs = Array.from({ length: PLAYER_COUNT }, (_, index) => ({
  port: DRIVER_BASE_PORT + index,
  websocketPort: DRIVER_BASE_PORT + PLAYER_COUNT + index,
}));
const drivers = [];
const players = [];
const roundReports = [];
let recoveredUnknownOutcomes = 0;
let convergenceRetries = 0;
let browserReloads = 0;
const startedAt = Date.now();

try {
  const manifestResponse = await fetch(`${WEB_URL}/world.json`, {
    signal: AbortSignal.timeout(20_000),
  });
  if (!manifestResponse.ok) {
    throw new Error(`world manifest returned ${manifestResponse.status}`);
  }
  const manifest = await manifestResponse.json();
  expectedLogSupply = manifest.logReservePerTree * manifest.trees.length;
  expectedXpSupply = manifest.xpPerTree * manifest.trees.length;
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

  await mapLimit(players, ACTIVATION_CONCURRENCY, (player) => player.execute(
    `globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(0, 0, arguments[0], arguments[1]);`,
    [
      Math.min(SOAK_VIEWPORT_MAX, manifest.mapWidth - 1),
      Math.min(SOAK_VIEWPORT_MAX, manifest.mapHeight - 1),
    ],
  ));

  let views = await mapLimit(players, ACTIVATION_CONCURRENCY, async (player) => {
    const refreshed = await player.refresh();
    assert.equal(refreshed.error, undefined, refreshed.error);
    return player.inspect();
  });
  assert.equal(new Set(views.map((view) => view.state.playerAsset)).size, PLAYER_COUNT);
  assertConverged(views, 'post-activation');
  assert.ok(
    TREES_PER_ROUND <= views[0].state.trees.length,
    `WOODLAND_SOAK_TREES_PER_ROUND exceeds the ${views[0].state.trees.length} world trees`,
  );
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

  let stickyTargetTreeId = null;
  for (let round = 1; round <= ROUNDS; round += 1) {
    const selection = selectRoundTargets(views[0].state, stickyTargetTreeId, round);
    const { targetTrees } = selection;
    stickyTargetTreeId = selection.stickyTargetTreeId;
    const targetTreeIds = targetTrees.map((tree) => tree.treeId);
    const assignedTreeIds = players.map(
      (_, index) => targetTreeIds[index % targetTreeIds.length],
    );
    const beforePlayerOutpoints = views.map((view) => view.state.playerStateOutpoint);
    const roundStartedAt = Date.now();
    const results = await mapLimit(players, RACE_CONCURRENCY, (player, index) => {
      const playerTree = views[index].state.trees.find(
        (tree) => tree.treeId === assignedTreeIds[index],
      );
      return player.race(views[index].state, playerTree);
    });
    if (ROUND_DELAY_MS) await sleep(ROUND_DELAY_MS);
    const convergence = await refreshUntilConverged(players, `round ${round}`);
    views = convergence.views;
    convergenceRetries += convergence.retries;
    const report = {
      ...classifyRoundTransitions(round, targetTrees, beforePlayerOutpoints, views, results),
      durationMs: Date.now() - roundStartedAt,
    };
    recoveredUnknownOutcomes += report.recoveryMessages.length;
    roundReports.push(report);

    const reloaded = await reloadPlayersForRound(players, views, round);
    views = reloaded.views;
    convergenceRetries += reloaded.retries;
    browserReloads += reloaded.reloads;

    if (round % 10 === 0 || round === ROUNDS) {
      console.log(
        `soak round ${round}/${ROUNDS}: trees ${report.treeIds.join(',')}, winners `
          + `${report.winners.map((winner) => winner.index + 1).join(',')}, `
          + `${report.durationMs}ms`,
      );
    }
  }

  const durations = roundReports.map((round) => round.durationMs);
  const health = await fetch(`${WEB_URL}/health.json`).then((response) => response.json());
  const [treeMarkers, logs, xp] = await Promise.all([
    fetchAssetSupply(manifest.arkadeServiceUrl, views[0].state.treeAsset),
    fetchAssetSupply(manifest.arkadeServiceUrl, views[0].state.logAsset),
    fetchAssetSupply(manifest.arkadeServiceUrl, views[0].state.xpAsset),
  ]);
  const indexedAssetSupplies = { treeMarkers, logs, xp };
  assert.equal(treeMarkers, views[0].state.trees.length, 'indexed TREE supply changed');
  assert.equal(logs, expectedLogSupply, 'indexed LOG supply changed');
  assert.equal(xp, expectedXpSupply, 'indexed XP supply changed');
  const report = {
    profile: 'soak',
    webUrl: WEB_URL,
    arkadeServiceUrl: manifest.arkadeServiceUrl,
    emulatorUrl: manifest.emulatorUrl,
    players: PLAYER_COUNT,
    rounds: ROUNDS,
    activationConcurrency: ACTIVATION_CONCURRENCY,
    raceConcurrency: RACE_CONCURRENCY,
    treesPerRound: TREES_PER_ROUND,
    reloadEvery: RELOAD_EVERY,
    reloadCount: RELOAD_COUNT,
    roundDelayMs: ROUND_DELAY_MS,
    durationMs: Date.now() - startedAt,
    p50RoundMs: percentile(durations, 0.5),
    p95RoundMs: percentile(durations, 0.95),
    acceptedSwings: ROUNDS * TREES_PER_ROUND,
    conflictedSwings: ROUNDS * (PLAYER_COUNT - TREES_PER_ROUND),
    drops: roundReports.reduce((total, round) => total + round.drops, 0),
    reportedAcceptedSwings:
      ROUNDS * TREES_PER_ROUND - recoveredUnknownOutcomes,
    recoveredUnknownOutcomes,
    totalPlayerXp: views.reduce((total, view) => total + view.state.playerXp, 0),
    totalPlayerLogs: views.reduce((total, view) => total + view.state.playerLogs, 0),
    indexedAssetSupplies,
    convergenceRetries,
    browserReloads,
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
