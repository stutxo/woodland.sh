#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import {
  assertPortAvailable,
  decodeAssetMetadata,
  E2E_PROFILE,
  FULL_E2E,
  saveScreenshot,
  sleep,
  startGeckodriver,
  startProcess,
  stopProcess,
  waitFor,
  waitForHttp,
  webdriverRequest,
} from './e2e-runtime.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const WEB_PORT = Number(process.env.WOODLAND_E2E_WEB_PORT || 18776);
const DRIVER_PORT = Number(process.env.WOODLAND_E2E_DRIVER_PORT || 14455);
const EXTERNAL_WEB_URL = process.env.WOODLAND_E2E_WEB_URL?.replace(/\/$/, '');
const WEB_URL = EXTERNAL_WEB_URL || `http://127.0.0.1:${WEB_PORT}`;
const DRIVER_URL = `http://127.0.0.1:${DRIVER_PORT}`;
const TARGET_HITS = FULL_E2E ? 5 : 1;
const ARKD = 'http://127.0.0.1:7070';
const REQUIRE_RENEWAL_FEE = process.env.WOODLAND_E2E_REQUIRE_RENEWAL_FEE === '1';

function xpForLevel(level) {
  let points = 0;
  for (let current = 1; current < level; current += 1) {
    points += Math.floor(current + 300 * (2 ** (current / 7)));
  }
  return Math.floor(points / 4);
}

function levelFromXp(xp) {
  let level = 1;
  while (level < 99 && xp >= xpForLevel(level + 1)) level += 1;
  return level;
}

function assertProgression(state, label) {
  const level = levelFromXp(state.playerXp);
  assert.equal(state.playerLevel, level, `${label}: level`);
  assert.equal(
    state.playerNextLevelXp,
    level === 99 ? null : xpForLevel(level + 1),
    `${label}: next-level XP`,
  );
}

async function request(method, pathName, body) {
  return webdriverRequest(DRIVER_URL, method, pathName, body);
}

async function indexerVtxos(params) {
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
    const response = await fetch(`${ARKD}/v1/indexer/vtxos?${query}`);
    if (!response.ok) throw new Error(`indexer query failed: ${await response.text()}`);
    const payload = await response.json();
    records.push(...(payload.vtxos || []));
    const next = Number(payload.page?.next || 0);
    if (next <= pageIndex) break;
    pageIndex = next;
  }
  return records;
}

// Withdrawn LOG sits in plain wallet VTXOs, outside the player state.
function walletAssetTotal(state, assetId) {
  return (state.walletVtxos || []).reduce(
    (sum, vtxo) => sum + (vtxo.assets || []).reduce(
      (inner, asset) => inner + (asset.id === assetId ? asset.amount : 0),
      0,
    ),
    0,
  );
}

function cleanWalletVtxos(state) {
  return (state.walletVtxos || []).filter((vtxo) => (vtxo.assets || []).length === 0);
}

function assertRenewalFeeWallet(state, label) {
  const clean = cleanWalletVtxos(state);
  assert.equal(clean.length, 1, `${label}: expected one clean renewal-fee VTXO`);
  assert.ok(
    clean[0].amountSats >= state.dustSats,
    `${label}: renewal-fee change fell below dust`,
  );
  assert.equal(
    state.walletSats,
    state.dustSats + clean[0].amountSats,
    `${label}: unexpected wallet value`,
  );
}

function assertLogSupply(state, expected, label) {
  const treeLogs = state.trees.reduce((sum, tree) => sum + tree.logReserveRemaining, 0);
  const total = treeLogs + state.playerLogs + walletAssetTotal(state, state.logAsset);
  assert.equal(total, expected, `${label}: fixed LOG supply is ${total}`);
}

function assertMaterialSupply(state, expectedStone, expectedIronOre, label) {
  const treeStone = state.trees.reduce((sum, tree) => sum + tree.stoneRemaining, 0);
  const treeIronOre = state.trees.reduce((sum, tree) => sum + tree.ironOreRemaining, 0);
  assert.equal(
    treeStone + state.playerStone,
    expectedStone,
    `${label}: fixed STONE supply is ${treeStone + state.playerStone}`,
  );
  assert.equal(
    treeIronOre + state.playerIronOre,
    expectedIronOre,
    `${label}: fixed IRON ORE supply is ${treeIronOre + state.playerIronOre}`,
  );
}

function assertXpAccounting(state, expected, label) {
  const total = state.seasonXpRemaining + state.playerXp;
  assert.equal(total, expected, `${label}: remaining plus earned XP is ${total}`);
}

function assertTreeValue(state, label) {
  for (const tree of state.trees) {
    assert.equal(
      tree.valueSats,
      state.fullTreeValueSats,
      `${label}: tree ${tree.treeId} value changed with health`,
    );
  }
}

function assertFixedSats(state, expected, label) {
  const treeSats = state.trees.reduce((sum, tree) => sum + tree.valueSats, 0);
  const total = treeSats + state.walletSats;
  assert.equal(total, expected, `${label}: visible fixed sats total is ${total}`);
}

async function main() {
  await Promise.all([
    waitForHttp(`${ARKD}/v1/info`, 5_000),
    waitForHttp('http://127.0.0.1:7073/v1/info', 5_000),
    ...(EXTERNAL_WEB_URL ? [] : [assertPortAvailable(WEB_PORT, 'web server')]),
    assertPortAvailable(DRIVER_PORT, 'WebDriver'),
  ]);
  const web = EXTERNAL_WEB_URL
    ? null
    : startProcess('node', ['scripts/dev-server.mjs', '--port', String(WEB_PORT)], ROOT);
  let driver = null;
  let sessionId;
  let inspect = null;

  try {
    driver = await startGeckodriver(
      ['--port', String(DRIVER_PORT)],
      ROOT,
      `${DRIVER_URL}/status`,
      'WebDriver',
    );
    await Promise.all([
      waitForHttp(`${WEB_URL}/`, 20_000, web),
      waitForHttp(`${WEB_URL}/health.json`, 30_000, web),
    ]);
    const manifestResponse = await fetch(`${WEB_URL}/world.json`);
    assert.equal(manifestResponse.ok, true, 'world manifest is unavailable');
    const manifest = await manifestResponse.json();
    const treeCount = manifest.trees.length;
    const totalLogs = manifest.logReservePerTree * treeCount;
    const totalXp = manifest.xpPerTree * manifest.woodcuttingXpPerLog * treeCount;
    const totalStone = manifest.stoneReservePerTree * treeCount;
    const totalIronOre = manifest.ironOreReservePerTree * treeCount;
    const session = await request('POST', '/session', {
      capabilities: {
        alwaysMatch: {
          browserName: 'firefox',
          unhandledPromptBehavior: 'accept',
          webSocketUrl: FULL_E2E,
          'moz:firefoxOptions': { args: ['-headless'] },
        },
      },
    });
    sessionId = session.sessionId;
    const wd = (method, suffix, body) => request(method, `/session/${sessionId}${suffix}`, body);
    const execute = (script, args = []) => wd('POST', '/execute/sync', { script, args });
    const executeAsync = (script, args = []) => wd('POST', '/execute/async', { script, args });
    await wd('POST', '/timeouts', { implicit: 0, pageLoad: 30_000, script: 120_000 });

    inspect = () => execute(`
      return {
        ready: Boolean(globalThis.__WOODLAND_E2E_READY),
        error: globalThis.__WOODLAND_E2E_ERROR || '',
        state: globalThis.__WOODLAND_E2E_STATE || null,
        player: globalThis.__WOODLAND_E2E_PLAYER || null,
        walk: globalThis.__WOODLAND_E2E_LAST_WALK || null,
        adjacent: Boolean(globalThis.__WOODLAND_E2E_ADJACENT),
        adjacentTree: globalThis.__WOODLAND_E2E_ADJACENT_TREE || null,
        autoChop: globalThis.__WOODLAND_E2E_LAST_CHOP_RUN || null,
        treeEffects: globalThis.__WOODLAND_E2E_TREE_EFFECTS || [],
        busy: Boolean(document.getElementById('refresh')?.disabled),
        mapFrame: globalThis.__WOODLAND_E2E_MAP_FRAME || null,
        mapHint: document.getElementById('map-hint')?.textContent || '',
        mapCells: globalThis.__WOODLAND_E2E_MAP_FRAME?.visibleTileCount || 0,
        camera: (() => {
          const viewport = document.getElementById('map-viewport');
          const playerCell = document.getElementById('camera-player');
          if (!viewport || !playerCell) return null;
          const viewportBounds = viewport.getBoundingClientRect();
          const playerBounds = playerCell.getBoundingClientRect();
          return {
            target: globalThis.__WOODLAND_E2E_CAMERA || null,
            trail: globalThis.__WOODLAND_E2E_CAMERA_TRAIL || [],
            playerGlyph: playerCell.textContent,
            playerHidden: playerCell.hidden,
            clientWidth: viewport.clientWidth,
            clientHeight: viewport.clientHeight,
            playerCenterX: playerBounds.left + playerBounds.width / 2 - viewportBounds.left,
            playerCenterY: playerBounds.top + playerBounds.height / 2 - viewportBounds.top,
            viewportCenterX: viewportBounds.width / 2,
            viewportCenterY: viewportBounds.height / 2,
          };
        })(),
        bagLogs: document.getElementById('player-logs')?.textContent || '',
        bagStone: document.getElementById('player-stone')?.textContent || '',
        bagIronOre: document.getElementById('player-iron-ore')?.textContent || '',
        craftAxeDisabled: Boolean(document.getElementById('craft-axe')?.disabled),
        inventorySlots: document.querySelectorAll('.bag-slots > .bag-slot').length,
        emptySlots: document.querySelectorAll('.bag-slots > .empty-slot').length,
        inventoryRows: getComputedStyle(document.querySelector('.bag-slots')).gridTemplateRows
          .split(' ').filter(Boolean).length,
        bagWidth: Math.round(document.querySelector('.bag')?.getBoundingClientRect().width || 0),
        bagStats: document.querySelectorAll(
          '.bag #level-number, .bag #xp-number, .bag #log-chance, .bag #wallet-sats',
        ).length,
        statsRightOfBag: (() => {
          const bag = document.querySelector('.bag')?.getBoundingClientRect();
          const stats = document.querySelector('.stats-box')?.getBoundingClientRect();
          return Boolean(bag && stats && stats.left >= bag.right);
        })(),
        standingTreeGlyphs: globalThis.__WOODLAND_E2E_MAP_FRAME?.standingTreeCount || 0,
        stumpGlyphs: globalThis.__WOODLAND_E2E_MAP_FRAME?.stumpCount || 0,
        focusedTreeHealth: document.getElementById('tree-health')?.textContent || '',
        activateDisabled: Boolean(document.getElementById('activate')?.disabled),
        status: document.getElementById('status')?.textContent || '',
        log: document.getElementById('log')?.textContent || '',
      };
    `);
    const click = (id) => execute(`document.getElementById(arguments[0]).click();`, [id]);
    const clickMapCell = (x, y) => execute(`
      if (!globalThis.__WOODLAND_E2E_CLICK_MAP) throw new Error('missing canvas map hook');
      globalThis.__WOODLAND_E2E_CLICK_MAP(arguments[0], arguments[1]);
    `, [x, y]);
    const clickCanvasPoint = (x, y) => execute(`
      const canvas = document.getElementById('map');
      const frame = globalThis.__WOODLAND_E2E_MAP_FRAME;
      if (!canvas || !frame) throw new Error('missing canvas frame');
      const bounds = canvas.getBoundingClientRect();
      canvas.dispatchEvent(new MouseEvent('click', {
        bubbles: true,
        clientX: bounds.left + frame.originX + (arguments[0] + 0.5) * frame.tileSize,
        clientY: bounds.top + frame.originY + (arguments[1] + 0.5) * frame.tileSize,
      }));
    `, [x, y]);
    const clickTree = (treeId) => execute(`
      if (!globalThis.__WOODLAND_E2E_CLICK_TREE) throw new Error('missing canvas tree hook');
      globalThis.__WOODLAND_E2E_CLICK_TREE(arguments[0]);
    `, [treeId]);
    const assertInvalidXpRejected = async (treeId, label) => {
      const before = await inspect();
      const beforeTree = before.state.trees.find((tree) => tree.treeId === treeId);
      const result = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_INVALID_XP(arguments[0])
          .then((state) => done({ ok: true, state }))
          .catch((error) => done({ error: String(error) }));
      `, [treeId]);
      assert.equal(result.error, undefined, `${label}: ${result.error}`);
      assert.equal(result.ok, true, `${label}: invalid XP probe did not complete`);
      const afterTree = result.state.trees.find((tree) => tree.treeId === treeId);
      assert.equal(result.state.playerStateOutpoint, before.state.playerStateOutpoint, label);
      assert.equal(result.state.playerXp, before.state.playerXp, label);
      assert.equal(result.state.playerLogs, before.state.playerLogs, label);
      assert.equal(result.state.playerStone, before.state.playerStone, label);
      assert.equal(result.state.playerIronOre, before.state.playerIronOre, label);
      assert.equal(result.state.playerAxe, before.state.playerAxe, label);
      assert.equal(afterTree.treeOutpoint, beforeTree.treeOutpoint, label);
      assert.equal(afterTree.health, beforeTree.health, label);
      return { ...before, state: result.state };
    };
    const assertInvalidAssetPacketRejected = async (probe, treeId, label) => {
      const before = await inspect();
      const beforeTree = before.state.trees.find((tree) => tree.treeId === treeId);
      const result = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis[arguments[0]](arguments[1])
          .then((state) => done({ state }))
          .catch((error) => done({ error: String(error) }));
      `, [probe, treeId]);
      assert.equal(result.error, undefined, `${label}: ${result.error}`);
      assert.equal(result.state.playerStateOutpoint, before.state.playerStateOutpoint, label);
      assert.equal(result.state.playerXp, before.state.playerXp, label);
      assert.equal(result.state.playerLogs, before.state.playerLogs, label);
      assert.equal(result.state.playerStone, before.state.playerStone, label);
      assert.equal(result.state.playerIronOre, before.state.playerIronOre, label);
      assert.equal(result.state.playerAxe, before.state.playerAxe, label);
      const afterTree = result.state.trees.find((tree) => tree.treeId === treeId);
      assert.equal(
        afterTree.treeOutpoint,
        beforeTree.treeOutpoint,
        label,
      );
      assert.equal(afterTree.health, beforeTree.health, label);
      return { ...before, state: result.state };
    };
    const assertChopMutationRejected = async (mutation, treeId, label) => {
      const before = await inspect();
      const beforeTree = before.state.trees.find((tree) => tree.treeId === treeId);
      const result = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_CHOP_MUTATION(arguments[0], arguments[1])
          .then((state) => done({ state }))
          .catch((error) => done({ error: String(error) }));
      `, [mutation, treeId]);
      assert.equal(result.error, undefined, `${label}: ${result.error}`);
      assert.equal(result.state.playerStateOutpoint, before.state.playerStateOutpoint, label);
      assert.equal(result.state.playerXp, before.state.playerXp, label);
      assert.equal(result.state.playerLogs, before.state.playerLogs, label);
      assert.equal(result.state.playerStone, before.state.playerStone, label);
      assert.equal(result.state.playerIronOre, before.state.playerIronOre, label);
      assert.equal(result.state.playerAxe, before.state.playerAxe, label);
      const afterTree = result.state.trees.find((tree) => tree.treeId === treeId);
      assert.equal(afterTree.treeOutpoint, beforeTree.treeOutpoint, label);
      assert.equal(afterTree.health, beforeTree.health, label);
      return { ...before, state: result.state };
    };
    const expectedChop = (snapshot, tree, overrides = {}) => executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_CHOP_EXPECTED(...Array.from(arguments).slice(0, -1))
        .then(done);
    `, [
      tree.treeId,
      overrides.treeOutpoint ?? tree.treeOutpoint,
      overrides.playerStateOutpoint ?? snapshot.playerStateOutpoint,
      overrides.drop ?? tree.nextDrop,
    ]);
    const expectedChopAfterRenewal = async (snapshot, tree, label) => {
      const result = await expectedChop(snapshot, tree);
      if (result.ok) return { result, refreshed: null };

      assert.match(result.message, /chop precondition changed/i, label);
      const refreshed = await inspect();
      const currentTree = refreshed.state.trees.find(
        (candidate) => candidate.treeId === tree.treeId,
      );
      assert.ok(currentTree, `${label}: refreshed tree disappeared`);
      assert.ok(
        refreshed.state.playerStateOutpoint !== snapshot.playerStateOutpoint
          || currentTree.treeOutpoint !== tree.treeOutpoint,
        `${label}: precondition rejection did not expose a renewed state`,
      );
      assert.equal(refreshed.state.playerAsset, snapshot.playerAsset, label);
      assert.equal(refreshed.state.playerXp, snapshot.playerXp, label);
      assert.equal(refreshed.state.playerLogs, snapshot.playerLogs, label);
      assert.equal(refreshed.state.playerStone, snapshot.playerStone, label);
      assert.equal(refreshed.state.playerIronOre, snapshot.playerIronOre, label);
      assert.equal(refreshed.state.playerAxe, snapshot.playerAxe, label);
      assert.equal(refreshed.state.playerLuckCredit, snapshot.playerLuckCredit, label);
      assert.equal(refreshed.state.playerLevel, snapshot.playerLevel, label);
      assert.equal(currentTree.health, tree.health, label);
      assert.equal(currentTree.logReserveRemaining, tree.logReserveRemaining, label);
      assert.equal(currentTree.xpRemaining, tree.xpRemaining, label);
      assert.equal(currentTree.stoneRemaining, tree.stoneRemaining, label);
      assert.equal(currentTree.ironOreRemaining, tree.ironOreRemaining, label);
      assert.equal(currentTree.nextRollBucket, tree.nextRollBucket, label);
      assert.equal(currentTree.nextDrop, tree.nextDrop, label);
      return { result: null, refreshed };
    };

    const recoverLaterSwingAfterReload = async (snapshot, tree) => {
      assert.notEqual(
        tree.treeOutpoint,
        `${tree.deploymentTxid}:0`,
        'pending reload must use a second-or-later tree input',
      );
      assert.equal(typeof WebSocket, 'function', 'pending reload requires Node 22+ WebSocket');
      const socket = new WebSocket(session.capabilities.webSocketUrl);
      await new Promise((resolve, reject) => {
        socket.addEventListener('open', resolve, { once: true });
        socket.addEventListener('error', reject, { once: true });
      });
      let commandId = 0;
      const bidi = (method, params) => new Promise((resolve, reject) => {
        const id = ++commandId;
        const timer = setTimeout(() => {
          socket.removeEventListener('message', receive);
          reject(new Error(`BiDi ${method} timed out`));
        }, 30_000);
        const receive = ({ data }) => {
          const response = JSON.parse(data);
          if (response.id !== id) return;
          clearTimeout(timer);
          socket.removeEventListener('message', receive);
          if (response.type === 'error') reject(new Error(response.message));
          else resolve(response.result);
        };
        socket.addEventListener('message', receive);
        socket.send(JSON.stringify({ id, method, params }));
      });
      const blockKey = 'woodland-e2e-block-submit';
      const pendingKey = `woodland.sh:web:v2:pending:${manifest.arkadeServiceUrl.replace(/\/+$/, '')}:${manifest.genesisTxid}`;
      // Preload runs in the page's own realm before WASM starts. Only transport
      // fails: the signed journal and all gameplay still come from real WASM.
      const intercept = `() => {
        const original = globalThis.fetch;
        globalThis.__WOODLAND_BLOCKED_SUBMITS = 0;
        globalThis.fetch = async (...args) => {
          const url = new URL(args[0] instanceof Request ? args[0].url : args[0], location.href);
          if (url.origin === ${JSON.stringify(new URL(manifest.emulatorUrl).origin)}
            && url.pathname === '/v1/tx'
            && sessionStorage.getItem(${JSON.stringify(blockKey)}) === '1') {
            globalThis.__WOODLAND_BLOCKED_SUBMITS += 1;
            throw new TypeError('local test transport unavailable');
          }
          return original(...args);
        };
      }`;
      let preload;
      try {
        preload = await bidi('script.addPreloadScript', { functionDeclaration: intercept });
        await execute('sessionStorage.setItem(arguments[0], "1");', [blockKey]);
        const { contexts } = await bidi('browsingContext.getTree', {});
        const installed = await bidi('script.callFunction', {
          functionDeclaration: intercept,
          target: { context: contexts[0].context },
          awaitPromise: false,
        });
        assert.equal(installed.type, 'success', JSON.stringify(installed));
        const rejected = await expectedChop(snapshot, tree);
        assert.equal(rejected.ok, false, 'blocked swing unexpectedly settled');
        const stored = await execute(`
          return {
            journal: localStorage.getItem(arguments[0]),
            blocked: globalThis.__WOODLAND_BLOCKED_SUBMITS,
          };
        `, [pendingKey]);
        assert.ok(stored.blocked > 0, 'emulator submission was not intercepted');
        assert.ok(stored.journal, 'unresolved swing has no persisted journal');
        const journal = JSON.parse(stored.journal);
        assert.equal(journal.treeInput, tree.treeOutpoint);
        assert.equal(journal.playerStateInput, snapshot.playerStateOutpoint);
        assert.equal(journal.treeId, tree.treeId);
        assert.equal(rejected.state.pendingChopTxid, journal.expectedTxid);

        await wd('POST', '/refresh', {});
        const reloaded = await waitFor(
          'unresolved later swing survives browser startup',
          () => execute(`return {
            ready: Boolean(globalThis.__WOODLAND_E2E_READY),
            busy: Boolean(document.getElementById('refresh')?.disabled),
            recoveryError: globalThis.__WOODLAND_E2E_ERROR || '',
          };`),
          (value) => !value.busy && (value.ready || value.recoveryError),
          180_000,
        );
        assert.equal(reloaded.ready, true, reloaded.recoveryError);
        assert.match(reloaded.recoveryError, /local test transport unavailable/);
        reloaded.state = await execute('return globalThis.__WOODLAND_E2E_STATE;');
        assert.equal(reloaded.state.pendingChopTxid, journal.expectedTxid);
        assert.equal(
          await execute('return localStorage.getItem(arguments[0]);', [pendingKey]),
          stored.journal,
          'reload replaced or discarded the exact signed journal',
        );
        for (const field of [
          'playerStateOutpoint', 'playerAsset', 'playerXp', 'playerLuckCredit',
          'playerLogs', 'playerStone', 'playerIronOre', 'playerAxe',
        ]) {
          assert.equal(reloaded.state[field], snapshot[field], `unresolved reload changed ${field}`);
        }
        assert.ok(
          await execute('return globalThis.__WOODLAND_BLOCKED_SUBMITS;') > 0,
          'reload did not retry the pending submission',
        );

        // Recover with a deliberately disjoint viewport: the pending tree must
        // be refreshed independently of the ordinary visible-world filter.
        const farX = tree.x < manifest.mapWidth / 2 ? manifest.mapWidth - 1 : 0;
        const farY = tree.y < manifest.mapHeight / 2 ? manifest.mapHeight - 1 : 0;
        await execute(`
          globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(arguments[0], arguments[1], arguments[0], arguments[1]);
          sessionStorage.removeItem(arguments[2]);
        `, [farX, farY, blockKey]);
        const resumed = await executeAsync(`
          const done = arguments[arguments.length - 1];
          globalThis.__WOODLAND_E2E_RESUME_PENDING()
            .then((state) => done({ state }))
            .catch((error) => done({ error: String(error) }));
        `);
        assert.equal(resumed.error, undefined, resumed.error);
        assert.equal(resumed.state.pendingChopTxid ?? null, null);
        assert.equal(resumed.state.playerStateOutpoint, `${journal.expectedTxid}:0`);
        assert.equal(
          await execute('return localStorage.getItem(arguments[0]);', [pendingKey]),
          null,
          'settled exact journal was not cleared',
        );
        // Snapshots publish only visible trees. Bring the recovered tree back
        // into view before checking its displayed lineage and reward balances.
        const visible = await executeAsync(`
          const done = arguments[arguments.length - 1];
          globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT();
          globalThis.__WOODLAND_E2E_REFRESH_WORLD()
            .then((state) => done({ state }))
            .catch((error) => done({ error: String(error) }));
        `);
        assert.equal(visible.error, undefined, visible.error);
        assert.equal(
          visible.state.trees.find((candidate) => candidate.treeId === tree.treeId).treeOutpoint,
          `${journal.expectedTxid}:1`,
        );
        // This boot error was the deliberately blocked transport, now proved
        // recovered. Do not leak it into later independent E2E diagnostics.
        await bidi('script.evaluate', {
          expression: 'globalThis.__WOODLAND_E2E_ERROR = null',
          target: { context: contexts[0].context },
          awaitPromise: false,
        });
        console.log(`recovered later swing ${journal.expectedTxid} after reload outside viewport`);
        return { result: { ok: true, state: visible.state }, refreshed: null };
      } finally {
        await execute(`
          sessionStorage.removeItem(arguments[0]);
          globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT?.();
        `, [blockKey]);
        try {
          if (preload) await bidi('script.removePreloadScript', { script: preload.script });
        } finally {
          socket.close();
        }
      }
    };
    const assertExpectedChopPreconditions = async (snapshot, tree) => {
      const mutations = [
        { treeOutpoint: `${tree.treeOutpoint}-stale` },
        { playerStateOutpoint: `${snapshot.playerStateOutpoint}-stale` },
        { drop: !tree.nextDrop },
      ];
      for (const mutation of mutations) {
        const result = await expectedChop(snapshot, tree, mutation);
        assert.equal(result.ok, false, 'stale expected chop was accepted');
        assert.match(result.message, /chop precondition changed/i);
        const after = await inspect();
        assert.equal(after.state.playerStateOutpoint, snapshot.playerStateOutpoint);
        assert.equal(
          after.state.trees.find((candidate) => candidate.treeId === tree.treeId).treeOutpoint,
          tree.treeOutpoint,
        );
      }
    };
    const assertPlayerRenewed = async (before, label) => {
      const fundingBefore = cleanWalletVtxos(before.state);
      assert.equal(fundingBefore.length, 1, `${label}: missing renewal-fee funding`);
      const treeOutpoints = before.state.trees.map((tree) => tree.treeOutpoint);
      const result = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_RENEW_PLAYER()
          .then((state) => done({ state }))
          .catch((error) => done({ error: String(error) }));
      `);
      assert.equal(result.error, undefined, `${label}: ${result.error}`);
      const after = await inspect();
      const fundingAfter = cleanWalletVtxos(after.state);
      assert.equal(fundingAfter.length, 1, `${label}: missing renewal-fee change`);
      if (REQUIRE_RENEWAL_FEE) {
        assert.ok(
          fundingAfter[0].amountSats < fundingBefore[0].amountSats,
          `${label}: configured intent fee was not paid`,
        );
      } else {
        assert.ok(
          fundingAfter[0].amountSats <= fundingBefore[0].amountSats,
          `${label}: renewal fee change increased`,
        );
      }
      assert.ok(
        fundingAfter[0].amountSats >= after.state.dustSats,
        `${label}: renewal-fee change fell below dust`,
      );
      assert.equal(
        before.state.walletSats - after.state.walletSats,
        fundingBefore[0].amountSats - fundingAfter[0].amountSats,
        `${label}: renewal changed value outside the fee wallet`,
      );
      assert.notEqual(after.state.playerStateOutpoint, before.state.playerStateOutpoint, label);
      assert.ok(
        after.state.playerStateExpiresInSeconds > before.state.playerStateExpiresInSeconds,
        `${label}: expiry did not increase`,
      );
      assert.equal(after.state.playerXp, before.state.playerXp, label);
      assert.equal(after.state.playerLogs, before.state.playerLogs, label);
      assert.equal(after.state.playerStone, before.state.playerStone, label);
      assert.equal(after.state.playerIronOre, before.state.playerIronOre, label);
      assert.equal(after.state.playerAxe, before.state.playerAxe, label);
      assert.equal(after.state.playerAsset, before.state.playerAsset, label);
      assert.deepEqual(
        after.state.trees.map((tree) => tree.treeOutpoint),
        treeOutpoints,
        label,
      );
      return after;
    };
    const assertAutoChopUntilLog = async (before, label) => {
      const tree = before.state.trees.find((candidate) => (
        candidate.health > 0
        && Math.abs(before.player.x - candidate.x) + Math.abs(before.player.y - candidate.y) > 1
      ));
      assert.ok(tree, `${label}: no distant standing tree is available`);
      await clickTree(tree.treeId);
      const after = await waitFor(
        label,
        inspect,
        (value) => !value.busy
          && value.autoChop?.treeId === tree.treeId
          && value.autoChop.success
          && value.autoChop.swings >= 1
          && value.state?.playerLogs === before.state.playerLogs + 1
          && value.state.playerXp === before.state.playerXp + manifest.woodcuttingXpPerLog,
        180_000,
      );
      assert.ok(
        after.autoChop.swings <= 11,
        `${label}: auto-chop exceeded the player luck bound`,
      );
      assert.ok(
        after.autoChop.durationMs >= (after.autoChop.swings - 1) * 900,
        `${label}: swing animations ran faster than one-second cadence`,
      );
      assert.equal(after.error, '', `${label}: ${after.error}`);
      const effects = after.treeEffects
        .filter((effect) => effect.treeId === tree.treeId)
        .map((effect) => effect.type);
      assert.ok(effects.includes('chop'), `${label}: chop flash was not emitted`);
      assert.equal(effects.at(-1), 'log', `${label}: LOG flash was not emitted`);
      const material = after.autoChop.material;
      assert.ok(['none', 'stone', 'ironOre'].includes(material), `${label}: material enum`);
      const stoneReward = Number(material === 'stone');
      const ironOreReward = Number(material === 'ironOre');
      const currentTree = after.state.trees.find(
        (candidate) => candidate.treeId === tree.treeId,
      );
      assert.equal(currentTree.health, tree.health - 1);
      assert.equal(currentTree.stoneRemaining, tree.stoneRemaining - stoneReward);
      assert.equal(currentTree.ironOreRemaining, tree.ironOreRemaining - ironOreReward);
      assert.equal(after.state.playerStone, before.state.playerStone + stoneReward);
      assert.equal(after.state.playerIronOre, before.state.playerIronOre + ironOreReward);
      assert.equal(
        Math.abs(after.player.x - tree.x) + Math.abs(after.player.y - tree.y),
        1,
        `${label}: mouse path did not stop beside the tree`,
      );
      assertLogSupply(after.state, before.state.trees.reduce(
        (sum, candidate) => sum + candidate.logReserveRemaining,
        before.state.playerLogs,
      ), label);
      assertMaterialSupply(after.state, totalStone, totalIronOre, label);
      console.log(
        `continuous chop tree ${tree.treeId}: LOG after ${after.autoChop.swings} swings`,
      );
      return after;
    };
    const craftWoodenAxe = async (before, label) => {
      assert.equal(before.state.playerAxe, 'none', `${label}: initial axe`);
      assert.equal(before.state.nextAxeRecipe?.axe, 'wooden', `${label}: next recipe`);
      assert.equal(before.state.craftAxeReady, true, `${label}: recipe readiness`);
      assert.ok(before.state.playerLogs >= 1, `${label}: Wooden Axe requires one LOG`);
      const treeOutpoints = before.state.trees.map((tree) => tree.treeOutpoint);
      const result = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_CRAFT_AXE(arguments[0])
          .then((state) => done({ state }))
          .catch((error) => done({ error: String(error) }));
      `, [before.state.playerStateOutpoint]);
      assert.equal(result.error, undefined, `${label}: ${result.error}`);
      const after = await inspect();
      assert.equal(after.state.playerAxe, 'wooden', `${label}: equipped axe`);
      assert.equal(after.state.playerLogs, before.state.playerLogs - 1, `${label}: LOG burn`);
      assert.equal(after.state.playerXp, before.state.playerXp, `${label}: XP preservation`);
      assert.equal(after.state.playerStone, before.state.playerStone, `${label}: STONE preservation`);
      assert.equal(
        after.state.playerIronOre,
        before.state.playerIronOre,
        `${label}: IRON ORE preservation`,
      );
      assert.equal(
        after.state.playerLuckCredit,
        before.state.playerLuckCredit,
        `${label}: luck preservation`,
      );
      assert.equal(
        after.state.logDropBasisPoints,
        before.state.logDropBasisPoints + 200,
        `${label}: Wooden Axe bonus`,
      );
      assert.equal(after.state.nextAxeRecipe?.axe, 'stone', `${label}: tier progression`);
      assert.equal(after.state.craftAxeReady, false, `${label}: next recipe must remain locked`);
      assert.notEqual(after.state.playerStateOutpoint, before.state.playerStateOutpoint, label);
      assert.deepEqual(
        after.state.trees.map((tree) => tree.treeOutpoint),
        treeOutpoints,
        `${label}: crafting touched a tree`,
      );
      assert.equal(after.craftAxeDisabled, true, `${label}: locked next craft button`);
      assertLogSupply(after.state, totalLogs - 1, label);
      assertMaterialSupply(after.state, totalStone, totalIronOre, label);
      assertXpAccounting(after.state, totalXp, label);

      const rejected = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_CRAFT_AXE(arguments[0])
          .then(() => done({ ok: true }))
          .catch((error) => done({ rejection: String(error) }));
      `, [after.state.playerStateOutpoint]);
      assert.match(rejected.rejection || '', /requires Woodcutting level 5/);
      const unchanged = await inspect();
      assert.equal(unchanged.state.playerStateOutpoint, after.state.playerStateOutpoint, label);
      assert.equal(unchanged.state.playerAxe, 'wooden', label);
      return unchanged;
    };
    // The first-party game deliberately has no withdrawal control. Keep
    // protocol/API coverage for a future marketplace or third-party client.
    const runWithdrawStage = async (before, label, expectedLogSupply = totalLogs) => {
      assert.ok(before.state.playerLogs >= 1, `${label}: withdraw needs at least 1 LOG`);
      const walletOutpointsBefore = new Set(
        before.state.walletVtxos.map((vtxo) => vtxo.outpoint),
      );
      assert.equal(
        before.state.walletVtxos.some(
          (vtxo) => vtxo.amountSats === 330 && (vtxo.assets || []).length === 0,
        ),
        false,
        `${label}: exact withdrawal funding already exists`,
      );
      execFileSync(
        path.join(ROOT, 'scripts/regtest.sh'),
        ['fund', before.state.address, '330'],
        { cwd: ROOT, stdio: 'pipe', encoding: 'utf8' },
      );
      await click('refresh');
      let view = await waitFor(
        `${label} withdraw funding`,
        inspect,
        (value) => !value.busy
          && value.state?.walletVtxos?.some(
            (vtxo) => !walletOutpointsBefore.has(vtxo.outpoint)
              && vtxo.amountSats === 330
              && (vtxo.assets || []).length === 0,
          )
          && value.state.walletSats === before.state.walletSats + 330,
        180_000,
      );
      const logsBefore = view.state.playerLogs;
      const xpBefore = view.state.playerXp;
      const stoneBefore = view.state.playerStone;
      const ironOreBefore = view.state.playerIronOre;
      const axeBefore = view.state.playerAxe;
      // Soulbound proxy: XP has no withdrawal path at all — the withdraw
      // covenant pins the XP balance inside the player state (covered by the
      // native withdraw-leaf unit tests) and the WASM exposes no XP-moving
      // mutation hook. The client-side check is that withdrawing more LOG
      // than the player holds is rejected before any transaction exists.
      const rejected = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_WITHDRAW_LOG(arguments[0])
          .then(() => done({ ok: true }))
          .catch((error) => done({ rejection: String(error) }));
      `, [logsBefore + 1]);
      assert.ok(rejected.rejection, `${label}: over-balance withdraw was accepted`);
      assert.match(rejected.rejection, /exceeds the player LOG balance/);
      await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_REFRESH().then(done);
      `);
      const unchanged = await inspect();
      assert.equal(unchanged.state.playerLogs, logsBefore, label);
      assert.equal(unchanged.state.playerXp, xpBefore, label);
      assert.equal(unchanged.state.playerStone, stoneBefore, label);
      assert.equal(unchanged.state.playerIronOre, ironOreBefore, label);
      assert.equal(unchanged.state.playerAxe, axeBefore, label);
      assert.equal(
        unchanged.state.playerStateOutpoint,
        view.state.playerStateOutpoint,
        label,
      );
      const withdrawn = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_WITHDRAW_LOG(arguments[0])
          .then(() => done({ ok: true }))
          .catch((error) => done({ error: String(error) }));
      `, [1]);
      assert.equal(withdrawn.error, undefined, `${label}: ${withdrawn.error}`);
      view = await waitFor(
        `${label} LOG withdraw settlement`,
        inspect,
        (value) => !value.busy
          && value.state?.playerLogs === logsBefore - 1
          && value.state?.playerXp === xpBefore
          && value.state?.walletVtxos?.some((vtxo) => (vtxo.assets || []).some(
            (asset) => asset.id === value.state.logAsset && asset.amount === 1,
          )),
        180_000,
      );
      assert.notEqual(
        view.state.playerStateOutpoint,
        unchanged.state.playerStateOutpoint,
        label,
      );
      assert.equal(view.state.playerAsset, before.state.playerAsset, label);
      const destination = view.state.walletVtxos.find((vtxo) => (vtxo.assets || []).some(
        (asset) => asset.id === view.state.logAsset,
      ));
      assert.equal(destination.amountSats, 330, `${label}: withdraw destination value`);
      assert.equal(
        destination.assets
          .filter((asset) => asset.id === view.state.logAsset)
          .reduce((sum, asset) => sum + asset.amount, 0),
        1,
        `${label}: withdrawn LOG amount`,
      );
      assert.equal(view.state.playerXp, xpBefore, `${label}: XP must be soulbound`);
      assert.equal(view.state.playerStone, stoneBefore, `${label}: STONE must stay soulbound`);
      assert.equal(
        view.state.playerIronOre,
        ironOreBefore,
        `${label}: IRON ORE must stay soulbound`,
      );
      assert.equal(view.state.playerAxe, axeBefore, `${label}: axe must be preserved`);
      assert.equal(
        view.state.walletSats,
        before.state.walletSats + 330,
        `${label}: withdraw must not create or destroy sats`,
      );
      assertLogSupply(view.state, expectedLogSupply, label);
      assertMaterialSupply(view.state, totalStone, totalIronOre, label);
      assertXpAccounting(view.state, totalXp, label);
      return view;
    };

    await wd('POST', '/url', { url: `${WEB_URL}/` });
    const initial = await waitFor(
      'shared forest initialization',
      inspect,
      (value) => value.ready
        && value.state?.address?.startsWith('tark1')
        && value.state?.trees?.length === treeCount
        && value.state.trees.every((tree) => tree.health === 10)
        && !value.state?.fundingReady
        && !value.state?.playerActive,
    );
    assert.equal(initial.state.mapWidth, manifest.mapWidth);
    assert.equal(initial.state.mapHeight, manifest.mapHeight);
    assert.ok(initial.mapCells > 0 && initial.mapCells < initial.state.mapWidth * initial.state.mapHeight);
    assert.equal(initial.standingTreeGlyphs, treeCount);
    assert.equal(initial.stumpGlyphs, 0);
    assert.equal(initial.state.dustSats, 330);
    assert.equal(initial.state.fundingRequiredSats, 330);
    assert.equal(initial.state.fullTreeValueSats, 330);
    assert.equal(new Set(initial.state.trees.map((tree) => tree.treeId)).size, treeCount);
    assert.equal(new Set(initial.state.trees.map((tree) => `${tree.x}:${tree.y}`)).size, treeCount);
    assert.equal(new Set(initial.state.trees.map((tree) => tree.treeOutpoint)).size, treeCount);
    assert.ok(initial.state.trees.every((tree) => tree.logReserveRemaining === 50_000));
    assert.ok(initial.state.trees.every(
      (tree) => tree.xpRemaining === manifest.xpPerTree * manifest.woodcuttingXpPerLog,
    ));
    assert.deepEqual(
      [...new Set(initial.state.trees.map((tree) => tree.stoneRemaining))],
      [manifest.stoneReservePerTree],
      'initial STONE reserves',
    );
    assert.deepEqual(
      [...new Set(initial.state.trees.map((tree) => tree.ironOreRemaining))],
      [manifest.ironOreReservePerTree],
      'initial IRON ORE reserves',
    );
    assert.ok(initial.state.trees.every((tree) => tree.depleted === false));
    assert.equal(initial.state.playerLogs, 0);
    assert.equal(initial.state.fundingRequiredSats, initial.state.dustSats);
    assert.equal(initial.state.playerXp, 0);
    assert.equal(initial.state.playerStone, 0);
    assert.equal(initial.state.playerIronOre, 0);
    assert.equal(initial.state.playerAxe, 'none');
    assert.equal(initial.state.nextAxeRecipe, null);
    assert.equal(initial.state.craftAxeReady, false);
    assert.equal(initial.state.woodcuttingXpPerLog, 25);
    assert.equal(initial.state.playerLevel, 1);
    assert.equal(initial.state.playerNextLevelXp, 83);
    assert.equal(initial.state.seasonXpRemaining, totalXp);
    assert.equal(initial.state.activationReady, false);
    assert.equal(initial.activateDisabled, true);
    assert.equal(initial.state.playerAsset, null);
    assert.equal(initial.state.logDropBasisPoints, manifest.baseLogDropBasisPoints);
    assert.ok(initial.state.xpAsset);
    assert.equal(
      new Set([
        initial.state.treeAsset,
        initial.state.logAsset,
        initial.state.xpAsset,
        initial.state.stoneAsset,
        initial.state.ironOreAsset,
      ]).size,
      5,
    );
    assertLogSupply(initial.state, totalLogs, 'initial world');
    assertMaterialSupply(initial.state, totalStone, totalIronOre, 'initial world');
    assertXpAccounting(initial.state, totalXp, 'initial world');
    assertTreeValue(initial.state, 'initial world');
    assert.equal(manifest.schemaVersion, 4, 'world manifest must be schema 4');
    assert.equal(manifest.protocolVersion, 4, 'world manifest must declare protocol v4');
    assert.equal(manifest.rulesetId, 'woodland.sh/forest/v4');
    assert.match(manifest.deployerSigner, /^[0-9a-f]{64}$/);
    assert.match(manifest.manifestSignature, /^[0-9a-f]{128}$/);
    assert.equal(manifest.playerLevelCurve, 'woodland-xp-v1');
    assert.equal(manifest.woodcuttingXpPerLog, 25);
    assert.equal(manifest.baseLogDropBasisPoints, 2_000);
    assert.equal(manifest.levelLogDropBonusBasisPoints, 200);
    assert.deepEqual(
      manifest.levelLogDropXpThresholds,
      [1_154, 4_470, 13_363, 37_224, 101_333],
    );
    assert.equal(manifest.maxLevelLogDropBasisPoints, 3_000);
    assert.equal(manifest.maxLogDropBasisPoints, 3_800);
    assert.equal(manifest.stoneDropBasisPoints, 1_000);
    assert.equal(manifest.ironOreDropBasisPoints, 200);
    assert.equal(manifest.ironOreUnlockLevel, 10);
    assert.deepEqual(manifest.axeRecipes, [
      { axe: 'wooden', requiredLevel: 1, logCost: 1, stoneCost: 0, ironOreCost: 0 },
      { axe: 'stone', requiredLevel: 5, logCost: 2, stoneCost: 2, ironOreCost: 0 },
      { axe: 'iron', requiredLevel: 15, logCost: 5, stoneCost: 0, ironOreCost: 2 },
    ]);
    assert.equal(manifest.luckWindowBasisPoints, 10_000);
    assert.equal(manifest.initialLuckCredit, 8_000);
    assert.equal(manifest.trees.length, 420, 'world must contain exactly 420 trees');
    assert.equal(manifest.activeLogsPerTree, 10);
    assert.equal(manifest.logReservePerTree, 50_000);
    assert.equal(manifest.xpPerTree, 50_000);
    assert.equal(manifest.stoneReservePerTree, 50_000);
    assert.equal(manifest.ironOreReservePerTree, 50_000);
    assert.match(manifest.stoneAsset, /^[0-9a-f]{68}$/);
    assert.match(manifest.ironOreAsset, /^[0-9a-f]{68}$/);
    assert.equal(manifest.stoneAsset.slice(0, 64), manifest.genesisTxid);
    assert.equal(manifest.ironOreAsset.slice(0, 64), manifest.genesisTxid);
    assert.equal(manifest.stoneAsset.slice(64), '0300');
    assert.equal(manifest.ironOreAsset.slice(64), '0400');
    for (const [assetId, label, supply] of [
      [manifest.treeAsset, 'TREE', treeCount],
      [manifest.logAsset, 'LOG', totalLogs],
      [manifest.xpAsset, 'XP', manifest.xpPerTree * treeCount],
      [manifest.stoneAsset, 'STONE', totalStone],
      [manifest.ironOreAsset, 'IRON ORE', totalIronOre],
    ]) {
      const response = await fetch(`${ARKD}/v1/indexer/asset/${assetId}`);
      assert.equal(response.ok, true, `${label} asset query`);
      const details = await response.json();
      assert.equal(details.assetId, assetId, `${label} asset ID`);
      assert.equal(details.supply, String(supply), `${label} fixed supply`);
      assert.equal(details.controlAsset || '', '', `${label} control asset`);
      const metadata = decodeAssetMetadata(details.metadata);
      assert.deepEqual(
        [...metadata.keys()],
        ['game', 'protocol', 'ruleset', 'asset', 'deployer', 'rollover'],
      );
      assert.equal(metadata.get('game'), 'woodland.sh');
      assert.equal(metadata.get('protocol'), String(manifest.protocolVersion));
      assert.equal(metadata.get('ruleset'), manifest.rulesetId);
      assert.equal(metadata.get('asset'), label);
      assert.equal(metadata.get('deployer'), manifest.deployerSigner);
      assert.equal(metadata.get('rollover'), manifest.rolloverSigner);
    }
    for (const removed of [
      'treeRetireArkadeScript',
      'vaultScript',
      'vaultRestockArkadeScript',
      'vaultRenewalArkadeScript',
    ]) {
      assert.equal(removed in manifest, false, `manifest retained obsolete ${removed}`);
    }
    const positionAfterKeyboard = await execute(`
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'd', bubbles: true }));
      return globalThis.__WOODLAND_E2E_PLAYER;
    `);
    assert.deepEqual(positionAfterKeyboard, initial.player);
    console.log(`shared woodland ready: ${initial.state.trees.length} trees`);
    console.log(`player wallet ready: ${initial.state.address}`);
    await clickMapCell(4, 17);
    await waitFor(
      'inactive movement is blocked',
      inspect,
      (value) => !value.state?.playerActive
        && value.player?.x === initial.player.x
        && value.player?.y === initial.player.y
        && value.camera?.playerHidden === true,
    );

    execFileSync(
      path.join(ROOT, 'scripts/regtest.sh'),
      ['fund', initial.state.address, String(initial.state.fundingRequiredSats)],
      { cwd: ROOT, stdio: 'pipe', encoding: 'utf8' },
    );
    await click('refresh');
    await waitFor(
      'player activation funding',
      inspect,
      (value) => value.state?.activationReady
        && !value.state.playerActive
        && value.state.walletSats === initial.state.fundingRequiredSats
        && value.state.fundingRequiredSats === 0
        && !value.activateDisabled,
    );
    await click('activate');
    await waitFor(
      'recursive player activation',
      inspect,
      (value) => value.state?.fundingReady
        && value.state.playerActive
        && Boolean(value.state.playerAsset)
        && value.state.playerLevel === 1
        && value.state.playerNextLevelXp === 83
        && value.state.playerAxe === 'none'
        && value.state.nextAxeRecipe?.axe === 'wooden'
        && !value.state.craftAxeReady
        && !value.state.activationReady
        && value.state.logDropBasisPoints === manifest.baseLogDropBasisPoints
        && value.state.walletSats === 330
        && value.state.fundingRequiredSats === 0,
      300_000,
    );
    let deployed = await inspect();
    const playerAsset = deployed.state.playerAsset;
    assert.ok(playerAsset);
    assert.equal(
      new Set([
        playerAsset,
        deployed.state.treeAsset,
        deployed.state.logAsset,
        deployed.state.xpAsset,
        deployed.state.stoneAsset,
        deployed.state.ironOreAsset,
      ]).size,
      6,
    );
    const playerAssetResponse = await fetch(
      `http://127.0.0.1:7070/v1/indexer/asset/${playerAsset}`,
    );
    if (!playerAssetResponse.ok) {
      throw new Error(`PLAYER_ID asset query failed: ${await playerAssetResponse.text()}`);
    }
    const playerAssetInfo = await playerAssetResponse.json();
    assert.equal(playerAssetInfo.assetId, playerAsset);
    assert.equal(playerAssetInfo.supply, '1');
    assert.equal(playerAssetInfo.controlAsset || '', '');
    const playerAssetMetadata = decodeAssetMetadata(playerAssetInfo.metadata);
    assert.deepEqual(
      [...playerAssetMetadata.keys()],
      ['game', 'protocol', 'asset', 'owner'],
    );
    assert.equal(playerAssetMetadata.get('game'), 'woodland.sh');
    assert.equal(playerAssetMetadata.get('protocol'), String(manifest.protocolVersion));
    assert.equal(playerAssetMetadata.get('asset'), 'PLAYER_ID');
    assert.match(playerAssetMetadata.get('owner'), /^[0-9a-f]{64}$/);
    assert.ok(deployed.state.playerStateExpiresInSeconds > 300);
    {
      const indexedTrees = deployed.state.trees.filter(
        (tree) => tree.expiresInSeconds != null,
      );
      assert.ok(
        indexedTrees.length > 0
          && indexedTrees.every((tree) => tree.expiresInSeconds > 300),
      );
    }
    assert.deepEqual(
      deployed.state.trees.map((tree) => tree.treeOutpoint),
      initial.state.trees.map((tree) => tree.treeOutpoint),
    );
    assert.equal(deployed.state.playerLogs, 0);
    assert.equal(deployed.state.playerStone, 0);
    assert.equal(deployed.state.playerIronOre, 0);
    assert.equal(deployed.state.playerAxe, 'none');
    assert.equal(deployed.bagLogs, '0');
    assert.equal(deployed.bagStone, '0');
    assert.equal(deployed.bagIronOre, '0');
    assert.equal(deployed.craftAxeDisabled, true);
    assert.equal(deployed.inventorySlots, 9);
    assert.equal(deployed.emptySlots, 5);
    assert.equal(deployed.inventoryRows, 3);
    assert.equal(deployed.bagWidth, 262);
    assert.equal(deployed.bagStats, 0);
    assert.equal(deployed.statsRightOfBag, true);
    assertLogSupply(deployed.state, totalLogs, 'activated player');
    assertMaterialSupply(deployed.state, totalStone, totalIronOre, 'activated player');
    assertXpAccounting(deployed.state, totalXp, 'activated player');
    assertTreeValue(deployed.state, 'activated player');
    const playerRewardView = await executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_REFRESH_WORLD_RAW()
        .then((state) => done({ state }))
        .catch((error) => done({ error: String(error) }));
    `);
    assert.equal(playerRewardView.error, undefined, playerRewardView.error);
    const liveOutcomes = playerRewardView.state.trees.filter(
      (tree) => tree.health > 0 && tree.logReserveRemaining > 0 && tree.xpRemaining > 0,
    );
    assert.ok(liveOutcomes.length > 1);
    assert.equal(
      new Set(liveOutcomes.map((tree) => tree.nextRollBucket)).size,
      1,
      'changing tree target must not change player-bound entropy',
    );
    assert.equal(
      new Set(liveOutcomes.map((tree) => tree.nextDrop)).size,
      1,
      'changing tree target must not change the next reward',
    );
    const denseLocations = Array.from({ length: 500 }, (_, index) => ({
      playerAsset: `synthetic-${index}`,
      x: 4,
      y: 17,
      updatedAtMs: Date.now(),
    }));
    const denseFrame = await execute(`
      globalThis.__WOODLAND_E2E_SET_REMOTE_LOCATIONS(arguments[0]);
      return globalThis.__WOODLAND_E2E_MAP_FRAME;
    `, [denseLocations]);
    assert.equal(denseFrame.remotePlayerCount, 500);
    assert.equal(denseFrame.clusterCount, 1);
    assert.ok(denseFrame.visibleTileCount > 0);
    await execute(`globalThis.__WOODLAND_E2E_SET_REMOTE_LOCATIONS([]);`);
    await clickCanvasPoint(4, 17);
    await waitFor(
      'canvas coordinate movement',
      inspect,
      (value) => value.player?.x === 4 && value.player?.y === 17,
    );
    await execute(`globalThis.__WOODLAND_E2E_LAST_WALK = null;`);
    await clickCanvasPoint(3, 17);
    await waitFor(
      'canvas coordinate return',
      inspect,
      (value) => value.player?.x === 3
        && value.player?.y === 17
        && value.walk?.steps === 1,
    );
    const responsiveWalk = await inspect();
    assert.equal(responsiveWalk.walk.steps, 1);
    assert.ok(responsiveWalk.walk.durationMs >= 80, JSON.stringify(responsiveWalk.walk));
    assert.ok(responsiveWalk.walk.durationMs < 300, JSON.stringify(responsiveWalk.walk));
    execFileSync(
      path.join(ROOT, 'scripts/regtest.sh'),
      ['fund', deployed.state.address, '350'],
      { cwd: ROOT, stdio: 'pipe', encoding: 'utf8' },
    );
    await click('refresh');
    deployed = await waitFor(
      'player renewal fee funding',
      inspect,
      (value) => !value.busy
        && value.state?.walletVtxos?.some(
          (vtxo) => vtxo.amountSats === 350 && (vtxo.assets || []).length === 0,
        )
        && value.state.walletSats === deployed.state.walletSats + 350,
      180_000,
    );
    deployed = await assertPlayerRenewed(deployed, 'zero-XP player renewal');
    const playerStateBeforeBrowserReload = deployed.state.playerStateOutpoint;
    let fixedSats = deployed.state.trees.reduce(
      (sum, tree) => sum + tree.valueSats,
      deployed.state.walletSats,
    );

    await wd('POST', '/window/rect', { width: 390, height: 844 });
    await clickMapCell(30, 10);
    const mobileCamera = await waitFor(
      'mobile camera fixes player at viewport center',
      inspect,
      (value) => value.player?.x === 30
        && value.player?.y === 10
        && value.camera?.target?.x === 30
        && value.camera?.target?.y === 10
        && Math.abs(value.camera.target.mapX) > 200
        && Math.abs(value.camera.playerCenterX - value.camera.viewportCenterX) < 2
        && Math.abs(value.camera.playerCenterY - value.camera.viewportCenterY) < 2,
    );
    assert.ok(mobileCamera.camera.clientHeight <= 360);
    assert.ok(mobileCamera.camera.trail.length > 10);
    assert.ok(mobileCamera.camera.trail.every((frame) => (
      Math.abs(frame.deltaX) < 2 && Math.abs(frame.deltaY) < 2
    )));
    await clickMapCell(0, 0);
    const edgeCamera = await waitFor(
      'mobile camera keeps map edge fixed',
      inspect,
      (value) => value.player?.x === 0
        && value.player?.y === 0
        && value.camera?.target?.x === 0
        && value.camera?.target?.y === 0
        && Math.abs(value.camera.target.deltaX) < 2
        && Math.abs(value.camera.target.deltaY) < 2,
    );
    assert.ok(
      Math.abs(edgeCamera.camera.playerCenterX - edgeCamera.camera.viewportCenterX) < 4
        && Math.abs(edgeCamera.camera.playerCenterY - edgeCamera.camera.viewportCenterY) < 4,
      JSON.stringify(edgeCamera.camera),
    );
    assert.ok(edgeCamera.camera.trail.every((frame) => (
      Math.abs(frame.deltaX) < 2 && Math.abs(frame.deltaY) < 2
    )));
    await wd('POST', '/window/rect', { width: 1280, height: 900 });

    const firstTree = initial.state.trees.find((tree) => tree.treeId === 417);
    assert.ok(firstTree, 'deterministic tree 417 is unavailable');
    const initialOutpoints = new Map(
      initial.state.trees.map((tree) => [tree.treeId, tree.treeOutpoint]),
    );
    await clickMapCell(firstTree.x, firstTree.y + 1);
    await waitFor(
      `player movement next to tree ${firstTree.treeId}`,
      inspect,
      (value) => value.adjacent
        && value.adjacentTree?.treeId === firstTree.treeId
        && value.player?.x === firstTree.x
        && value.player?.y === firstTree.y + 1
        && value.camera?.clientWidth <= 720
        && Math.abs(value.camera.playerCenterX - value.camera.viewportCenterX) < 2
        && Math.abs(value.camera.playerCenterY - value.camera.viewportCenterY) < 2
        && value.camera.playerGlyph === '@'
        && value.mapFrame?.standingTreeCount === treeCount,
    );
    await wd('POST', '/refresh', {});
    await waitFor(
      'saved player and map position after browser reload',
      inspect,
      (value) => value.ready
        && value.state?.playerActive
        && value.state.playerStateOutpoint === playerStateBeforeBrowserReload
        && value.player?.x === firstTree.x
        && value.player?.y === firstTree.y + 1
        && value.adjacentTree?.treeId === firstTree.treeId,
      180_000,
    );

    let chopped = await assertInvalidXpRejected(
      firstTree.treeId,
      'XP counter increment on a miss must be rejected',
    );
    chopped = await assertInvalidAssetPacketRejected(
      '__WOODLAND_E2E_INVALID_ASSET_ORDER',
      firstTree.treeId,
      'noncanonical world asset group order must be rejected',
    );
    chopped = await assertInvalidAssetPacketRejected(
      '__WOODLAND_E2E_INVALID_XP_GROUP',
      firstTree.treeId,
      'a foreign group cannot replace XP',
    );
    chopped = await assertInvalidAssetPacketRejected(
      '__WOODLAND_E2E_INVALID_LOG_XP_ORDER',
      firstTree.treeId,
      'LOG and XP groups cannot trade positions',
    );
    for (const [mutation, label] of [
      ['wrong-roll', 'wrong roll successor must be rejected'],
      ['noncanonical-health-zero', 'negative-zero health encoding must be rejected'],
      ['wrong-luck-credit', 'wrong luck credit successor must be rejected'],
      ['wrong-log-delta', 'LOG delta inconsistent with the reward bit must be rejected'],
      ['wrong-xp-delta', 'XP transfer inconsistent with the reward bit must be rejected'],
      ['wrong-material-delta', 'material transfer inconsistent with the roll must be rejected'],
      ['asset-metadata', 'transfer metadata mutation must be rejected'],
      ['player-marker-metadata', 'PLAYER_ID transfer metadata must be rejected'],
      ['asset-control', 'transfer control-asset mutation must be rejected'],
      ['double-tree-marker', 'TREE marker inflation must be rejected'],
      ['extra-output', 'extra chop output must be rejected'],
      ['wrong-anchor', 'wrong anchor script must be rejected'],
      ['fund-extension', 'nonzero extension value must be rejected'],
    ]) {
      chopped = await assertChopMutationRejected(mutation, firstTree.treeId, label);
    }
    await assertExpectedChopPreconditions(
      chopped.state,
      chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId),
    );
    let hits = 0;
    let attempts = 0;
    let sawMiss = false;
    let checkedNonzeroXpMutation = false;
    let renewalRefreshes = 0;
    let testedPendingReload = false;
    while (hits < TARGET_HITS) {
      assert.ok(
        attempts < TARGET_HITS * 11,
        `${TARGET_HITS} LOG drop(s) exceeded the luck-protection bound`,
      );
      const before = chopped.state;
      const beforeTree = before.trees.find(
        (tree) => tree.treeId === firstTree.treeId,
      );
      const previousTreeOutpoint = beforeTree.treeOutpoint;
      assert.ok(Number.isInteger(beforeTree.nextRollBucket));
      assert.ok(beforeTree.nextRollBucket >= 0 && beforeTree.nextRollBucket < 10_000);
      const rawLuckCredit = before.playerLuckCredit + before.logDropBasisPoints;
      const expectedDrop = rawLuckCredit > manifest.luckWindowBasisPoints * 2
        ? true
        : rawLuckCredit < manifest.luckWindowBasisPoints
          ? false
          : beforeTree.nextRollBucket < before.logDropBasisPoints;
      assert.equal(beforeTree.nextDrop, expectedDrop);
      if (FULL_E2E && hits > 0 && !checkedNonzeroXpMutation) {
        chopped = await assertInvalidXpRejected(
          firstTree.treeId,
          'a nonzero-XP transition mismatch must be rejected',
        );
        checkedNonzeroXpMutation = true;
      }
      const recoverPending = FULL_E2E && attempts === 1 && !testedPendingReload;
      const attempt = recoverPending
        ? await recoverLaterSwingAfterReload(before, beforeTree)
        : await expectedChopAfterRenewal(before, beforeTree, 'recursive swing renewal');
      if (recoverPending) testedPendingReload = true;
      if (attempt.refreshed) {
        renewalRefreshes += 1;
        assert.ok(renewalRefreshes <= 20, 'recursive swings encountered excessive renewals');
        chopped = attempt.refreshed;
        continue;
      }
      attempts += 1;
      const { result } = attempt;
      assert.equal(result.ok, true, result.message);
      chopped = await waitFor(
        `recursive swing ${attempts}`,
        inspect,
        (value) => !value.busy
          && value.state?.trees?.find((tree) => tree.treeId === firstTree.treeId)?.treeOutpoint
            !== previousTreeOutpoint,
        180_000,
      );
      const success = chopped.state.lastAttempt?.success === true;
      assert.equal(success, beforeTree.nextDrop, 'published next-drop prediction must be exact');
      const material = chopped.state.lastAttempt?.material;
      assert.ok(['none', 'stone', 'ironOre'].includes(material), 'published material enum');
      if (!success) assert.equal(material, 'none', 'a miss cannot award materials');
      if (before.playerLevel < manifest.ironOreUnlockLevel) {
        assert.notEqual(material, 'ironOre', 'IRON ORE unlocked before level 10');
      }
      const stoneReward = Number(material === 'stone');
      const ironOreReward = Number(material === 'ironOre');
      if (success) hits += 1;
      else sawMiss = true;
      const current = chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId);
      assert.equal(current.health, 10 - hits);
      assert.equal(
        current.logReserveRemaining,
        beforeTree.logReserveRemaining - Number(success),
      );
      assert.equal(
        current.xpRemaining,
        beforeTree.xpRemaining - Number(success) * manifest.woodcuttingXpPerLog,
      );
      assert.equal(
        current.stoneRemaining,
        beforeTree.stoneRemaining - stoneReward,
      );
      assert.equal(
        current.ironOreRemaining,
        beforeTree.ironOreRemaining - ironOreReward,
      );
      assert.equal(chopped.state.playerStone, before.playerStone + stoneReward);
      assert.equal(chopped.state.playerIronOre, before.playerIronOre + ironOreReward);
      assert.equal(chopped.state.playerLogs, hits);
      assert.equal(
        chopped.state.playerXp,
        before.playerXp + Number(success) * manifest.woodcuttingXpPerLog,
      );
      assert.equal(chopped.state.playerXp, hits * manifest.woodcuttingXpPerLog);
      assertProgression(chopped.state, `swing ${attempts}`);
      assert.equal(chopped.state.logDropBasisPoints, manifest.baseLogDropBasisPoints);
      assert.equal(chopped.state.lastAttempt.treeId, firstTree.treeId);
      assert.equal(chopped.state.playerStateOutpoint === before.playerStateOutpoint, false);
      assert.equal(
        chopped.state.playerLuckCredit + Number(success) * manifest.luckWindowBasisPoints,
        before.playerLuckCredit + before.logDropBasisPoints,
        'luck credit must conserve expected reward value',
      );
      for (const tree of chopped.state.trees.filter((tree) => tree.treeId !== firstTree.treeId)) {
        assert.equal(tree.treeOutpoint, initialOutpoints.get(tree.treeId));
      }
      assertRenewalFeeWallet(chopped.state, `swing ${attempts}`);
      assert.equal(current.valueSats, chopped.state.fullTreeValueSats);
      assertLogSupply(chopped.state, totalLogs, `swing ${attempts}`);
      assertMaterialSupply(
        chopped.state,
        totalStone,
        totalIronOre,
        `swing ${attempts}`,
      );
      assertXpAccounting(chopped.state, totalXp, `swing ${attempts}`);
      assertTreeValue(chopped.state, `swing ${attempts}`);
      assertFixedSats(chopped.state, fixedSats, `swing ${attempts}`);
      console.log(
        `tree ${firstTree.treeId} swing ${attempts}: ${success ? 'LOG' : 'miss'}`
          + `${material === 'none' ? '' : ` + ${material}`} ${current.lastAttemptTxid}`,
      );
    }
    assert.ok(
      attempts >= TARGET_HITS && attempts <= TARGET_HITS * 11,
      'drop sequence exceeded the luck-protection bound',
    );
    if (FULL_E2E) {
      assert.equal(testedPendingReload, true, 'later-swing journal reload was not exercised');
      assert.ok(sawMiss, 'the bounded luck sequence must exercise at least one miss');
      assert.equal(checkedNonzeroXpMutation, true, 'nonzero-XP mutation probe did not run');
    }
    chopped = await assertPlayerRenewed(chopped, 'nonzero-XP player renewal');
    fixedSats = chopped.state.trees.reduce(
      (sum, tree) => sum + tree.valueSats,
      chopped.state.walletSats,
    );
    if (!FULL_E2E) {
      assert.equal(chopped.state.playerXp, manifest.woodcuttingXpPerLog);
      assert.equal(chopped.state.playerLogs, 1);
      assert.equal(
        chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId).health,
        9,
      );
      chopped = await assertAutoChopUntilLog(chopped, 'continuous chopping UX');
      assert.equal(chopped.bagLogs, String(chopped.state.playerLogs));
      assert.equal(chopped.state.craftAxeReady, true);
      assert.equal(chopped.craftAxeDisabled, false);
      chopped = await craftWoodenAxe(chopped, 'Wooden Axe crafting');
      assert.equal(chopped.bagLogs, String(chopped.state.playerLogs));
      chopped = await runWithdrawStage(chopped, 'smoke LOG withdraw', totalLogs - 1);
      console.log(JSON.stringify({
        profile: E2E_PROFILE,
        address: chopped.state.address,
        treeId: firstTree.treeId,
        rollAdvances: attempts,
        playerXp: chopped.state.playerXp,
        playerLogs: chopped.state.playerLogs,
        playerStone: chopped.state.playerStone,
        playerIronOre: chopped.state.playerIronOre,
        playerAxe: chopped.state.playerAxe,
      }));
      return;
    }
    assert.equal(chopped.mapFrame.stumpCount, 0);
    assert.equal(chopped.standingTreeGlyphs, treeCount);
    assert.equal(chopped.stumpGlyphs, 0);
    assert.equal(chopped.bagLogs, String(TARGET_HITS));
    const partialXp = chopped.state.playerXp;
    const partialLogs = chopped.state.playerLogs;
    const partialTree = chopped.state.trees.find(
      (tree) => tree.treeId === firstTree.treeId,
    );
    assert.equal(partialTree.health, 10 - TARGET_HITS);
    assert.equal(partialTree.logReserveRemaining, 50_000 - TARGET_HITS);
    assert.equal(
      partialTree.xpRemaining,
      (50_000 - TARGET_HITS) * manifest.woodcuttingXpPerLog,
    );
    assert.equal(partialTree.depleted, false);
    assertLogSupply(chopped.state, totalLogs, 'partial first-tree harvest');
    assertMaterialSupply(
      chopped.state,
      totalStone,
      totalIronOre,
      'partial first-tree harvest',
    );
    assertXpAccounting(chopped.state, totalXp, 'partial first-tree harvest');
    assertTreeValue(chopped.state, 'partial first-tree harvest');
    assertFixedSats(chopped.state, fixedSats, 'partial first-tree harvest');

    const secondTree = chopped.state.trees.find((tree) => tree.treeId === 426);
    assert.ok(secondTree, 'tree 426 is unavailable');
    await execute(`
      globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(
        arguments[0],
        arguments[1],
        arguments[0],
        arguments[1],
      );
    `, [secondTree.x, secondTree.y]);
    await executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_REFRESH().then(done);
    `);
    chopped = await inspect();
    const beforeSecondOutpoints = new Map(
      chopped.state.trees.map((tree) => [tree.treeId, tree.treeOutpoint]),
    );
    let secondAttempts = 0;
    let secondTreeRenewalRefreshes = 0;
    while (chopped.state.playerLogs === partialLogs) {
      assert.ok(secondAttempts < 11, 'second tree exceeded the player luck bound');
      const beforeTree = chopped.state.trees.find(
        (tree) => tree.treeId === secondTree.treeId,
      );
      const previous = beforeTree.treeOutpoint;
      const previousStateOutpoint = chopped.state.playerStateOutpoint;
      const attempt = await expectedChopAfterRenewal(
        chopped.state,
        beforeTree,
        'second-tree swing renewal',
      );
      if (attempt.refreshed) {
        secondTreeRenewalRefreshes += 1;
        assert.ok(
          secondTreeRenewalRefreshes <= 20,
          'second-tree swings encountered excessive renewals',
        );
        chopped = attempt.refreshed;
        continue;
      }
      secondAttempts += 1;
      const { result } = attempt;
      assert.equal(result.ok, true, result.message);
      chopped = await waitFor(
        `second tree swing ${secondAttempts}`,
        inspect,
        (value) => !value.busy
          && value.state?.trees?.find((tree) => tree.treeId === secondTree.treeId)?.treeOutpoint
            !== previous,
        180_000,
      );
      assert.notEqual(chopped.state.playerStateOutpoint, previousStateOutpoint);
    }
    assert.ok(secondAttempts >= 1 && secondAttempts <= 11);
    assert.equal(chopped.state.trees.find((tree) => tree.treeId === secondTree.treeId).health, 9);
    assert.equal(
      chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId).health,
      10 - TARGET_HITS,
    );
    assert.equal(chopped.state.playerLogs, partialLogs + 1);
    assert.equal(chopped.state.playerXp, partialXp + manifest.woodcuttingXpPerLog);
    assertProgression(chopped.state, 'continuous chop');
    assert.equal(chopped.state.logDropBasisPoints, manifest.baseLogDropBasisPoints);
    for (const tree of chopped.state.trees.filter((tree) => tree.treeId !== secondTree.treeId)) {
      assert.equal(tree.treeOutpoint, beforeSecondOutpoints.get(tree.treeId));
    }
    assertRenewalFeeWallet(chopped.state, 'second tree chop');
    assertLogSupply(chopped.state, totalLogs, 'second tree chop');
    assertMaterialSupply(chopped.state, totalStone, totalIronOre, 'second tree chop');
    assertXpAccounting(chopped.state, totalXp, 'second tree chop');
    assertTreeValue(chopped.state, 'second tree chop');
    assertFixedSats(chopped.state, fixedSats, 'second tree chop');
    await wd('POST', '/refresh', {});
    await waitFor(
      'browser app ready after final reload',
      inspect,
      (value) => value.ready && value.state?.playerActive,
      180_000,
    );
    await execute(`
      globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(
        arguments[0],
        arguments[1],
        arguments[2],
        arguments[3],
      );
    `, [
      Math.min(firstTree.x, secondTree.x),
      Math.min(firstTree.y, secondTree.y),
      Math.max(firstTree.x, secondTree.x),
      Math.max(firstTree.y, secondTree.y),
    ]);
    await executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_REFRESH().then(done);
    `);
    chopped = await waitFor(
      'world reconstruction after final reload',
      inspect,
      (value) => value.ready
        && value.state?.trees?.length === treeCount
        && value.state.playerActive
        && value.state.playerXp === partialXp + manifest.woodcuttingXpPerLog
        && value.state.playerLevel === levelFromXp(partialXp + manifest.woodcuttingXpPerLog)
        && value.state.playerNextLevelXp
          === xpForLevel(levelFromXp(partialXp + manifest.woodcuttingXpPerLog) + 1)
        && value.state.logDropBasisPoints === manifest.baseLogDropBasisPoints
        && value.state.playerLogs === partialLogs + 1
        && value.state.trees.find((tree) => tree.treeId === firstTree.treeId)?.health
          === 10 - TARGET_HITS
        && value.state.trees.find((tree) => tree.treeId === secondTree.treeId)?.health === 9,
    );
    assertLogSupply(chopped.state, totalLogs, 'final reload');
    assertMaterialSupply(chopped.state, totalStone, totalIronOre, 'final reload');
    assertXpAccounting(chopped.state, totalXp, 'final reload');
    assert.equal(chopped.state.playerAsset, playerAsset);
    assertTreeValue(chopped.state, 'final reload');
    assertFixedSats(chopped.state, fixedSats, 'final reload');
    await execute('globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT();');
    chopped = await assertAutoChopUntilLog(chopped, 'continuous chopping UX after full profile');
    chopped = await craftWoodenAxe(chopped, 'Wooden Axe crafting after full profile');
    chopped = await runWithdrawStage(chopped, 'full LOG withdraw', totalLogs - 1);
    console.log(JSON.stringify({
      profile: E2E_PROFILE,
      address: chopped.state.address,
      playerFundingSats: initial.state.fundingRequiredSats,
      treeAsset: chopped.state.treeAsset,
      logAsset: chopped.state.logAsset,
      xpAsset: chopped.state.xpAsset,
      stoneAsset: chopped.state.stoneAsset,
      ironOreAsset: chopped.state.ironOreAsset,
      playerAsset: chopped.state.playerAsset,
      playerXp: chopped.state.playerXp,
      playerLevel: chopped.state.playerLevel,
      trees: chopped.state.trees.map((tree) => ({ id: tree.treeId, health: tree.health })),
      playerLogs: chopped.state.playerLogs,
      playerStone: chopped.state.playerStone,
      playerIronOre: chopped.state.playerIronOre,
      playerAxe: chopped.state.playerAxe,
    }));
  } catch (error) {
    console.error(error);
    if (inspect) {
      try {
        console.error(`last page snapshot: ${JSON.stringify(await inspect())}`);
      } catch (snapshotError) {
        console.error(`page snapshot unavailable: ${snapshotError.message}`);
      }
    }
    await saveScreenshot(DRIVER_URL, sessionId, 'tree-failure.png');
    if (web) console.error(`web process output:\n${web.output()}`);
    if (driver) console.error(`geckodriver output:\n${driver.output()}`);
    process.exitCode = 1;
  } finally {
    if (sessionId) {
      try { await request('DELETE', `/session/${sessionId}`); } catch {}
    }
    await Promise.all([stopProcess(web), stopProcess(driver)]);
  }
}

await main();
