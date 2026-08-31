#!/usr/bin/env node
// Regrowth E2E: ten successful drops turn a pristine tree into a funded
// stump. An unfunded, inactive browser is rejected before two Bitcoin tip
// advances, then permissionlessly regrows the exact same local reserve.
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
const DRIVER_BASE_PORT = setting('WOODLAND_REGROWTH_DRIVER_PORT', 16_500, 1_024, 60_000);
const OPERATION_TIMEOUT_MS = setting(
  'WOODLAND_REGROWTH_TIMEOUT_MS',
  600_000,
  30_000,
  900_000,
);
const BOOT_TIMEOUT_MS = setting('WOODLAND_REGROWTH_BOOT_TIMEOUT_MS', 120_000, 30_000, 300_000);
const MAX_ROUNDS = setting('WOODLAND_REGROWTH_MAX_ROUNDS', 500, 20, 10_000);
const VIEWPORT_MAX = 64;
const TREE_COUNT = 420;
const ACTIVE_LOGS_PER_TREE = 10;
const LOG_RESERVE_PER_TREE = 50_000;
const INDEXED_LOG_SUPPLY = 21_000_000;
const INDEXED_XP_SUPPLY = 21_000_000;
const FUND_COMMAND = process.env.WOODLAND_REGROWTH_FUND_COMMAND;
const reportPath = path.resolve(
  ROOT,
  process.env.WOODLAND_REGROWTH_REPORT || 'regtest/_build/regrowth-report.json',
);
const startedAt = Date.now();

function setting(name, fallback, minimum, maximum) {
  const value = Number(process.env[name] || fallback);
  if (!Number.isInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be an integer from ${minimum} to ${maximum}`);
  }
  return value;
}

async function createBrowser(driverUrl, label) {
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
      adjacentTree: globalThis.__WOODLAND_E2E_ADJACENT_TREE || null,
      busy: Boolean(document.getElementById('refresh')?.disabled),
      log: document.getElementById('log')?.textContent || '',
    };
  `);
  const refreshWorld = () => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_REFRESH_WORLD()
      .then((state) => done({ state }))
      .catch((error) => done({ failure: String(error) }));
  `);
  const chop = (snapshot, tree) => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_CHOP_EXPECTED(...Array.from(arguments).slice(0, -1))
      .then(done)
      .catch((error) => done({ ok: false, message: String(error) }));
  `, [tree.treeId, tree.treeOutpoint, snapshot.playerStateOutpoint, tree.nextDrop]);
  const regrow = (treeId) => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_REGROW(arguments[0])
      .then((state) => done({ state }))
      .catch((error) => done({ message: String(error) }));
  `, [treeId]);
  const clickMapCell = (x, y) => execute(`
    if (!globalThis.__WOODLAND_E2E_CLICK_MAP) throw new Error('missing canvas map hook');
    globalThis.__WOODLAND_E2E_CLICK_MAP(arguments[0], arguments[1]);
  `, [x, y]);
  await wd('POST', '/timeouts', { implicit: 0, pageLoad: 30_000, script: OPERATION_TIMEOUT_MS });
  await wd('POST', '/url', { url: `${WEB_URL}/?regrowth=${encodeURIComponent(label)}` });
  return {
    label,
    driverUrl,
    sessionId,
    wd,
    execute,
    inspect,
    refreshWorld,
    chop,
    clickMapCell,
    regrow,
  };
}

function treeProjection(state) {
  return state.trees.map((tree) => ({
    treeId: tree.treeId,
    health: tree.health,
    stumpHeight: tree.stumpHeight,
    regrowAtHeight: tree.regrowAtHeight,
    logReserveRemaining: tree.logReserveRemaining,
    xpRemaining: tree.xpRemaining,
    treeOutpoint: tree.treeOutpoint,
    depleted: tree.depleted,
  }));
}

async function refreshBoth(chopper, regrower, label) {
  const [left, right] = await Promise.all([chopper.refreshWorld(), regrower.refreshWorld()]);
  if (left.failure) throw new Error(`${label}: chopper refresh: ${left.failure}`);
  if (right.failure) throw new Error(`${label}: regrower refresh: ${right.failure}`);
  assert.deepEqual(treeProjection(left.state), treeProjection(right.state), `${label}: tree divergence`);
  return [left.state, right.state];
}

async function indexerVtxos(baseUrl, params) {
  const query = new URLSearchParams({
    ...params,
    'page.size': '500',
    'page.index': '1',
  });
  const response = await fetch(`${baseUrl}/v1/indexer/vtxos?${query}`, {
    signal: AbortSignal.timeout(60_000),
  });
  if (!response.ok) throw new Error(`indexer query failed: ${await response.text()}`);
  return (await response.json()).vtxos || [];
}

async function fetchAssetSupply(baseUrl, assetId) {
  const response = await fetch(`${baseUrl}/v1/indexer/asset/${assetId}`, {
    signal: AbortSignal.timeout(20_000),
  });
  assert.equal(response.ok, true, `asset ${assetId} returned ${response.status}`);
  const supply = Number((await response.json()).supply);
  assert.ok(Number.isSafeInteger(supply) && supply >= 0, `asset ${assetId} has invalid supply`);
  return supply;
}

async function blockTip(emulatorUrl) {
  const response = await fetch(`${emulatorUrl.replace(/\/$/, '')}/v1/block-tip`, {
    cache: 'no-store',
    signal: AbortSignal.timeout(20_000),
  });
  assert.equal(response.ok, true, `block tip returned ${response.status}`);
  const tip = await response.json();
  assert.ok(Number.isInteger(tip.height) && tip.height > 0, 'gate returned an invalid block height');
  assert.match(tip.blockHash, /^[0-9a-f]{64}$/i, 'gate returned an invalid block hash');
  return tip;
}

function mine(blocks) {
  execFileSync(path.join(ROOT, 'scripts/regtest.sh'), ['mine', String(blocks)], {
    cwd: ROOT,
    stdio: 'pipe',
    encoding: 'utf8',
    timeout: 120_000,
  });
}

async function waitForIndexedRecord(baseUrl, outpoint, spent) {
  return waitFor(
    `${spent ? 'spent' : 'live'} tree ${outpoint}`,
    async () => (await indexerVtxos(baseUrl, { outpoints: outpoint }))[0] || null,
    (record) => record?.isSpent === spent,
    OPERATION_TIMEOUT_MS,
  );
}

const driverConfigs = [0, 1].map((index) => ({
  port: DRIVER_BASE_PORT + index,
  websocketPort: DRIVER_BASE_PORT + 2 + index,
}));
const drivers = [];
const browsers = [];

try {
  const manifestResponse = await fetch(`${WEB_URL}/world.json`, {
    signal: AbortSignal.timeout(20_000),
  });
  assert.equal(manifestResponse.ok, true, `world manifest returned ${manifestResponse.status}`);
  const manifest = await manifestResponse.json();
  assert.equal(manifest.schemaVersion, 2, 'world manifest must be schema 2');
  assert.equal(manifest.protocolVersion, 2, 'world manifest must declare protocol v2');
  assert.equal(manifest.trees.length, TREE_COUNT, 'world must contain exactly 420 trees');
  assert.equal(manifest.activeLogsPerTree, ACTIVE_LOGS_PER_TREE);
  assert.equal(manifest.logReservePerTree, LOG_RESERVE_PER_TREE);
  assert.equal(manifest.xpPerTree, LOG_RESERVE_PER_TREE);
  for (const removed of [
    'vaultScript',
    'vaultRestockArkadeScript',
    'vaultRenewalArkadeScript',
    'treeRetireArkadeScript',
  ]) {
    assert.equal(removed in manifest, false, `manifest retained obsolete ${removed}`);
  }
  const arkadeBase = manifest.arkadeServiceUrl.replace(/\/$/, '');
  const arkadeHost = new URL(manifest.arkadeServiceUrl).hostname;
  const localFunding = ['127.0.0.1', 'localhost'].includes(arkadeHost);
  if (!localFunding && !FUND_COMMAND) {
    throw new Error(
      'WOODLAND_REGROWTH_FUND_COMMAND is required for a remote world; it receives <address> <sats>',
    );
  }

  await Promise.all([
    waitForHttp(`${WEB_URL}/health.json`, 20_000),
    waitForHttp(`${arkadeBase}/v1/info`, 20_000),
    waitForHttp(`${manifest.emulatorUrl.replace(/\/$/, '')}/v1/block-tip`, 20_000),
    ...driverConfigs.flatMap(({ port, websocketPort }, index) => [
      assertPortAvailable(port, `regrowth WebDriver ${index + 1}`),
      assertPortAvailable(websocketPort, `regrowth WebDriver ${index + 1} WebSocket`),
    ]),
  ]);

  for (const [index, config] of driverConfigs.entries()) {
    const driverUrl = `http://127.0.0.1:${config.port}`;
    drivers.push(await startGeckodriver(
      ['--port', String(config.port), '--websocket-port', String(config.websocketPort)],
      ROOT,
      `${driverUrl}/status`,
      `regrowth WebDriver ${index + 1}`,
    ));
    browsers.push(await createBrowser(driverUrl, index === 0 ? 'chopper' : 'regrower'));
  }
  const [chopper, regrower] = browsers;
  await Promise.all(browsers.map((browser) => waitFor(
    `${browser.label} boot`,
    browser.inspect,
    (view) => view.ready && view.state?.fundingRequiredSats === 330,
    BOOT_TIMEOUT_MS,
  )));
  await Promise.all(browsers.map((browser) => browser.execute(
    `globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(0, 0, arguments[0], arguments[1]);`,
    [
      Math.min(VIEWPORT_MAX, manifest.mapWidth - 1),
      Math.min(VIEWPORT_MAX, manifest.mapHeight - 1),
    ],
  )));

  const initial = await chopper.inspect();
  const fundArgs = FUND_COMMAND
    ? [initial.state.address, String(initial.state.fundingRequiredSats)]
    : ['fund', initial.state.address, String(initial.state.fundingRequiredSats)];
  execFileSync(FUND_COMMAND || path.join(ROOT, 'scripts/regtest.sh'), fundArgs, {
    cwd: ROOT,
    stdio: 'pipe',
    encoding: 'utf8',
    timeout: 120_000,
  });
  await chopper.refreshWorld();
  await waitFor(
    'chopper activation funding',
    chopper.inspect,
    (view) => view.state?.activationReady === true,
    OPERATION_TIMEOUT_MS,
  );
  await chopper.execute(`document.getElementById('activate').click();`);
  await waitFor(
    'chopper activation',
    chopper.inspect,
    (view) => view.state?.playerActive && Boolean(view.state.playerAsset),
    OPERATION_TIMEOUT_MS,
  );
  assert.equal((await regrower.inspect()).state.playerActive, false, 'regrower must remain inactive');

  let [chopperState] = await refreshBoth(chopper, regrower, 'initial');
  const candidates = chopperState.trees
    .filter((tree) => tree.health === ACTIVE_LOGS_PER_TREE
      && tree.logReserveRemaining === LOG_RESERVE_PER_TREE
      && tree.xpRemaining === LOG_RESERVE_PER_TREE)
    .sort((left, right) => left.treeId - right.treeId);
  assert.ok(candidates.length > 0, 'no pristine tree is available inside the viewport');
  let target = candidates.find((tree) => tree.treeId === 417) || candidates[0];
  await chopper.clickMapCell(target.x, target.y + 1);
  const positioned = await waitFor(
    `chopper movement next to tree ${target.treeId}`,
    chopper.inspect,
    (view) => !view.busy && view.adjacentTree?.treeId === target.treeId,
    OPERATION_TIMEOUT_MS,
  );
  chopperState = positioned.state;
  target = chopperState.trees.find((tree) => tree.treeId === target.treeId);
  const initialOutpoint = target.treeOutpoint;
  let rounds = 0;
  let drops = 0;

  while (target.health > 0) {
    rounds += 1;
    assert.ok(rounds <= MAX_ROUNDS, `stumping tree exceeded ${MAX_ROUNDS} rounds`);
    const before = target;
    const result = await chopper.chop(chopperState, before);
    assert.equal(result.ok, true, `round ${rounds}: ${result.message || 'chop rejected'}`);
    chopperState = result.state;
    target = chopperState.trees.find((tree) => tree.treeId === before.treeId);
    assert.ok(target, `round ${rounds}: target tree disappeared`);
    assert.notEqual(target.treeOutpoint, before.treeOutpoint, `round ${rounds}: tree did not rotate`);
    const dropped = before.logReserveRemaining - target.logReserveRemaining;
    assert.equal(dropped, Number(before.nextDrop), `round ${rounds}: reward prediction mismatch`);
    assert.equal(target.xpRemaining, before.xpRemaining - dropped, `round ${rounds}: XP mismatch`);
    assert.equal(target.health, before.health - dropped, `round ${rounds}: health mismatch`);
    drops += dropped;
  }

  assert.equal(drops, ACTIVE_LOGS_PER_TREE, 'one full health cycle must yield ten LOG');
  assert.equal(target.logReserveRemaining, LOG_RESERVE_PER_TREE - ACTIVE_LOGS_PER_TREE);
  assert.equal(target.xpRemaining, LOG_RESERVE_PER_TREE - ACTIVE_LOGS_PER_TREE);
  assert.equal(target.depleted, false, 'funded stump must not be terminal');
  assert.ok(target.stumpHeight > 0, 'final chop must record a Bitcoin stump height');
  assert.equal(target.regrowAtHeight, target.stumpHeight + 2);
  const stumpOutpoint = target.treeOutpoint;
  const stumpTip = await blockTip(manifest.emulatorUrl);
  assert.equal(stumpTip.height, target.stumpHeight, 'final chop must stamp the attested tip');

  const early = await regrower.regrow(target.treeId);
  assert.match(early.message || '', /requires Bitcoin height/i, 'same-tip regrowth must fail');
  const [, sameTipState] = await refreshBoth(chopper, regrower, 'same-tip rejection');
  assert.equal(
    sameTipState.trees.find((tree) => tree.treeId === target.treeId)?.treeOutpoint,
    stumpOutpoint,
    'same-tip rejection changed the tree lineage',
  );
  mine(1);
  const oneTip = await blockTip(manifest.emulatorUrl);
  assert.equal(oneTip.height, target.stumpHeight + 1, 'first tip advance was not observed');
  const stillEarly = await regrower.regrow(target.treeId);
  assert.match(stillEarly.message || '', /requires Bitcoin height/i, 'one-tip regrowth must fail');
  const [, oneTipState] = await refreshBoth(chopper, regrower, 'one-tip rejection');
  assert.equal(
    oneTipState.trees.find((tree) => tree.treeId === target.treeId)?.treeOutpoint,
    stumpOutpoint,
    'one-tip rejection changed the tree lineage',
  );
  mine(1);
  const twoTips = await blockTip(manifest.emulatorUrl);
  assert.equal(twoTips.height, target.stumpHeight + 2, 'second tip advance was not observed');

  await regrower.clickMapCell(target.x, target.y);
  await waitFor(
    'anonymous map-click regrowth',
    regrower.inspect,
    (view) => !view.busy
      && view.state?.playerActive === false
      && view.state?.trees.find((tree) => tree.treeId === target.treeId)?.health
        === ACTIVE_LOGS_PER_TREE
      && view.state?.trees.find((tree) => tree.treeId === target.treeId)?.stumpHeight === 0,
    OPERATION_TIMEOUT_MS,
  );
  const [finalChopper, finalRegrower] = await refreshBoth(chopper, regrower, 'post-regrowth');
  const regrown = finalRegrower.trees.find((tree) => tree.treeId === target.treeId);
  assert.ok(regrown, 'regrown tree disappeared');
  assert.equal(regrown.health, ACTIVE_LOGS_PER_TREE, 'regrowth must restore ten health');
  assert.equal(regrown.stumpHeight, 0, 'regrowth must clear the stump height');
  assert.equal(regrown.regrowAtHeight, null, 'active tree must not advertise regrowth');
  assert.equal(regrown.logReserveRemaining, target.logReserveRemaining, 'regrowth minted LOG');
  assert.equal(regrown.xpRemaining, target.xpRemaining, 'regrowth minted XP');
  assert.equal(regrown.depleted, false);
  assert.notEqual(regrown.treeOutpoint, stumpOutpoint, 'regrowth must rotate the lineage');
  assert.equal(
    finalChopper.playerLogs,
    ACTIVE_LOGS_PER_TREE,
    'chopper must own exactly the ten local LOG removed from the tree',
  );
  assert.equal(finalChopper.playerXp, ACTIVE_LOGS_PER_TREE);

  const stumpRecord = await waitForIndexedRecord(arkadeBase, stumpOutpoint, true);
  const regrownRecord = await waitForIndexedRecord(arkadeBase, regrown.treeOutpoint, false);
  assert.equal(stumpRecord.script, manifest.treeScript, 'stump changed covenant script');
  assert.equal(regrownRecord.script, manifest.treeScript, 'regrowth changed covenant script');
  assert.equal(Number(regrownRecord.amount), 330, 'regrowth changed tree sats');
  const holdings = new Map(
    regrownRecord.assets.map((asset) => [asset.assetId, Number(asset.amount)]),
  );
  assert.equal(holdings.get(manifest.treeAsset), 1);
  assert.equal(holdings.get(manifest.logAsset), LOG_RESERVE_PER_TREE - ACTIVE_LOGS_PER_TREE);
  assert.equal(holdings.get(manifest.xpAsset), LOG_RESERVE_PER_TREE - ACTIVE_LOGS_PER_TREE);

  const [logs, xp] = await Promise.all([
    fetchAssetSupply(arkadeBase, manifest.logAsset),
    fetchAssetSupply(arkadeBase, manifest.xpAsset),
  ]);
  assert.equal(logs, INDEXED_LOG_SUPPLY, 'indexed LOG supply changed');
  assert.equal(xp, INDEXED_XP_SUPPLY, 'indexed XP supply changed');

  const report = {
    profile: 'regrowth',
    webUrl: WEB_URL,
    arkadeServiceUrl: manifest.arkadeServiceUrl,
    emulatorUrl: manifest.emulatorUrl,
    treeId: target.treeId,
    coordinate: { x: target.x, y: target.y },
    initialOutpoint,
    stumpOutpoint,
    regrownOutpoint: regrown.treeOutpoint,
    stumpHeight: target.stumpHeight,
    eligibleHeight: target.regrowAtHeight,
    rounds,
    drops,
    localReserveAfter: regrown.logReserveRemaining,
    permissionlessRegrowerActive: finalRegrower.playerActive,
    indexedAssetSupplies: { logs, xp },
    durationMs: Date.now() - startedAt,
  };
  await mkdir(path.dirname(reportPath), { recursive: true });
  await writeFile(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report));
} catch (error) {
  console.error(error);
  for (const browser of browsers) {
    try {
      console.error(`${browser.label}: ${JSON.stringify(await browser.inspect())}`);
      await saveScreenshot(browser.driverUrl, browser.sessionId, `regrowth-${browser.label}.png`);
    } catch {}
  }
  for (const [index, driver] of drivers.entries()) {
    if (driver.output()) console.error(`regrowth WebDriver ${index + 1} output:\n${driver.output()}`);
  }
  process.exitCode = 1;
} finally {
  await Promise.all(browsers.map(async (browser) => {
    try { await browser.wd('DELETE', ''); } catch {}
  }));
  await Promise.all(drivers.map(stopProcess));
}
