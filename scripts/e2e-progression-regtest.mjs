#!/usr/bin/env node
// Progression E2E: one fixed-key Firefox player earns every material,
// crafts all three axe tiers under the live covenant, then proves the final
// Iron Axe state survives owner renewal and browser recovery.
import { execFileSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import {
  assertPortAvailable,
  saveScreenshot,
  startGeckodriver,
  stopProcess,
  waitFor,
  waitForHttp,
  webdriverRequest,
} from './e2e-runtime.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const WEB_URL = (process.env.WOODLAND_E2E_WEB_URL || 'http://127.0.0.1:8090').replace(/\/$/, '');
const DRIVER_PORT = setting('WOODLAND_PROGRESSION_DRIVER_PORT', 16_800, 1_024, 60_000);
const OPERATION_TIMEOUT_MS = setting(
  'WOODLAND_PROGRESSION_TIMEOUT_MS',
  180_000,
  30_000,
  900_000,
);
const MAX_SWINGS = setting('WOODLAND_PROGRESSION_MAX_SWINGS', 6_600, 100, 10_000);
const MAX_SUCCESSES = setting('WOODLAND_PROGRESSION_MAX_SUCCESSES', 600, 100, 1_000);
const DETERMINISTIC_SECRET = '06'.repeat(32);
const TREE_COUNT = 420;
const INVENTORY_SUPPLY = 21_000_000;
const FUND_COMMAND = process.env.WOODLAND_PROGRESSION_FUND_COMMAND;
const reportPath = path.resolve(
  ROOT,
  process.env.WOODLAND_PROGRESSION_REPORT || 'regtest/_build/progression-report.json',
);
const startedAt = Date.now();

function setting(name, fallback, minimum, maximum) {
  const value = Number(process.env[name] || fallback);
  if (!Number.isInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be an integer from ${minimum} to ${maximum}`);
  }
  return value;
}

function fundAddress(address, sats) {
  const args = FUND_COMMAND
    ? [address, String(sats)]
    : ['fund', address, String(sats)];
  execFileSync(FUND_COMMAND || path.join(ROOT, 'scripts/regtest.sh'), args, {
    cwd: ROOT,
    stdio: 'pipe',
    encoding: 'utf8',
    timeout: 120_000,
  });
}

function playerView(state) {
  return {
    address: state.address,
    playerActive: state.playerActive,
    playerAsset: state.playerAsset,
    playerStateOutpoint: state.playerStateOutpoint,
    playerStateExpiresInSeconds: state.playerStateExpiresInSeconds,
    playerLuckCredit: state.playerLuckCredit,
    playerXp: state.playerXp,
    playerLevel: state.playerLevel,
    playerLogs: state.playerLogs,
    playerStone: state.playerStone,
    playerIronOre: state.playerIronOre,
    playerAxe: state.playerAxe,
    logDropBasisPoints: state.logDropBasisPoints,
    nextAxeRecipe: state.nextAxeRecipe,
    craftAxeReady: state.craftAxeReady,
    walletSats: state.walletSats,
    walletVtxos: state.walletVtxos,
    pendingChopTxid: state.pendingChopTxid,
  };
}

function expectedDropRate(manifest, xp, axe) {
  const levelBonus = manifest.levelLogDropXpThresholds
    .filter((threshold) => xp >= threshold).length
    * manifest.levelLogDropBonusBasisPoints;
  const levelRate = Math.min(
    manifest.maxLevelLogDropBasisPoints,
    manifest.baseLogDropBasisPoints + levelBonus,
  );
  const axeBonus = {
    none: 0,
    wooden: 200,
    stone: 500,
    iron: 800,
  }[axe];
  assert.notEqual(axeBonus, undefined, `unknown axe tier ${axe}`);
  return Math.min(manifest.maxLogDropBasisPoints, levelRate + axeBonus);
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

async function createBrowser(driverUrl) {
  const session = await webdriverRequest(driverUrl, 'POST', '/session', {
    capabilities: {
      alwaysMatch: {
        browserName: 'firefox',
        unhandledPromptBehavior: 'accept',
        'moz:firefoxOptions': { args: ['-headless'] },
      },
    },
  }, OPERATION_TIMEOUT_MS);
  assert.ok(session.sessionId, 'progression WebDriver session has no ID');
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
      busy: Boolean(document.getElementById('refresh')?.disabled),
      state: globalThis.__WOODLAND_E2E_STATE || null,
      bagAxe: document.getElementById('player-axe')?.textContent || '',
      axeSlotLabel: document.getElementById('axe-slot')?.getAttribute('aria-label') || '',
      craftAxeText: document.getElementById('craft-axe')?.textContent || '',
      craftAxeDisabled: Boolean(document.getElementById('craft-axe')?.disabled),
      axeRecipeText: document.getElementById('axe-recipe')?.textContent || '',
      log: document.getElementById('log')?.textContent || '',
    };
  `);
  const refreshWorld = () => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_REFRESH_WORLD()
      .then((state) => done({ state }))
      .catch((error) => done({ failure: String(error) }));
  `);
  const chop = (state, tree) => executeAsync(`
    const done = arguments[arguments.length - 1];
    const [treeId, treeOutpoint, playerStateOutpoint, expectedDrop] = arguments;
    globalThis.__WOODLAND_E2E_CHOP_EXPECTED(
      treeId,
      treeOutpoint,
      playerStateOutpoint,
      expectedDrop,
    ).then((result) => {
      const current = result.state;
      const currentTree = current.trees.find((candidate) => candidate.treeId === treeId);
      done({
        ok: result.ok,
        message: result.message || '',
        player: {
          address: current.address,
          playerActive: current.playerActive,
          playerAsset: current.playerAsset,
          playerStateOutpoint: current.playerStateOutpoint,
          playerStateExpiresInSeconds: current.playerStateExpiresInSeconds,
          playerLuckCredit: current.playerLuckCredit,
          playerXp: current.playerXp,
          playerLevel: current.playerLevel,
          playerLogs: current.playerLogs,
          playerStone: current.playerStone,
          playerIronOre: current.playerIronOre,
          playerAxe: current.playerAxe,
          logDropBasisPoints: current.logDropBasisPoints,
          nextAxeRecipe: current.nextAxeRecipe,
          craftAxeReady: current.craftAxeReady,
          walletSats: current.walletSats,
          pendingChopTxid: current.pendingChopTxid,
        },
        tree: currentTree,
        lastAttempt: current.lastAttempt,
      });
    }).catch((error) => done({ ok: false, message: String(error) }));
  `, [tree.treeId, tree.treeOutpoint, state.playerStateOutpoint, tree.nextDrop]);
  const craft = () => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_CRAFT_AXE()
      .then(() => done({ ok: true }))
      .catch((error) => done({ ok: false, failure: String(error) }));
  `);
  const renew = () => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_RENEW_PLAYER()
      .then(() => done({ ok: true }))
      .catch((error) => done({ ok: false, failure: String(error) }));
  `);
  await wd('POST', '/timeouts', { implicit: 0, pageLoad: 30_000, script: OPERATION_TIMEOUT_MS });
  await wd('POST', '/url', { url: `${WEB_URL}/?progression=1` });
  return {
    driverUrl,
    sessionId,
    wd,
    execute,
    inspect,
    refreshWorld,
    chop,
    craft,
    renew,
  };
}

let driver = null;
let browser = null;
try {
  const manifestResponse = await fetch(`${WEB_URL}/world.json`, {
    signal: AbortSignal.timeout(20_000),
  });
  assert.equal(manifestResponse.ok, true, `world manifest returned ${manifestResponse.status}`);
  const manifest = await manifestResponse.json();
  assert.equal(manifest.schemaVersion, 3);
  assert.equal(manifest.protocolVersion, 3);
  assert.equal(manifest.rulesetId, 'woodland.sh/forest/v3');
  assert.equal(manifest.trees.length, TREE_COUNT);
  assert.equal(manifest.woodcuttingXpPerLog, 25);
  assert.deepEqual(manifest.axeRecipes, [
    { axe: 'wooden', requiredLevel: 1, logCost: 1, stoneCost: 0, ironOreCost: 0 },
    { axe: 'stone', requiredLevel: 5, logCost: 2, stoneCost: 2, ironOreCost: 0 },
    { axe: 'iron', requiredLevel: 15, logCost: 5, stoneCost: 0, ironOreCost: 2 },
  ]);
  const arkadeBase = manifest.arkadeServiceUrl.replace(/\/$/, '');
  const arkadeHost = new URL(manifest.arkadeServiceUrl).hostname;
  if (!['127.0.0.1', 'localhost'].includes(arkadeHost) && !FUND_COMMAND) {
    throw new Error(
      'WOODLAND_PROGRESSION_FUND_COMMAND is required for a remote world; it receives <address> <sats>',
    );
  }

  const driverUrl = `http://127.0.0.1:${DRIVER_PORT}`;
  await Promise.all([
    waitForHttp(`${WEB_URL}/health.json`, 20_000),
    waitForHttp(`${arkadeBase}/v1/info`, 20_000),
    waitForHttp(`${manifest.emulatorUrl.replace(/\/$/, '')}/v1/info`, 20_000),
    assertPortAvailable(DRIVER_PORT, 'progression WebDriver'),
  ]);
  driver = await startGeckodriver(
    ['--port', String(DRIVER_PORT)],
    ROOT,
    `${driverUrl}/status`,
    'progression WebDriver',
  );
  browser = await createBrowser(driverUrl);
  await waitFor(
    'initial progression browser boot',
    browser.inspect,
    (view) => view.ready && view.state?.fundingRequiredSats === 330,
    OPERATION_TIMEOUT_MS,
  );

  await browser.execute(`
    localStorage.clear();
    localStorage.setItem('woodland.sh:web:v2:key:' + location.origin, arguments[0]);
  `, [DETERMINISTIC_SECRET]);
  await browser.wd('POST', '/refresh', {});
  let view = await waitFor(
    'deterministic progression wallet boot',
    browser.inspect,
    (candidate) => candidate.ready
      && !candidate.busy
      && candidate.state?.fundingRequiredSats === 330
      && !candidate.state.playerActive,
    OPERATION_TIMEOUT_MS,
  );
  assert.equal(
    await browser.execute(`return globalThis.__WOODLAND_E2E_APP.exportKey();`),
    DETERMINISTIC_SECRET,
  );

  fundAddress(view.state.address, view.state.fundingRequiredSats);
  const funded = await browser.refreshWorld();
  assert.equal(funded.failure, undefined, funded.failure);
  await waitFor(
    'progression activation funding',
    browser.inspect,
    (candidate) => candidate.state?.activationReady === true,
    OPERATION_TIMEOUT_MS,
  );
  await browser.execute(`document.getElementById('activate').click();`);
  view = await waitFor(
    'progression player activation',
    browser.inspect,
    (candidate) => candidate.state?.playerActive
      && Boolean(candidate.state.playerAsset)
      && candidate.state.playerAxe === 'none'
      && candidate.state.playerXp === 0,
    OPERATION_TIMEOUT_MS,
  );
  const playerAsset = view.state.playerAsset;
  const declaredTrees = manifest.trees
    .map(({ state }) => state)
    .sort((left, right) => left.treeId - right.treeId);
  let treeIndex = -1;
  let current = { player: playerView(view.state), tree: null };
  let swings = 0;
  let successes = 0;
  let stoneFinds = 0;
  let ironOreFinds = 0;
  let testedStoneBoundary = false;
  let testedIronBoundary = false;
  const crafts = [];

  const selectNextTree = async () => {
    treeIndex += 1;
    const declared = declaredTrees[treeIndex];
    assert.ok(declared, 'progression exhausted declared trees');
    await browser.execute(`
      globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(
        arguments[0],
        arguments[1],
        arguments[0],
        arguments[1],
      );
    `, [declared.x, declared.y]);
    const refreshed = await browser.refreshWorld();
    assert.equal(refreshed.failure, undefined, refreshed.failure);
    const tree = refreshed.state.trees.find((candidate) => candidate.treeId === declared.treeId);
    assert.ok(tree, `tree ${declared.treeId} disappeared`);
    assert.ok(tree.health > 0, `tree ${declared.treeId} is already a stump`);
    assert.ok(Number.isInteger(tree.nextRollBucket), `tree ${declared.treeId} has no roll forecast`);
    current = { player: playerView(refreshed.state), tree };
  };

  const assertCraftRejected = async (pattern, label) => {
    const before = current.player;
    const result = await browser.craft();
    assert.equal(result.ok, false, `${label}: craft unexpectedly succeeded`);
    assert.match(result.failure || '', pattern, label);
    const unchanged = await browser.inspect();
    assert.equal(unchanged.state.playerStateOutpoint, before.playerStateOutpoint, label);
    assert.equal(unchanged.state.playerAxe, before.playerAxe, label);
    assert.equal(unchanged.state.playerLogs, before.playerLogs, label);
    assert.equal(unchanged.state.playerStone, before.playerStone, label);
    assert.equal(unchanged.state.playerIronOre, before.playerIronOre, label);
    current.player = playerView(unchanged.state);
  };

  const craftTier = async (recipe) => {
    const before = current.player;
    assert.equal(before.nextAxeRecipe?.axe, recipe.axe, `${recipe.axe}: next recipe`);
    assert.equal(before.craftAxeReady, true, `${recipe.axe}: recipe readiness`);
    const result = await browser.craft();
    assert.equal(result.ok, true, `${recipe.axe}: ${result.failure || 'craft failed'}`);
    const crafted = await browser.inspect();
    const after = playerView(crafted.state);
    assert.notEqual(after.playerStateOutpoint, before.playerStateOutpoint, `${recipe.axe}: state`);
    assert.equal(after.playerAsset, before.playerAsset, `${recipe.axe}: PLAYER_ID`);
    assert.equal(after.playerXp, before.playerXp, `${recipe.axe}: XP`);
    assert.equal(after.playerLuckCredit, before.playerLuckCredit, `${recipe.axe}: luck`);
    assert.equal(after.playerLogs, before.playerLogs - recipe.logCost, `${recipe.axe}: LOG burn`);
    assert.equal(
      after.playerStone,
      before.playerStone - recipe.stoneCost,
      `${recipe.axe}: STONE burn`,
    );
    assert.equal(
      after.playerIronOre,
      before.playerIronOre - recipe.ironOreCost,
      `${recipe.axe}: IRON ORE burn`,
    );
    assert.equal(after.playerAxe, recipe.axe, `${recipe.axe}: equipped tier`);
    assert.equal(
      after.logDropBasisPoints,
      expectedDropRate(manifest, after.playerXp, recipe.axe),
      `${recipe.axe}: drop rate`,
    );
    const expectedName = `${recipe.axe[0].toUpperCase()}${recipe.axe.slice(1)}`;
    assert.equal(crafted.bagAxe, expectedName, `${recipe.axe}: bag label`);
    assert.equal(crafted.axeSlotLabel, `${expectedName} Axe equipped`, `${recipe.axe}: slot label`);
    crafts.push({
      axe: recipe.axe,
      before: before.playerStateOutpoint,
      after: after.playerStateOutpoint,
      xp: after.playerXp,
    });
    current.player = after;
    console.log(
      `crafted ${expectedName} Axe at ${after.playerXp} XP; `
        + `${after.playerLogs} LOG, ${after.playerStone} STONE, ${after.playerIronOre} IRON ORE remain`,
    );
  };

  await selectNextTree();
  while (current.player.playerAxe !== 'iron') {
    assert.ok(swings < MAX_SWINGS, `Iron Axe exceeded ${MAX_SWINGS} swings`);
    assert.ok(successes < MAX_SUCCESSES, `Iron Axe exceeded ${MAX_SUCCESSES} successful LOGs`);
    if (current.tree.health === 0) await selectNextTree();

    const before = current;
    const result = await browser.chop(before.player, before.tree);
    if (!result.ok) {
      assert.match(result.message, /chop precondition changed/i, result.message);
      const refreshed = await browser.refreshWorld();
      assert.equal(refreshed.failure, undefined, refreshed.failure);
      const tree = refreshed.state.trees.find(
        (candidate) => candidate.treeId === before.tree.treeId,
      );
      current = { player: playerView(refreshed.state), tree };
      continue;
    }
    swings += 1;
    const success = result.player.playerXp === before.player.playerXp + manifest.woodcuttingXpPerLog;
    assert.equal(
      result.player.playerXp,
      before.player.playerXp + Number(success) * manifest.woodcuttingXpPerLog,
      `swing ${swings}: XP delta`,
    );
    assert.equal(
      result.player.playerLogs,
      before.player.playerLogs + Number(success),
      `swing ${swings}: LOG delta`,
    );
    assert.notEqual(
      result.player.playerStateOutpoint,
      before.player.playerStateOutpoint,
      `swing ${swings}: player state did not rotate`,
    );
    assert.notEqual(
      result.tree.treeOutpoint,
      before.tree.treeOutpoint,
      `swing ${swings}: tree did not rotate`,
    );
    assert.equal(
      result.tree.health,
      before.tree.health - Number(success),
      `swing ${swings}: health delta`,
    );
    const stoneDelta = result.player.playerStone - before.player.playerStone;
    const ironOreDelta = result.player.playerIronOre - before.player.playerIronOre;
    assert.ok([0, 1].includes(stoneDelta), `swing ${swings}: STONE delta ${stoneDelta}`);
    assert.ok([0, 1].includes(ironOreDelta), `swing ${swings}: IRON ORE delta ${ironOreDelta}`);
    assert.ok(stoneDelta + ironOreDelta <= 1, `swing ${swings}: multiple materials`);
    assert.equal(
      result.tree.stoneRemaining,
      before.tree.stoneRemaining - stoneDelta,
      `swing ${swings}: tree STONE`,
    );
    assert.equal(
      result.tree.ironOreRemaining,
      before.tree.ironOreRemaining - ironOreDelta,
      `swing ${swings}: tree IRON ORE`,
    );
    if (!success) {
      assert.equal(stoneDelta + ironOreDelta, 0, `swing ${swings}: material on miss`);
      assert.equal(result.lastAttempt?.material, 'none', `swing ${swings}: miss material`);
    }
    if (before.player.playerLevel < manifest.ironOreUnlockLevel) {
      assert.equal(ironOreDelta, 0, `swing ${swings}: IRON ORE before level 10`);
    }
    const expectedMaterial = stoneDelta ? 'stone' : ironOreDelta ? 'ironOre' : 'none';
    assert.equal(result.lastAttempt?.material, expectedMaterial, `swing ${swings}: material report`);
    assert.equal(
      result.player.logDropBasisPoints,
      expectedDropRate(manifest, result.player.playerXp, result.player.playerAxe),
      `swing ${swings}: drop rate`,
    );
    assert.equal(result.player.pendingChopTxid ?? null, null, `swing ${swings}: pending chop`);
    if (success) successes += 1;
    stoneFinds += stoneDelta;
    ironOreFinds += ironOreDelta;
    current = { player: result.player, tree: result.tree };

    if (current.player.playerAxe === 'none' && current.player.playerLogs >= 1) {
      await craftTier(manifest.axeRecipes[0]);
      await assertCraftRejected(/requires Woodcutting level 5/, 'early Stone Axe');
    }
    if (
      current.player.playerAxe === 'wooden'
      && successes === 15
      && !testedStoneBoundary
    ) {
      await assertCraftRejected(/requires Woodcutting level 5/, 'Stone Axe XP boundary');
      testedStoneBoundary = true;
    }
    if (
      current.player.playerAxe === 'wooden'
      && successes >= 16
      && current.player.playerLogs >= manifest.axeRecipes[1].logCost
      && current.player.playerStone >= manifest.axeRecipes[1].stoneCost
    ) {
      await craftTier(manifest.axeRecipes[1]);
      await assertCraftRejected(/requires Woodcutting level 15/, 'early Iron Axe');
    }
    if (
      current.player.playerAxe === 'stone'
      && successes === 96
      && !testedIronBoundary
    ) {
      await assertCraftRejected(/requires Woodcutting level 15/, 'Iron Axe XP boundary');
      testedIronBoundary = true;
    }
    if (
      current.player.playerAxe === 'stone'
      && successes >= 97
      && current.player.playerLogs >= manifest.axeRecipes[2].logCost
      && current.player.playerIronOre >= manifest.axeRecipes[2].ironOreCost
    ) {
      await craftTier(manifest.axeRecipes[2]);
    }
    if (successes > 0 && successes % 25 === 0 && success) {
      console.log(
        `progression ${successes} LOGs / ${swings} swings: level ${current.player.playerLevel}, `
          + `${stoneFinds} STONE and ${ironOreFinds} IRON ORE found`,
      );
    }
  }

  assert.equal(testedStoneBoundary, true, 'Stone Axe exact level boundary was not exercised');
  assert.equal(testedIronBoundary, true, 'Iron Axe exact level boundary was not exercised');
  assert.deepEqual(crafts.map(({ axe }) => axe), ['wooden', 'stone', 'iron']);
  assert.ok(stoneFinds >= 2, 'progression did not find enough STONE');
  assert.ok(ironOreFinds >= 2, 'progression did not find enough IRON ORE');
  await assertCraftRejected(/already the highest tier/, 'maximum Iron Axe tier');

  view = await browser.inspect();
  assert.equal(view.state.playerAxe, 'iron');
  assert.equal(view.state.nextAxeRecipe, null);
  assert.equal(view.state.craftAxeReady, false);
  assert.equal(view.bagAxe, 'Iron');
  assert.equal(view.axeSlotLabel, 'Iron Axe equipped');
  assert.equal(view.craftAxeText, 'Highest axe crafted');
  assert.equal(view.craftAxeDisabled, true);
  assert.equal(view.axeRecipeText, 'Highest axe tier crafted');
  assert.equal(
    view.state.logDropBasisPoints,
    expectedDropRate(manifest, view.state.playerXp, 'iron'),
  );

  const totalLogBurn = manifest.axeRecipes.reduce((sum, recipe) => sum + recipe.logCost, 0);
  const totalStoneBurn = manifest.axeRecipes.reduce((sum, recipe) => sum + recipe.stoneCost, 0);
  const totalIronOreBurn = manifest.axeRecipes.reduce(
    (sum, recipe) => sum + recipe.ironOreCost,
    0,
  );
  const treeLogs = view.state.trees.reduce(
    (sum, tree) => sum + tree.logReserveRemaining,
    0,
  );
  const treeXp = view.state.trees.reduce((sum, tree) => sum + tree.xpRemaining, 0);
  const treeStone = view.state.trees.reduce((sum, tree) => sum + tree.stoneRemaining, 0);
  const treeIronOre = view.state.trees.reduce((sum, tree) => sum + tree.ironOreRemaining, 0);
  assert.equal(treeLogs + view.state.playerLogs, INVENTORY_SUPPLY - totalLogBurn);
  assert.equal(
    treeXp + view.state.playerXp,
    INVENTORY_SUPPLY * manifest.woodcuttingXpPerLog,
  );
  assert.equal(treeStone + view.state.playerStone, INVENTORY_SUPPLY - totalStoneBurn);
  assert.equal(treeIronOre + view.state.playerIronOre, INVENTORY_SUPPLY - totalIronOreBurn);

  const [treeSupply, logSupply, xpSupply, stoneSupply, ironOreSupply, playerSupply] = await Promise.all([
    fetchAssetSupply(arkadeBase, manifest.treeAsset),
    fetchAssetSupply(arkadeBase, manifest.logAsset),
    fetchAssetSupply(arkadeBase, manifest.xpAsset),
    fetchAssetSupply(arkadeBase, manifest.stoneAsset),
    fetchAssetSupply(arkadeBase, manifest.ironOreAsset),
    fetchAssetSupply(arkadeBase, playerAsset),
  ]);
  assert.equal(treeSupply, TREE_COUNT);
  assert.equal(logSupply, INVENTORY_SUPPLY - totalLogBurn);
  assert.equal(xpSupply, INVENTORY_SUPPLY);
  assert.equal(stoneSupply, INVENTORY_SUPPLY - totalStoneBurn);
  assert.equal(ironOreSupply, INVENTORY_SUPPLY - totalIronOreBurn);
  assert.equal(playerSupply, 1);

  fundAddress(view.state.address, 350);
  await browser.refreshWorld();
  view = await waitFor(
    'Iron Axe renewal funding',
    browser.inspect,
    (candidate) => candidate.state?.walletVtxos?.some(
      (vtxo) => vtxo.amountSats === 350 && (vtxo.assets || []).length === 0,
    ),
    OPERATION_TIMEOUT_MS,
  );
  const beforeRenewal = playerView(view.state);
  const treeOutpointsBeforeRenewal = view.state.trees.map((tree) => tree.treeOutpoint);
  const renewed = await browser.renew();
  assert.equal(renewed.ok, true, renewed.failure || 'Iron Axe renewal failed');
  view = await browser.inspect();
  const afterRenewal = playerView(view.state);
  assert.notEqual(afterRenewal.playerStateOutpoint, beforeRenewal.playerStateOutpoint);
  assert.ok(
    afterRenewal.playerStateExpiresInSeconds > beforeRenewal.playerStateExpiresInSeconds,
    'Iron Axe renewal did not extend expiry',
  );
  for (const field of [
    'playerAsset',
    'playerLuckCredit',
    'playerXp',
    'playerLevel',
    'playerLogs',
    'playerStone',
    'playerIronOre',
    'playerAxe',
    'logDropBasisPoints',
  ]) {
    assert.equal(afterRenewal[field], beforeRenewal[field], `renewal changed ${field}`);
  }
  assert.deepEqual(
    view.state.trees.map((tree) => tree.treeOutpoint),
    treeOutpointsBeforeRenewal,
    'player renewal changed tree state',
  );

  const renewedOutpoint = afterRenewal.playerStateOutpoint;
  await browser.wd('POST', '/refresh', {});
  view = await waitFor(
    'Iron Axe browser recovery',
    browser.inspect,
    (candidate) => candidate.ready
      && !candidate.busy
      && candidate.state?.playerStateOutpoint === renewedOutpoint
      && candidate.state.playerAxe === 'iron',
    OPERATION_TIMEOUT_MS,
  );
  assert.equal(view.state.playerAsset, playerAsset);
  assert.equal(view.state.playerLogs, afterRenewal.playerLogs);
  assert.equal(view.state.playerStone, afterRenewal.playerStone);
  assert.equal(view.state.playerIronOre, afterRenewal.playerIronOre);
  assert.equal(
    await browser.execute(`return globalThis.__WOODLAND_E2E_APP.exportKey();`),
    DETERMINISTIC_SECRET,
  );

  const report = {
    profile: 'progression',
    address: view.state.address,
    playerAsset,
    swings,
    successes,
    stoneFinds,
    ironOreFinds,
    playerXp: view.state.playerXp,
    playerLevel: view.state.playerLevel,
    playerLogs: view.state.playerLogs,
    playerStone: view.state.playerStone,
    playerIronOre: view.state.playerIronOre,
    playerAxe: view.state.playerAxe,
    logDropBasisPoints: view.state.logDropBasisPoints,
    treesUsed: treeIndex + 1,
    crafts,
    indexedAssetSupplies: {
      tree: treeSupply,
      log: logSupply,
      xp: xpSupply,
      stone: stoneSupply,
      ironOre: ironOreSupply,
      player: playerSupply,
    },
    renewedPlayerStateOutpoint: renewedOutpoint,
    durationMs: Date.now() - startedAt,
  };
  await mkdir(path.dirname(reportPath), { recursive: true });
  await writeFile(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report));
} catch (error) {
  console.error(error);
  if (browser) {
    try {
      console.error(`progression snapshot: ${JSON.stringify(await browser.inspect())}`);
      await saveScreenshot(browser.driverUrl, browser.sessionId, 'progression-failure.png');
    } catch {}
  }
  if (driver?.output()) console.error(`progression WebDriver output:\n${driver.output()}`);
  process.exitCode = 1;
} finally {
  if (browser) {
    try { await browser.wd('DELETE', ''); } catch {}
  }
  if (driver) await stopProcess(driver);
}
