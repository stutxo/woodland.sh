#!/usr/bin/env node
// Restock E2E: browser players exhaust one tree until it carries only its
// TREE marker, then the operator restocks it atomically from the supply vault.
// The local wrapper uses a regtest-only five-LOG genesis reserve so this checks
// the same depletion boundary without repeating one covenant transition 1,000
// times. A directly targeted canonical world still exercises all 1,000 drops.
// Assertions pin the depleted lineage, the fresh canonical tree at the same
// coordinate, and the exact 1,000 LOG / 1,000 XP vault drawdown.
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
const MANIFEST = process.env.WOODLAND_WORLD_MANIFEST
  || path.join(ROOT, 'regtest/_build/woodland-world.json');
// One shared tree input permits at most one committed chop per round. Two
// players prove race exclusion without the memory cost of eight Firefox/WASM
// instances; callers may raise either setting for dedicated load testing.
const PLAYER_COUNT = setting('WOODLAND_RESTOCK_PLAYERS', 2, 2, 32);
const ACTIVATION_CONCURRENCY = setting('WOODLAND_RESTOCK_ACTIVATION_CONCURRENCY', 1, 1, 8);
const RACE_CONCURRENCY = setting(
  'WOODLAND_RESTOCK_RACE_CONCURRENCY',
  Math.min(4, PLAYER_COUNT),
  1,
  PLAYER_COUNT,
);
const ROUND_DELAY_MS = setting('WOODLAND_RESTOCK_ROUND_DELAY_MS', 0, 0, 60_000);
const DRIVER_BASE_PORT = setting('WOODLAND_RESTOCK_DRIVER_PORT', 16_500, 1_024, 60_000);
const OPERATION_TIMEOUT_MS = setting('WOODLAND_RESTOCK_TIMEOUT_MS', 600_000, 30_000, 900_000);
const BOOT_TIMEOUT_MS = setting('WOODLAND_RESTOCK_BOOT_TIMEOUT_MS', 120_000, 30_000, 300_000);
const RENEWAL_TIMEOUT_MS = setting('WOODLAND_RESTOCK_RENEWAL_TIMEOUT_MS', 600_000, 60_000, 900_000);
const MAX_ROUNDS = setting('WOODLAND_RESTOCK_MAX_ROUNDS', 12_000, 1_000, 100_000);
const VIEWPORT_MAX = 64;
const LOG_RESERVE_PER_TREE = 1_000;
const INITIAL_TREE_RESERVE = setting(
  'WOODLAND_E2E_INITIAL_TREE_RESERVE',
  LOG_RESERVE_PER_TREE,
  5,
  LOG_RESERVE_PER_TREE,
);
const INDEXED_LOG_SUPPLY = 21_000_000;
const INDEXED_XP_SUPPLY = 21_000_000;
const DEPLOYER_SECRET = process.env.WOODLAND_DEPLOYER_SECRET
  || '1111111111111111111111111111111111111111111111111111111111111111';
const ROLLOVER_SECRET = process.env.WOODLAND_ROLLOVER_SECRET
  || '4444444444444444444444444444444444444444444444444444444444444444';
const reportPath = path.resolve(
  ROOT,
  process.env.WOODLAND_RESTOCK_REPORT || 'regtest/_build/restock-report.json',
);
const FUND_COMMAND = process.env.WOODLAND_RESTOCK_FUND_COMMAND;
let expectedLogSupply = 0;
let expectedXpSupply = 0;
const startedAt = Date.now();

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
  // A page whose main thread is blocked by a long WASM sync makes the driver
  // answer past undici's fixed 300s headers timeout; retry those transport
  // failures for calls that submit no transaction (reads and refreshes).
  const isTransportFailure = (error) => String(error?.cause?.code || '').startsWith('UND_ERR')
    || /fetch failed|HeadersTimeoutError|AbortError|TimeoutError/.test(String(error));
  const wdRead = async (method, suffix, body, timeout = OPERATION_TIMEOUT_MS) => {
    for (let attempt = 0; ; attempt += 1) {
      try {
        return await wd(method, suffix, body, timeout);
      } catch (error) {
        if (attempt >= 6 || !isTransportFailure(error)) throw error;
        await sleep(2_000 * (attempt + 1));
      }
    }
  };
  const execute = (script, args = []) => wd('POST', '/execute/sync', { script, args });
  const executeRead = (script, args = []) => wdRead('POST', '/execute/sync', { script, args });
  const executeAsync = (script, args = []) => wd('POST', '/execute/async', { script, args });
  const executeAsyncRead = (script, args = []) => wdRead('POST', '/execute/async', { script, args });
  const inspect = () => executeRead(`
    return {
      ready: Boolean(globalThis.__WOODLAND_E2E_READY),
      error: globalThis.__WOODLAND_E2E_ERROR || '',
      state: globalThis.__WOODLAND_E2E_STATE || null,
      log: document.getElementById('log')?.textContent || '',
    };
  `);
  const refresh = () => executeAsyncRead(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_REFRESH()
      .then((state) => done({ state }))
      .catch((error) => done({ error: String(error) }));
  `);
  const refreshWorld = () => executeAsyncRead(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_REFRESH_WORLD()
      .then((state) => done({ state }))
      .catch((error) => done({ error: String(error) }));
  `);
  // Chops are retried on transport failure: a resubmission races the same
  // outpoint, so at most one lands and the loser is rejected by the covenant.
  const race = (snapshot, tree) => executeAsyncRead(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_CHOP_EXPECTED(...Array.from(arguments).slice(0, -1)).then(done);
  `, [tree.treeId, tree.treeOutpoint, snapshot.playerStateOutpoint, tree.nextDrop]);
  await wd('POST', '/timeouts', { implicit: 0, pageLoad: 30_000, script: OPERATION_TIMEOUT_MS });
  await wd('POST', '/url', { url: `${WEB_URL}/?restock=${encodeURIComponent(label)}` });
  return {
    label,
    driverUrl,
    sessionId,
    wd,
    execute,
    executeAsync,
    inspect,
    refresh,
    refreshWorld,
    race,
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
  const treeXp = views[0].state.seasonXpRemaining;
  assert.ok(Number.isInteger(treeXp) && treeXp >= 0, `${label}: invalid on-tree XP supply`);
  // This profile has no withdrawal and every chop/restock moves LOG and XP
  // together, so the verified global on-tree XP balance is also the LOG
  // balance. Unvisited map tiles intentionally retain manifest placeholders.
  const treeLogs = treeXp;
  const playerLogs = views.reduce((total, view) => total + view.state.playerLogs, 0);
  const playerXp = views.reduce((total, view) => total + view.state.playerXp, 0);
  assert.equal(treeLogs + playerLogs, expectedLogSupply, `${label}: LOG supply changed`);
  assert.equal(treeXp + playerXp, expectedXpSupply, `${label}: XP supply changed`);
}

async function refreshAll(players) {
  return mapLimit(players, ACTIVATION_CONCURRENCY, async (player) => {
    const refreshed = await player.refreshWorld();
    if (refreshed.error) throw new Error(refreshed.error);
    return player.inspect();
  });
}

async function refreshUntilConverged(players, label) {
  const result = await waitFor(
    `${label} convergence`,
    async () => {
      const views = await refreshAll(players);
      try {
        assertConverged(views, label);
        return { views, converged: true };
      } catch (error) {
        return { converged: false, reason: String(error), views };
      }
    },
    (value) => value.converged,
    OPERATION_TIMEOUT_MS,
  );
  return result.views;
}

// A stump blocks the whole swarm (health 0 cannot be chopped); the watcher's
// ~60s renewal tick refills it without changing any player's luck state.
async function waitForRenewal(players, treeId, reserves, label) {
  const deadline = Date.now() + RENEWAL_TIMEOUT_MS;
  for (;;) {
    await sleep(5_000);
    const views = await refreshAll(players);
    const tree = views[0].state?.trees?.find((candidate) => candidate.treeId === treeId);
    if (tree?.health === 5) {
      assert.equal(tree.logReserveRemaining, reserves, `${label}: renewal changed LOG reserve`);
      assert.equal(tree.xpRemaining, reserves, `${label}: renewal changed XP reserve`);
      assertConverged(views, label);
      return { views, tree };
    }
    assert.ok(
      Date.now() < deadline,
      `${label}: tree ${treeId} stump was not renewed in time`,
    );
  }
}

async function indexerVtxos(baseUrl, params) {
  const records = [];
  const visited = new Set();
  let pageIndex = 1;
  while (!visited.has(pageIndex)) {
    visited.add(pageIndex);
    const query = new URLSearchParams({
      ...params,
      'page.size': '500',
      'page.index': String(pageIndex),
    });
    // The arkd indexer can stall behind world bootstrap indexing on a fresh
    // stack; bound each attempt and retry instead of dying on one slow page.
    let response;
    for (let attempt = 0; ; attempt += 1) {
      try {
        response = await fetch(`${baseUrl}/v1/indexer/vtxos?${query}`, {
          signal: AbortSignal.timeout(60_000),
        });
        break;
      } catch (error) {
        if (attempt >= 4) throw error;
        await sleep(5_000);
      }
    }
    if (!response.ok) throw new Error(`indexer query failed: ${await response.text()}`);
    const payload = await response.json();
    records.push(...(payload.vtxos || []));
    const next = Number(payload.page?.next || 0);
    if (next <= pageIndex) break;
    pageIndex = next;
  }
  return records;
}

async function fetchAssetSupply(baseUrl, assetId) {
  const response = await fetch(
    `${baseUrl.replace(/\/$/, '')}/v1/indexer/asset/${assetId}`,
    { signal: AbortSignal.timeout(20_000) },
  );
  assert.equal(response.ok, true, `asset ${assetId} returned ${response.status}`);
  const details = await response.json();
  const supply = Number(details.supply);
  assert.ok(Number.isSafeInteger(supply) && supply >= 0, `asset ${assetId} has invalid supply`);
  return supply;
}


function restockTree(manifest, treeId) {
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
      'restock',
      MANIFEST,
      'tree',
      String(treeId),
    ],
    {
      cwd: ROOT,
      env: {
        ...process.env,
        WOODLAND_NETWORK: 'regtest',
        WOODLAND_ARKADE_SERVICE_URL: manifest.arkadeServiceUrl,
        WOODLAND_EMULATOR_URL: manifest.emulatorUrl,
        WOODLAND_DEPLOYER_SECRET: DEPLOYER_SECRET,
        WOODLAND_ROLLOVER_SECRET: ROLLOVER_SECRET,
      },
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'inherit'],
    },
  );
  return JSON.parse(output.trim().split('\n').at(-1));
}

const driverConfigs = Array.from({ length: PLAYER_COUNT }, (_, index) => ({
  port: DRIVER_BASE_PORT + index,
  websocketPort: DRIVER_BASE_PORT + PLAYER_COUNT + index,
}));
const drivers = [];
const players = [];

try {
  const manifestResponse = await fetch(`${WEB_URL}/world.json`, {
    signal: AbortSignal.timeout(20_000),
  });
  if (!manifestResponse.ok) {
    throw new Error(`world manifest returned ${manifestResponse.status}`);
  }
  const manifest = await manifestResponse.json();
  assert.equal(manifest.schemaVersion, 1, 'world manifest must be schema 1');
  assert.equal(manifest.protocolVersion, 1, 'world manifest must declare protocol v1');
  assert.equal(manifest.baseLogDropBasisPoints, 2_000);
  assert.equal(manifest.levelLogDropBonusBasisPoints, 200);
  assert.equal(manifest.maxLevelLogDropBasisPoints, 3_000);
  assert.equal(manifest.luckWindowBasisPoints, 10_000);
  assert.equal(manifest.initialLuckCredit, 8_000);
  assert.ok(manifest.vaultScript, 'world manifest must pin the supply vault');
  assert.equal(manifest.logReservePerTree, LOG_RESERVE_PER_TREE);
  assert.equal(manifest.xpPerTree, LOG_RESERVE_PER_TREE);
  expectedLogSupply = INITIAL_TREE_RESERVE * manifest.trees.length;
  expectedXpSupply = INITIAL_TREE_RESERVE * manifest.trees.length;
  const arkadeBase = manifest.arkadeServiceUrl.replace(/\/$/, '');
  const arkadeHost = new URL(manifest.arkadeServiceUrl).hostname;
  const localFunding = ['127.0.0.1', 'localhost'].includes(arkadeHost);
  if (!localFunding && !FUND_COMMAND) {
    throw new Error(
      'WOODLAND_RESTOCK_FUND_COMMAND is required for a remote world; it receives <address> <sats>',
    );
  }

  await Promise.all([
    waitForHttp(`${WEB_URL}/health.json`, 20_000),
    waitForHttp(`${arkadeBase}/v1/info`, 20_000),
    waitForHttp(`${manifest.emulatorUrl.replace(/\/$/, '')}/v1/info`, 20_000),
    ...driverConfigs.flatMap(({ port, websocketPort }, index) => [
      assertPortAvailable(port, `restock WebDriver ${index + 1}`),
      assertPortAvailable(websocketPort, `restock WebDriver ${index + 1} WebSocket`),
    ]),
  ]);

  for (const [index, config] of driverConfigs.entries()) {
    const driverUrl = `http://127.0.0.1:${config.port}`;
    drivers.push(await startGeckodriver(
      ['--port', String(config.port), '--websocket-port', String(config.websocketPort)],
      ROOT,
      `${driverUrl}/status`,
      `restock WebDriver ${index + 1}`,
    ));
  }
  await mapLimit(driverConfigs, ACTIVATION_CONCURRENCY, async ({ port }, index) => {
    const player = await createPlayer(
      `http://127.0.0.1:${port}`,
      `player-${index + 1}`,
    );
    players[index] = player;
    const waitForWallet = (suffix = '') => waitFor(
      `wallet initialization${suffix} (${player.label})`,
      player.inspect,
      (value) => value.ready
        && !value.state?.playerActive
        && value.state?.fundingRequiredSats === 330,
      BOOT_TIMEOUT_MS,
    );
    try {
      player.initial = await waitForWallet();
    } catch (error) {
      console.warn(`${player.label} did not boot; reloading once: ${error}`);
      await player.wd('POST', '/url', {
        url: `${WEB_URL}/?restock=${encodeURIComponent(player.label)}&reload=1`,
      });
      player.initial = await waitForWallet(' after reload');
    }
  });
  const initial = players.map((player) => player.initial);
  for (const view of initial) {
    // The seeded ark send can stall under load; bound each attempt and retry
    // rather than let one slow send sink the stage.
    const fundArgs = FUND_COMMAND
      ? [view.state.address, String(view.state.fundingRequiredSats)]
      : ['fund', view.state.address, String(view.state.fundingRequiredSats)];
    let funded = false;
    for (let attempt = 0; attempt < 3 && !funded; attempt += 1) {
      try {
        execFileSync(
          FUND_COMMAND || path.join(ROOT, 'scripts/regtest.sh'),
          fundArgs,
          { cwd: ROOT, stdio: 'pipe', encoding: 'utf8', timeout: 60_000 },
        );
        funded = true;
      } catch (error) {
        if (attempt === 2) throw error;
      }
    }
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
      `activation (${player.label})`,
      player.inspect,
      (value) => value.state?.playerActive && Boolean(value.state.playerAsset),
      OPERATION_TIMEOUT_MS,
    );
  });

  await mapLimit(players, ACTIVATION_CONCURRENCY, (player) => player.execute(
    `globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(0, 0, arguments[0], arguments[1]);`,
    [
      Math.min(VIEWPORT_MAX, manifest.mapWidth - 1),
      Math.min(VIEWPORT_MAX, manifest.mapHeight - 1),
    ],
  ));

  let views = await refreshUntilConverged(players, 'post-activation');
  const candidates = views[0].state.trees
    .filter((tree) => (
      tree.x <= Math.min(VIEWPORT_MAX, manifest.mapWidth - 1)
      && tree.y <= Math.min(VIEWPORT_MAX, manifest.mapHeight - 1)
      && tree.health === 5
      && tree.logReserveRemaining === INITIAL_TREE_RESERVE
      && tree.depleted === false
    ))
    .sort((left, right) => left.treeId - right.treeId);
  assert.ok(candidates.length > 0, 'no full-reserve tree is available inside the viewport');
  let target = candidates[0];
  console.log(
    `restock target: tree ${target.treeId} at ${target.x}:${target.y} `
      + `(${PLAYER_COUNT} players, ${INITIAL_TREE_RESERVE} initial LOG, ~5 swings per LOG)`,
  );

  const vaultBefore = await indexerVtxos(arkadeBase, {
    scripts: manifest.vaultScript,
    spendableOnly: 'true',
  });
  assert.equal(vaultBefore.length, 1, 'supply vault must be one live VTXO');
  assert.equal(Number(vaultBefore[0].amount), 330, 'supply vault holds the dust value');
  const vaultHoldingsBefore = new Map(
    vaultBefore[0].assets.map((asset) => [asset.assetId, Number(asset.amount)]),
  );
  assert.equal(vaultHoldingsBefore.size, 2, 'supply vault must carry only LOG and XP');

  let rounds = 0;
  let drops = 0;
  let stumpWaits = 0;
  let recoveredUnknownOutcomes = 0;
  while (target.logReserveRemaining > 0) {
    if (target.health === 0) {
      stumpWaits += 1;
      const renewed = await waitForRenewal(
        players,
        target.treeId,
        target.logReserveRemaining,
        `stump renewal after ${drops} drops`,
      );
      views = renewed.views;
      target = renewed.tree;
      continue;
    }
    rounds += 1;
    assert.ok(
      rounds <= MAX_ROUNDS,
      `depleting tree ${target.treeId} exceeded ${MAX_ROUNDS} rounds`,
    );
    assert.ok(
      views.every((view) => (
        view.state.trees.find((tree) => tree.treeId === target.treeId)?.treeOutpoint
          === target.treeOutpoint
      )),
      `round ${rounds}: players disagree on the shared tree lineage`,
    );
    const beforePlayerOutpoints = views.map((view) => view.state.playerStateOutpoint);
    const predictedDrops = views.map((view) => (
      view.state.trees.find((tree) => tree.treeId === target.treeId).nextDrop
    ));
    const results = await mapLimit(players, RACE_CONCURRENCY, (player, index) => (
      player.race(
        views[index].state,
        views[index].state.trees.find((tree) => tree.treeId === target.treeId),
      )
    ));
    if (ROUND_DELAY_MS) await sleep(ROUND_DELAY_MS);
    views = await refreshUntilConverged(players, `round ${rounds}`);
    const after = views[0].state.trees.find((tree) => tree.treeId === target.treeId);
    assert.notEqual(
      after.treeOutpoint,
      target.treeOutpoint,
      `round ${rounds}: target tree did not rotate`,
    );
    const winnerIndex = views.findIndex((view, index) => (
      view.state.playerStateOutpoint !== beforePlayerOutpoints[index]
    ));
    assert.notEqual(winnerIndex, -1, `round ${rounds}: no committed player transition`);
    const dropped = target.logReserveRemaining - after.logReserveRemaining;
    assert.ok(dropped === 0 || dropped === 1, `round ${rounds}: LOG reserve jumped`);
    assert.equal(after.xpRemaining, target.xpRemaining - dropped, `round ${rounds}: XP delta`);
    assert.equal(
      dropped === 1,
      predictedDrops[winnerIndex],
      `round ${rounds}: winning player's prediction must be exact`,
    );
    // A drop that empties health is followed by the watcher's stump refill
    // (0 -> full) and may land before this round's snapshot: accept the
    // deplete-then-refill sequence as one valid round outcome.
    const refillAfterDeplete = dropped === 1
      && target.health === 1
      && after.health === manifest.activeLogsPerTree;
    if (!dropped) {
      assert.equal(after.health, target.health, `round ${rounds}: a miss changed tree health`);
    } else if (refillAfterDeplete) {
      drops += 1;
    } else {
      assert.equal(after.health, target.health - 1, `round ${rounds}: drop health delta`);
      drops += 1;
    }
    assert.ok(
      results.filter((result) => result.ok).length <= 1,
      `round ${rounds}: too many accepted reports: ${JSON.stringify(results)}`,
    );
    const reportedIndex = results.findIndex((result) => result.ok);
    assert.ok(
      reportedIndex === -1 || reportedIndex === winnerIndex,
      `round ${rounds}: reported winner ${reportedIndex + 1} did not commit`,
    );
    if (reportedIndex === -1) {
      recoveredUnknownOutcomes += 1;
      console.warn(
        `restock round ${rounds}: the winner committed despite a client error; recovered`,
      );
    }
    target = after;
    if (rounds % 100 === 0) {
      console.log(
        `restock round ${rounds}: tree ${target.treeId} LOG reserve ${target.logReserveRemaining}`,
      );
    }
  }
  assert.equal(drops, INITIAL_TREE_RESERVE, 'every initial reserve LOG must drop exactly once');
  assert.equal(target.xpRemaining, 0, 'LOG and XP deplete together');
  assert.equal(target.depleted, true, 'depleted tree must be flagged');
  assertConverged(views, 'depleted tree');

  const depletedRecord = await indexerVtxos(arkadeBase, { outpoints: target.treeOutpoint });
  assert.equal(depletedRecord.length, 1, 'depleted tree outpoint must be indexed');
  assert.equal(depletedRecord[0].isSpent, false, 'depleted tree must still be live');
  assert.equal(Number(depletedRecord[0].amount), 330, 'depleted tree keeps the dust value');
  assert.equal(
    depletedRecord[0].script,
    manifest.treeScript,
    'depleted tree keeps the world contract',
  );
  assert.equal(depletedRecord[0].assets.length, 1, 'depleted tree carries only its TREE marker');
  assert.equal(depletedRecord[0].assets[0].assetId, manifest.treeAsset);
  assert.equal(Number(depletedRecord[0].assets[0].amount), 1);

  const standingComparison = views[0].state.trees.find((tree) => (
    tree.treeId !== target.treeId
    && tree.health > 0
    && tree.logReserveRemaining > 0
    && tree.xpRemaining > 0
  ));
  assert.ok(standingComparison, 'no standing tree exposes the player next outcome');
  assert.equal(
    standingComparison.nextRollBucket,
    target.nextRollBucket,
    'depletion must not make player-bound entropy tree-specific',
  );
  const nextDropBeforeRestock = standingComparison.nextDrop;

  const restock = restockTree(manifest, target.treeId);
  assert.equal(restock.restocked, 1, 'operator restock must report exactly one tree');

  const oldLineage = await indexerVtxos(arkadeBase, { outpoints: target.treeOutpoint });
  assert.equal(oldLineage.length, 1, 'old tree outpoint must be indexed');
  assert.equal(oldLineage[0].isSpent, true, 'old depleted lineage must be spent');
  expectedLogSupply += LOG_RESERVE_PER_TREE;
  expectedXpSupply += LOG_RESERVE_PER_TREE;

  views = await refreshUntilConverged(players, 'post-restock');
  const fresh = views[0].state.trees.find((tree) => tree.treeId === target.treeId);
  assert.ok(fresh, 'restocked tree is missing from the world view');
  assert.equal(fresh.x, target.x, 'restock must keep the coordinate');
  assert.equal(fresh.y, target.y, 'restock must keep the coordinate');
  assert.equal(fresh.health, 5, 'restocked tree must be at full health');
  assert.equal(fresh.logReserveRemaining, LOG_RESERVE_PER_TREE);
  assert.equal(fresh.xpRemaining, LOG_RESERVE_PER_TREE);
  assert.equal(fresh.depleted, false, 'restocked tree must not be flagged depleted');
  assert.equal(fresh.valueSats, 330, 'restocked tree keeps the dust value');
  assert.notEqual(fresh.treeOutpoint, target.treeOutpoint, 'restock must rotate the outpoint');
  assert.equal(
    fresh.nextRollBucket,
    target.nextRollBucket,
    'restocking a tree must not change player-bound reward entropy',
  );
  assert.equal(
    fresh.nextDrop,
    nextDropBeforeRestock,
    'restocking a tree must preserve the player-bound next outcome',
  );

  const freshRecords = await indexerVtxos(arkadeBase, { outpoints: fresh.treeOutpoint });
  assert.equal(freshRecords.length, 1, 'restocked tree must be indexed exactly once');
  assert.equal(freshRecords[0].isSpent, false, 'restocked tree must be live');
  assert.equal(Number(freshRecords[0].amount), 330, 'restocked tree keeps the dust value');
  assert.equal(freshRecords[0].script, manifest.treeScript, 'restock keeps the world contract');
  const freshHoldings = new Map(
    freshRecords[0].assets.map((asset) => [asset.assetId, Number(asset.amount)]),
  );
  assert.equal(freshHoldings.get(manifest.treeAsset), 1, 'restocked tree marker');
  assert.equal(freshHoldings.get(manifest.logAsset), LOG_RESERVE_PER_TREE);
  assert.equal(freshHoldings.get(manifest.xpAsset), LOG_RESERVE_PER_TREE);

  const vaultAfter = await indexerVtxos(arkadeBase, {
    scripts: manifest.vaultScript,
    spendableOnly: 'true',
  });
  assert.equal(vaultAfter.length, 1, 'supply vault must remain one live VTXO');
  assert.equal(Number(vaultAfter[0].amount), 330, 'restock must not move vault sats');
  assert.equal(vaultAfter[0].script, manifest.vaultScript, 'restock keeps the vault contract');
  const vaultHoldingsAfter = new Map(
    vaultAfter[0].assets.map((asset) => [asset.assetId, Number(asset.amount)]),
  );
  assert.equal(vaultHoldingsAfter.size, 2, 'supply vault must carry only LOG and XP');
  assert.equal(
    vaultHoldingsAfter.get(manifest.logAsset),
    vaultHoldingsBefore.get(manifest.logAsset) - LOG_RESERVE_PER_TREE,
    'restock must draw exactly one tree reserve of LOG from the vault',
  );
  assert.equal(
    vaultHoldingsAfter.get(manifest.xpAsset),
    vaultHoldingsBefore.get(manifest.xpAsset) - LOG_RESERVE_PER_TREE,
    'restock must draw exactly one tree reserve of XP from the vault',
  );

  const [logs, xp] = await Promise.all([
    fetchAssetSupply(arkadeBase, views[0].state.logAsset),
    fetchAssetSupply(arkadeBase, views[0].state.xpAsset),
  ]);
  assert.equal(logs, INDEXED_LOG_SUPPLY, 'indexed LOG supply changed');
  assert.equal(xp, INDEXED_XP_SUPPLY, 'indexed XP supply changed');

  const report = {
    profile: 'restock',
    webUrl: WEB_URL,
    arkadeServiceUrl: manifest.arkadeServiceUrl,
    players: PLAYER_COUNT,
    initialTreeReserve: INITIAL_TREE_RESERVE,
    treeId: target.treeId,
    coordinate: { x: target.x, y: target.y },
    rounds,
    drops,
    stumpWaits,
    recoveredUnknownOutcomes,
    durationMs: Date.now() - startedAt,
    vault: {
      before: Object.fromEntries(vaultHoldingsBefore),
      after: Object.fromEntries(vaultHoldingsAfter),
    },
    indexedAssetSupplies: { logs, xp },
  };
  await mkdir(path.dirname(reportPath), { recursive: true });
  await writeFile(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report));
} catch (error) {
  console.error(error);
  for (const player of players.slice(0, 4)) {
    try {
      console.error(`${player.label}: ${JSON.stringify(await player.inspect())}`);
      await saveScreenshot(player.driverUrl, player.sessionId, `restock-${player.label}.png`);
    } catch {}
  }
  for (const [index, driver] of drivers.entries()) {
    if (driver.output()) {
      console.error(`restock WebDriver ${index + 1} output:\n${driver.output()}`);
    }
  }
  process.exitCode = 1;
} finally {
  await Promise.all(players.map(async (player) => {
    try { await player.wd('DELETE', ''); } catch {}
  }));
  await Promise.all(drivers.map(stopProcess));
}
