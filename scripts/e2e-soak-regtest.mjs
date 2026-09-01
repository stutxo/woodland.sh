#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import {
  assertPortAvailable,
  E2E_PROFILE,
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
const CHAOS_CONTROL_URL = (process.env.WOODLAND_SOAK_CHAOS_CONTROL_URL || '').replace(/\/$/, '');
const CHAOS_FAIL_BEFORE_ROUND = setting('WOODLAND_SOAK_CHAOS_FAIL_BEFORE_ROUND', 0, 0, ROUNDS);
const CHAOS_FAIL_AFTER_SUCCESS_ROUND = setting(
  'WOODLAND_SOAK_CHAOS_FAIL_AFTER_SUCCESS_ROUND',
  0,
  0,
  ROUNDS,
);
const CHAOS_ENABLED = CHAOS_FAIL_BEFORE_ROUND > 0 || CHAOS_FAIL_AFTER_SUCCESS_ROUND > 0;
if (CHAOS_FAIL_BEFORE_ROUND > 0 && CHAOS_FAIL_BEFORE_ROUND === CHAOS_FAIL_AFTER_SUCCESS_ROUND) {
  throw new Error('chaos failure rounds must be distinct');
}
if (CHAOS_ENABLED && !CHAOS_CONTROL_URL) {
  throw new Error('WOODLAND_SOAK_CHAOS_CONTROL_URL is required when chaos rounds are enabled');
}
if (CHAOS_ENABLED && TREES_PER_ROUND !== 1) {
  throw new Error('chaos rounds require WOODLAND_SOAK_TREES_PER_ROUND=1');
}
const FORCE_TREE_RENEWAL_ROUND = setting(
  'WOODLAND_SOAK_FORCE_TREE_RENEWAL_ROUND',
  0,
  0,
  ROUNDS,
);
const FORCE_POST_CHOP_RENEWAL_ROUND = setting(
  'WOODLAND_SOAK_FORCE_POST_CHOP_RENEWAL_ROUND',
  0,
  0,
  ROUNDS,
);
if (
  (FORCE_TREE_RENEWAL_ROUND > 0 || FORCE_POST_CHOP_RENEWAL_ROUND > 0)
  && TREES_PER_ROUND !== 1
) {
  throw new Error('forced tree renewal requires one tree per round');
}
const SOAK_VIEWPORT_MAX = 64;
// All fixed LOG and XP supply is issued into the 420 tree-local reserves at
// genesis.
const INDEXED_LOG_SUPPLY = 21_000_000;
const INDEXED_XP_SUPPLY = 21_000_000;
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
async function chaosRequest(route, init = {}) {
  const response = await fetch(`${CHAOS_CONTROL_URL}/${route}`, {
    ...init,
    headers: {
      'content-type': 'application/json',
      ...(init.headers || {}),
    },
    signal: AbortSignal.timeout(10_000),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`chaos control ${route} failed (${response.status}): ${text}`);
  }
  return JSON.parse(text);
}

function configureChaos(mode, remaining = 0) {
  return chaosRequest('config', {
    method: 'POST',
    body: JSON.stringify({ mode, remaining }),
  });
}

function inspectChaos() {
  return chaosRequest('status');
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
  const resume = () => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_RESUME_PENDING()
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
    resume,
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

function playerGameProjection(state) {
  return {
    playerAsset: state.playerAsset,
    playerXp: state.playerXp,
    playerLogs: state.playerLogs,
    playerLuckCredit: state.playerLuckCredit,
    playerLevel: state.playerLevel,
  };
}
function treeRenewalProjection(tree) {
  return {
    treeId: tree.treeId,
    x: tree.x,
    y: tree.y,
    logReserveRemaining: tree.logReserveRemaining,
    xpRemaining: tree.xpRemaining,
    valueSats: tree.valueSats,
    deploymentTxid: tree.deploymentTxid,
    depleted: tree.depleted,
  };
}

function assertTreeRenewal(beforeTree, afterTree, label) {
  assert.notEqual(afterTree.treeOutpoint, beforeTree.treeOutpoint, `${label}: outpoint did not rotate`);
  assert.deepEqual(
    treeRenewalProjection(afterTree),
    treeRenewalProjection(beforeTree),
    `${label}: balances or identity changed`,
  );
  const expectedHealth = beforeTree.health === 0 && beforeTree.logReserveRemaining > 0
    ? 10
    : beforeTree.health;
  assert.equal(afterTree.health, expectedHealth, `${label}: health changed unexpectedly`);
  assert.equal(afterTree.lastAttemptTxid ?? null, null, `${label}: retained a chop transaction`);
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
async function recoverPendingChops(players, label) {
  const initialViews = await Promise.all(players.map((player) => player.inspect()));
  const initialPending = initialViews.filter((view) => view.state.pendingChopTxid).length;
  if (initialPending === 0) {
    return {
      initialPending,
      resumeCalls: 0,
      resumeErrors: 0,
      retries: 0,
    };
  }

  let attempts = 0;
  let resumeCalls = 0;
  let resumeErrors = 0;
  const recovered = await waitFor(
    `${label} pending recovery`,
    async () => {
      attempts += 1;
      const views = await Promise.all(players.map((player) => player.inspect()));
      const pending = players.filter((_, index) => views[index].state.pendingChopTxid);
      if (pending.length === 0) return { resolved: true };
      const outcomes = await mapLimit(pending, RACE_CONCURRENCY, async (player) => {
        try {
          return await player.resume();
        } catch (error) {
          return { error: error instanceof Error ? error.message : String(error) };
        }
      });
      resumeCalls += outcomes.length;
      resumeErrors += outcomes.filter((outcome) => outcome.error).length;
      const after = await Promise.all(players.map((player) => player.inspect()));
      return {
        resolved: after.every((view) => !view.state.pendingChopTxid),
      };
    },
    (value) => value.resolved,
    OPERATION_TIMEOUT_MS,
  );
  assert.equal(recovered.resolved, true, `${label}: pending chops did not recover`);
  return {
    initialPending,
    resumeCalls,
    resumeErrors,
    retries: attempts - 1,
  };
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
function forceTreeRenewal(treeId) {
  const output = execFileSync(
    'cargo',
    [
      'run',
      '--manifest-path',
      path.join(ROOT, 'Cargo.toml'),
      '--locked',
      '--quiet',
      '--features',
      'regtest-e2e',
      '--bin',
      'woodland-operator',
      '--',
      'renew',
      process.env.WOODLAND_WORLD_MANIFEST
        || path.join(ROOT, 'regtest/_build/woodland-world.json'),
      'tree',
      String(treeId),
    ],
    {
      cwd: ROOT,
      env: { ...process.env, WOODLAND_FORCE_ROLLOVER: '1' },
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'inherit'],
    },
  );
  const renewal = JSON.parse(output.trim().split('\n').at(-1));
  assert.equal(renewal.kind, 'tree', 'forced renewal returned the wrong state kind');
  assert.equal(renewal.treeId, treeId, 'forced renewal returned the wrong tree');
  return renewal;
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

function classifyRoundTransitions(round, targetTrees, beforePlayers, views, results) {
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
      ({ view, index }) => view.state.playerStateOutpoint !== beforePlayers[index].outpoint,
    );
  const transitions = targetTreeIds.map((treeId) => {
    const beforeTree = beforeTrees.get(treeId);
    const afterTree = views[0].state.trees.find((tree) => tree.treeId === treeId);
    assert.ok(afterTree, `round ${round}: tree ${treeId} disappeared`);
    assert.notEqual(
      afterTree.treeOutpoint,
      beforeTree.treeOutpoint,
      `round ${round}: tree ${treeId} did not rotate`,
    );
    const treeTxid = afterTree.treeOutpoint.split(':')[0];
    const reportedChops = reportedAccepted
      .map(({ result, index }) => ({
        index,
        result,
        tree: result.state?.trees?.find((tree) => tree.treeId === treeId),
      }))
      .filter(({ index, result, tree }) => {
        if (!tree || !result.state?.playerStateOutpoint) return false;
        const playerTxid = result.state.playerStateOutpoint.split(':')[0];
        const retainedGameState = JSON.stringify(playerGameProjection(views[index].state))
          === JSON.stringify(playerGameProjection(result.state));
        return tree.treeOutpoint.split(':')[0] === playerTxid && retainedGameState;
      });
    const matchingIndexes = new Set([
      ...changedPlayers
        .filter(({ view }) => view.state.playerStateOutpoint.split(':')[0] === treeTxid)
        .map(({ index }) => index),
      ...reportedChops.map(({ index }) => index),
    ]);
    return {
      treeId,
      beforeTree,
      afterTree,
      matchingIndexes,
      reportedChops,
    };
  });
  const renewalOnly = transitions.every(({ matchingIndexes }) => matchingIndexes.size === 0);
  if (renewalOnly) {
    assert.equal(
      reportedAccepted.length,
      0,
      `round ${round}: watcher renewal accompanied an accepted chop report`,
    );
    for (const { treeId, beforeTree, afterTree } of transitions) {
      assertTreeRenewal(
        beforeTree,
        afterTree,
        `round ${round}: watcher renewal of tree ${treeId}`,
      );
    }
    for (const { view, index } of changedPlayers) {
      assert.deepEqual(
        playerGameProjection(view.state),
        beforePlayers[index].game,
        `round ${round}: player ${index + 1} changed game state during watcher renewal`,
      );
    }
    return {
      round,
      retryAfterTreeRenewal: true,
      renewedTreeIds: targetTreeIds,
      renewedPlayers: changedPlayers.map(({ index }) => index),
    };
  }

  assert.ok(
    changedPlayers.length >= targetTrees.length,
    `round ${round}: only ${changedPlayers.length} player transitions for `
      + `${targetTrees.length} trees`,
  );
  const winners = [];
  const renewedTreeIds = [];
  let drops = 0;
  for (const {
    treeId,
    beforeTree,
    afterTree,
    matchingIndexes,
    reportedChops,
  } of transitions) {
    assert.equal(
      matchingIndexes.size,
      1,
      `round ${round}: tree ${treeId} has ${matchingIndexes.size} player transitions`,
    );
    const [winnerIndex] = matchingIndexes;
    assert.equal(
      changedPlayers.some(({ index }) => index === winnerIndex),
      true,
      `round ${round}: tree ${treeId} winner did not retain a player transition`,
    );
    const reportedChop = reportedChops.find(({ index }) => index === winnerIndex);
    const committedTree = reportedChop?.tree ?? afterTree;
    if (committedTree.treeOutpoint !== afterTree.treeOutpoint) {
      assertTreeRenewal(
        committedTree,
        afterTree,
        `round ${round}: post-chop renewal of tree ${treeId}`,
      );
      renewedTreeIds.push(treeId);
    }
    winners.push({ treeId, index: winnerIndex });
    drops += Number(committedTree.health < beforeTree.health);
  }
  const winnerIndexes = new Set(winners.map((winner) => winner.index));
  const renewedPlayers = changedPlayers.filter(({ index }) => !winnerIndexes.has(index));
  for (const { view, index } of renewedPlayers) {
    assert.deepEqual(
      playerGameProjection(view.state),
      beforePlayers[index].game,
      `round ${round}: player ${index + 1} changed game state outside a target-tree transaction`,
    );
  }
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
    retryAfterTreeRenewal: false,
    treeId: targetTreeIds.length === 1 ? targetTreeIds[0] : null,
    treeIds: targetTreeIds,
    winner: winners.length === 1 ? winners[0].index : null,
    winners,
    reportedAccepted: reportedAccepted.length === targetTrees.length,
    reportedAcceptedCount: reportedAccepted.length,
    recoveryMessage: recoveryMessages.length === 1 ? recoveryMessages[0].message : null,
    recoveryMessages,
    renewedPlayers: renewedPlayers.map(({ index }) => index),
    renewedTreeIds,
    drop: drops > 0,
    drops,
    conflicts: PLAYER_COUNT - targetTrees.length,
  };
}

async function reloadPlayersForRound(players, views, round, soakViewport) {
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
    await player.execute(
      `globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(0, 0, arguments[0], arguments[1]);`,
      soakViewport,
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
const chaosEvents = [];
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
  assert.equal(manifest.woodcuttingXpPerLog, 25);
  expectedXpSupply = manifest.xpPerTree
    * manifest.woodcuttingXpPerLog
    * manifest.trees.length;
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

  const soakViewport = [
    Math.min(SOAK_VIEWPORT_MAX, manifest.mapWidth - 1),
    Math.min(SOAK_VIEWPORT_MAX, manifest.mapHeight - 1),
  ];
  await mapLimit(players, ACTIVATION_CONCURRENCY, (player) => player.execute(
    `globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(0, 0, arguments[0], arguments[1]);`,
    soakViewport,
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
  let treeRenewalRetries = 0;
  let forcedPreChopTreeRenewals = 0;
  let forcedPostChopTreeRenewals = 0;
  let playerRenewals = 0;
  for (let round = 1; round <= ROUNDS; round += 1) {
    const selection = selectRoundTargets(views[0].state, stickyTargetTreeId, round);
    const { targetTrees } = selection;
    stickyTargetTreeId = selection.stickyTargetTreeId;
    const targetTreeIds = targetTrees.map((tree) => tree.treeId);
    const assignedTreeIds = players.map(
      (_, index) => targetTreeIds[index % targetTreeIds.length],
    );
    const beforePlayers = views.map((view) => ({
      outpoint: view.state.playerStateOutpoint,
      game: playerGameProjection(view.state),
    }));
    const roundStartedAt = Date.now();
    if (
      FORCE_TREE_RENEWAL_ROUND === round
      && forcedPreChopTreeRenewals === 0
    ) {
      const renewal = forceTreeRenewal(targetTrees[0].treeId);
      assert.equal(
        renewal.oldOutpoint,
        targetTrees[0].treeOutpoint,
        `round ${round}: forced renewal raced an unexpected tree state`,
      );
      forcedPreChopTreeRenewals += 1;
      console.log(`soak round ${round}: forced pre-chop tree ${renewal.treeId} renewal`);
    }
    let chaosKind = null;
    if (CHAOS_FAIL_BEFORE_ROUND === round) {
      chaosKind = 'fail-before';
      await configureChaos(chaosKind, null);
      console.log(`soak round ${round}: emulator outage enabled`);
    } else if (CHAOS_FAIL_AFTER_SUCCESS_ROUND === round) {
      chaosKind = 'fail-after-success';
      await configureChaos(chaosKind, 1);
      console.log(`soak round ${round}: successful emulator response will be masked`);
    }
    let results = await mapLimit(players, RACE_CONCURRENCY, (player, index) => {
      const playerTree = views[index].state.trees.find(
        (tree) => tree.treeId === assignedTreeIds[index],
      );
      return player.race(views[index].state, playerTree);
    });
    if (chaosKind) {
      const duringFault = await inspectChaos();
      assert.equal(duringFault.event.mode, chaosKind, `round ${round}: wrong chaos mode`);
      if (chaosKind === 'fail-before') {
        assert.equal(
          duringFault.event.failedBefore,
          PLAYER_COUNT * 2,
          `round ${round}: persistent outage did not reject both submissions per player`,
        );
        assert.equal(
          results.filter((result) => result.ok).length,
          0,
          `round ${round}: outage unexpectedly reported a winner`,
        );
        const pendingViews = await Promise.all(players.map((player) => player.inspect()));
        for (const [index, view] of pendingViews.entries()) {
          assert.ok(view.state.pendingChopTxid, `round ${round}: player ${index + 1} lost pending chop`);
          assert.equal(
            view.state.playerStateOutpoint,
            beforePlayers[index].outpoint,
            `round ${round}: player ${index + 1} changed during the outage`,
          );
          const pendingTree = view.state.trees.find((tree) => tree.treeId === targetTrees[0].treeId);
          assert.equal(
            pendingTree.treeOutpoint,
            targetTrees[0].treeOutpoint,
            `round ${round}: tree changed during the outage`,
          );
        }
      } else {
        assert.equal(
          duringFault.event.maskedSuccesses,
          1,
          `round ${round}: no successful emulator response was masked`,
        );
      }
      await configureChaos('pass');
      const recovery = await recoverPendingChops(players, `round ${round} chaos`);
      chaosEvents.push({
        round,
        kind: chaosKind,
        fault: duringFault.event,
        recovery,
      });
      console.log(
        `soak round ${round}: chaos recovered ${recovery.initialPending} pending chop(s) `
          + `with ${recovery.resumeCalls} resume call(s)`,
      );
      if (chaosKind === 'fail-before') {
        const postRecovery = await refreshUntilConverged(players, `round ${round} post-recovery`);
        views = postRecovery.views;
        convergenceRetries += postRecovery.retries;
        const recoveredTree = views[0].state.trees.find(
          (tree) => tree.treeId === targetTrees[0].treeId,
        );
        if (recoveredTree.treeOutpoint === targetTrees[0].treeOutpoint) {
          results = await mapLimit(players, RACE_CONCURRENCY, (player, index) => {
            const playerTree = views[index].state.trees.find(
              (tree) => tree.treeId === assignedTreeIds[index],
            );
            return player.race(views[index].state, playerTree);
          });
          console.log(`soak round ${round}: rebuilt stale block-bound attempts`);
        }
      }
    }
    if (
      FORCE_POST_CHOP_RENEWAL_ROUND === round
      && forcedPostChopTreeRenewals === 0
    ) {
      const accepted = results
        .map((result, index) => ({ result, index }))
        .filter(({ result }) => result.ok);
      assert.equal(
        accepted.length,
        1,
        `round ${round}: forced post-chop renewal requires one accepted report`,
      );
      const committedTree = accepted[0].result.state.trees.find(
        (tree) => tree.treeId === targetTrees[0].treeId,
      );
      assert.ok(committedTree, `round ${round}: accepted tree state is missing`);
      const renewal = forceTreeRenewal(targetTrees[0].treeId);
      assert.equal(
        renewal.oldOutpoint,
        committedTree.treeOutpoint,
        `round ${round}: forced post-chop renewal raced an unexpected tree state`,
      );
      forcedPostChopTreeRenewals += 1;
      console.log(`soak round ${round}: forced post-chop tree ${renewal.treeId} renewal`);
    }
    if (ROUND_DELAY_MS) await sleep(ROUND_DELAY_MS);
    const convergence = await refreshUntilConverged(players, `round ${round}`);
    views = convergence.views;
    convergenceRetries += convergence.retries;
    const transition = classifyRoundTransitions(
      round,
      targetTrees,
      beforePlayers,
      views,
      results,
    );
    if (transition.retryAfterTreeRenewal) {
      treeRenewalRetries += transition.renewedTreeIds.length;
      playerRenewals += transition.renewedPlayers.length;
      assert.ok(
        treeRenewalRetries <= ROUNDS * TREES_PER_ROUND,
        'tree renewals prevented sustained chop progress',
      );
      console.warn(
        `soak round ${round}: renewed tree(s) `
          + `${transition.renewedTreeIds.join(',')}; retrying the logical round`,
      );
      round -= 1;
      continue;
    }
    const report = {
      ...transition,
      durationMs: Date.now() - roundStartedAt,
    };
    playerRenewals += report.renewedPlayers.length;
    recoveredUnknownOutcomes += report.recoveryMessages.length;
    roundReports.push(report);

    const reloaded = await reloadPlayersForRound(players, views, round, soakViewport);
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
  assert.equal(health.ready, true, `server unhealthy after soak: ${JSON.stringify(health)}`);
  assert.equal(health.lastError ?? null, null, `server retained an error: ${JSON.stringify(health)}`);
  const [treeMarkers, logs, xp] = await Promise.all([
    fetchAssetSupply(manifest.arkadeServiceUrl, views[0].state.treeAsset),
    fetchAssetSupply(manifest.arkadeServiceUrl, views[0].state.logAsset),
    fetchAssetSupply(manifest.arkadeServiceUrl, views[0].state.xpAsset),
  ]);
  const indexedAssetSupplies = { treeMarkers, logs, xp };
  assert.equal(treeMarkers, views[0].state.trees.length, 'indexed TREE supply changed');
  assert.equal(logs, INDEXED_LOG_SUPPLY, 'indexed LOG supply changed');
  assert.equal(xp, INDEXED_XP_SUPPLY, 'indexed XP supply changed');
  const report = {
    profile: E2E_PROFILE,
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
    chaosFailBeforeRound: CHAOS_FAIL_BEFORE_ROUND,
    chaosFailAfterSuccessRound: CHAOS_FAIL_AFTER_SUCCESS_ROUND,
    chaosEvents,
    forceTreeRenewalRound: FORCE_TREE_RENEWAL_ROUND,
    forcePostChopRenewalRound: FORCE_POST_CHOP_RENEWAL_ROUND,
    durationMs: Date.now() - startedAt,
    p50RoundMs: percentile(durations, 0.5),
    p95RoundMs: percentile(durations, 0.95),
    acceptedSwings: ROUNDS * TREES_PER_ROUND,
    conflictedSwings: ROUNDS * (PLAYER_COUNT - TREES_PER_ROUND),
    drops: roundReports.reduce((total, round) => total + round.drops, 0),
    reportedAcceptedSwings:
      ROUNDS * TREES_PER_ROUND - recoveredUnknownOutcomes,
    recoveredUnknownOutcomes,
    playerRenewals,
    treeRenewalRetries,
    forcedTreeRenewals: forcedPreChopTreeRenewals + forcedPostChopTreeRenewals,
    forcedPreChopTreeRenewals,
    forcedPostChopTreeRenewals,
    postChopTreeRenewals: roundReports.reduce(
      (total, round) => total + round.renewedTreeIds.length,
      0,
    ),
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
