#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import {
  assertPortAvailable,
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

async function request(method, pathName, body) {
  return webdriverRequest(DRIVER_URL, method, pathName, body);
}

function assertLogSupply(state, expected, label) {
  const treeLogs = state.trees.reduce((sum, tree) => sum + tree.logReserveRemaining, 0);
  const total = treeLogs + state.playerLogs;
  assert.equal(total, expected, `${label}: fixed LOG supply is ${total}`);
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
    waitForHttp('http://127.0.0.1:7070/v1/info', 5_000),
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
    const session = await request('POST', '/session', {
      capabilities: {
        alwaysMatch: {
          browserName: 'firefox',
          unhandledPromptBehavior: 'accept',
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
        adjacent: Boolean(globalThis.__WOODLAND_E2E_ADJACENT),
        adjacentTree: globalThis.__WOODLAND_E2E_ADJACENT_TREE || null,
        autoChop: globalThis.__WOODLAND_E2E_LAST_CHOP_RUN || null,
        treeEffects: globalThis.__WOODLAND_E2E_TREE_EFFECTS || [],
        busy: Boolean(document.getElementById('refresh')?.disabled),
        map: document.getElementById('map')?.textContent || '',
        mapHint: document.getElementById('map-hint')?.textContent || '',
        mapCells: document.querySelectorAll('#map .map-cell').length,
        camera: (() => {
          const viewport = document.getElementById('map-viewport');
          const playerCell = document.querySelector('#map .player');
          if (!viewport || !playerCell) return null;
          const viewportBounds = viewport.getBoundingClientRect();
          const playerBounds = playerCell.getBoundingClientRect();
          return {
            target: globalThis.__WOODLAND_E2E_CAMERA || null,
            trail: globalThis.__WOODLAND_E2E_CAMERA_TRAIL || [],
            scrollLeft: viewport.scrollLeft,
            clientHeight: viewport.clientHeight,
            playerCenterX: playerBounds.left + playerBounds.width / 2 - viewportBounds.left,
            playerCenterY: playerBounds.top + playerBounds.height / 2 - viewportBounds.top,
            viewportCenterX: viewportBounds.width / 2,
            viewportCenterY: viewportBounds.height / 2,
          };
        })(),
        bagLogs: document.getElementById('player-logs')?.textContent || '',
        logSlotLabel: document.getElementById('log-slot')?.getAttribute('aria-label') || '',
        logIcon: document.querySelector('#log-slot .bag-item')?.textContent || '',
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
        statsText: document.querySelector('.stats-box')?.textContent.replace(/\\s+/g, ' ').trim() || '',
        standingTreeGlyphs: document.querySelectorAll('#map .map-cell.tree').length,
        stumpGlyphs: document.querySelectorAll('#map .map-cell.stump').length,
        focusedTreeHealth: document.getElementById('tree-health')?.textContent || '',
        chance: document.getElementById('log-chance')?.textContent || '',
        fundingInstruction: document.getElementById('funding-instruction')?.textContent || '',
        status: document.getElementById('status')?.textContent || '',
        log: document.getElementById('log')?.textContent || '',
      };
    `);
    const click = (id) => execute(`document.getElementById(arguments[0]).click();`, [id]);
    const clickMapCell = (x, y) => execute(`
      const cell = document.querySelector(
        '#map .map-cell[data-x=\"' + arguments[0] + '\"][data-y=\"' + arguments[1] + '\"]',
      );
      if (!cell) throw new Error('missing map cell');
      cell.click();
    `, [x, y]);
    const clickTree = (treeId) => execute(`
      const tree = document.querySelector(
        '#map .map-cell[data-tree-id=\"' + arguments[0] + '\"]',
      );
      if (!tree) throw new Error('missing tree cell');
      tree.click();
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
      const treeOutpoints = before.state.trees.map((tree) => tree.treeOutpoint);
      const result = await executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_RENEW_PLAYER()
          .then((state) => done({ state }))
          .catch((error) => done({ error: String(error) }));
      `);
      assert.equal(result.error, undefined, `${label}: ${result.error}`);
      const after = await inspect();
      assert.notEqual(after.state.playerStateOutpoint, before.state.playerStateOutpoint, label);
      assert.ok(
        after.state.playerStateExpiresInSeconds > before.state.playerStateExpiresInSeconds,
        `${label}: expiry did not increase`,
      );
      assert.equal(after.state.playerXp, before.state.playerXp, label);
      assert.equal(after.state.playerLogs, before.state.playerLogs, label);
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
        && candidate.nextDrop === false
        && Math.abs(before.player.x - candidate.x) + Math.abs(before.player.y - candidate.y) > 1
      ));
      assert.ok(tree, `${label}: no distant standing miss-first tree is available`);
      await clickTree(tree.treeId);
      const after = await waitFor(
        label,
        inspect,
        (value) => !value.busy
          && value.autoChop?.treeId === tree.treeId
          && value.autoChop.success
          && value.autoChop.swings > 1
          && value.state?.playerLogs === before.state.playerLogs + 1
          && value.state.playerXp === before.state.playerXp + 1,
        180_000,
      );
      const suffix = after.autoChop.swings === 1 ? 'swing' : 'swings';
      assert.equal(
        after.status,
        `You get a LOG and 1 XP after ${after.autoChop.swings} ${suffix}.`,
      );
      assert.ok(
        after.autoChop.durationMs >= after.autoChop.swings * 800,
        `${label}: swings completed faster than the minimum cadence`,
      );
      const effects = after.treeEffects
        .filter((effect) => effect.treeId === tree.treeId)
        .map((effect) => effect.type);
      assert.ok(effects.includes('chop'), `${label}: chop flash was not emitted`);
      assert.equal(effects.at(-1), 'log', `${label}: LOG flash was not emitted`);
      assert.equal(
        after.state.trees.find((candidate) => candidate.treeId === tree.treeId).health,
        tree.health - 1,
      );
      assert.equal(
        Math.abs(after.player.x - tree.x) + Math.abs(after.player.y - tree.y),
        1,
        `${label}: mouse path did not stop beside the tree`,
      );
      assertLogSupply(after.state, before.state.trees.reduce(
        (sum, candidate) => sum + candidate.logReserveRemaining,
        before.state.playerLogs,
      ), label);
      console.log(
        `continuous chop tree ${tree.treeId}: LOG after ${after.autoChop.swings} swings`,
      );
      return after;
    };

    await wd('POST', '/url', { url: `${WEB_URL}/` });
    const initial = await waitFor(
      'shared forest initialization',
      inspect,
      (value) => value.ready
        && value.state?.address?.startsWith('tark1')
        && value.state?.trees?.length === 10
        && value.state.trees.every((tree) => tree.health === 5)
        && !value.state?.fundingReady
        && !value.state?.playerActive,
    );
    assert.equal(initial.state.mapWidth, 45);
    assert.equal(initial.state.mapHeight, 19);
    assert.equal(initial.mapCells, initial.state.mapWidth * initial.state.mapHeight);
    assert.equal(initial.standingTreeGlyphs, 10);
    assert.equal(initial.stumpGlyphs, 0);
    assert.equal(initial.state.dustSats, 330);
    assert.equal(initial.state.fundingRequiredSats, 330);
    assert.equal(initial.state.fullTreeValueSats, 1_980);
    assert.equal(new Set(initial.state.trees.map((tree) => tree.treeId)).size, 10);
    assert.equal(new Set(initial.state.trees.map((tree) => `${tree.x}:${tree.y}`)).size, 10);
    assert.equal(new Set(initial.state.trees.map((tree) => tree.treeOutpoint)).size, 10);
    assert.ok(initial.state.trees.every((tree) => tree.logReserveRemaining === 10));
    assert.ok(initial.state.trees.every((tree) => tree.xpRemaining === 10));
    assert.equal(initial.state.playerLogs, 0);
    assert.equal(initial.state.fundingRequiredSats, initial.state.dustSats);
    assert.equal(initial.state.playerXp, 0);
    assert.equal(initial.state.playerLevel, 1);
    assert.equal(initial.state.playerNextLevelXp, 83);
    assert.equal(initial.state.seasonXpRemaining, 100);
    assert.equal(initial.state.activationReady, false);
    assert.equal(initial.state.activationBlockedReason ?? null, null);
    assert.equal(initial.state.playerAsset, null);
    assert.equal(initial.state.logDropBasisPoints, 1_000);
    assert.equal(initial.chance, '10%');
    assert.ok(initial.state.xpAsset);
    assert.equal(
      new Set([
        initial.state.treeAsset,
        initial.state.logAsset,
        initial.state.xpAsset,
      ]).size,
      3,
    );
    assertLogSupply(initial.state, 100, 'initial world');
    assertXpAccounting(initial.state, 100, 'initial world');
    assertTreeValue(initial.state, 'initial world');
    assert.equal(await execute(`return document.getElementById('create-tree');`), null);
    assert.equal(await execute(`return document.getElementById('sell');`), null);
    assert.equal(await execute(`return document.title;`), 'woodland.sh');
    assert.equal(await execute(`return document.querySelector('h1')?.textContent;`), 'woodland.sh');
    const positionAfterKeyboard = await execute(`
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'd', bubbles: true }));
      return globalThis.__WOODLAND_E2E_PLAYER;
    `);
    assert.deepEqual(positionAfterKeyboard, initial.player);
    console.log(`shared woodland ready: ${initial.state.trees.length} trees`);
    console.log(`player wallet ready: ${initial.state.address}`);
    await clickMapCell(4, 17);
    await waitFor(
      'inactive mouse movement',
      inspect,
      (value) => !value.state?.playerActive && value.player?.x === 4 && value.player?.y === 17,
    );
    await clickMapCell(3, 17);
    await waitFor(
      'inactive mouse return',
      inspect,
      (value) => !value.state?.playerActive && value.player?.x === 3 && value.player?.y === 17,
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
      (value) => !value.state?.fundingReady
        && value.state.walletSats === initial.state.fundingRequiredSats
        && value.state.fundingRequiredSats === 0
        && value.state.activationReady
        && value.fundingInstruction === 'No additional player funding required',
    );
    await click('activate');
    await waitFor(
      'recursive player activation',
      inspect,
      (value) => value.state?.fundingReady
        && value.state.playerActive
        && Boolean(value.state.playerAsset)
        && value.state.playerXp === 0
        && value.state.playerLevel === 1
        && value.state.playerNextLevelXp === 83
        && !value.state.activationReady
        && value.state.logDropBasisPoints === 1_000
        && value.state.walletSats === 330
        && value.state.fundingRequiredSats === 0,
      180_000,
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
      ]).size,
      4,
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
    const playerAssetMetadata = Buffer.from(playerAssetInfo.metadata, 'hex');
    for (const token of ['game', 'woodland.sh', 'protocol', '1', 'asset', 'PLAYER_ID', 'owner']) {
      assert.ok(
        playerAssetMetadata.includes(Buffer.from(token)),
        `PLAYER_ID metadata omits ${token}`,
      );
    }
    assert.ok(deployed.state.playerStateExpiresInSeconds > 300);
    assert.ok(deployed.state.trees.every((tree) => tree.expiresInSeconds > 300));
    assert.deepEqual(
      deployed.state.trees.map((tree) => tree.treeOutpoint),
      initial.state.trees.map((tree) => tree.treeOutpoint),
    );
    assert.equal(deployed.state.playerLogs, 0);
    assert.equal(deployed.bagLogs, '0');
    assert.equal(deployed.logSlotLabel, '0 LOG in inventory');
    assert.equal(deployed.logIcon, '🪵');
    assert.equal(deployed.inventorySlots, 9);
    assert.equal(deployed.emptySlots, 8);
    assert.equal(deployed.inventoryRows, 3);
    assert.equal(deployed.bagWidth, 262);
    assert.equal(deployed.bagStats, 0);
    assert.equal(deployed.statsRightOfBag, true);
    assert.match(deployed.statsText, /Level 1/);
    assert.match(deployed.statsText, /0 XP/);
    assert.match(deployed.statsText, /LOG drop chance 10%/);
    assertLogSupply(deployed.state, 100, 'activated player');
    assertXpAccounting(deployed.state, 100, 'activated player');
    assertTreeValue(deployed.state, 'activated player');
    deployed = await assertPlayerRenewed(deployed, 'zero-XP player renewal');
    const playerStateBeforeBrowserReload = deployed.state.playerStateOutpoint;
    const fixedSats = deployed.state.trees.reduce(
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
        && value.camera.scrollLeft > 200
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
        && value.map.includes('@')
        && value.map.includes('🌲'),
    );
    await wd('POST', '/refresh', {});
    await waitFor(
      'saved player and map position after browser reload',
      inspect,
      (value) => value.ready
        && value.state?.playerActive
        && value.state.activationBlockedReason == null
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
      ['wrong-log-delta', 'LOG delta inconsistent with the roll must be rejected'],
      ['wrong-xp-delta', 'XP transfer inconsistent with the roll must be rejected'],
      ['asset-metadata', 'transfer metadata mutation must be rejected'],
      ['player-marker-metadata', 'PLAYER_ID transfer metadata must be rejected'],
      ['asset-control', 'transfer control-asset mutation must be rejected'],
      ['noncanonical-xp-zero', 'negative-zero XP encoding must be rejected'],
      ['double-tree-marker', 'TREE marker inflation must be rejected'],
      ['wrong-player-position', 'player position mutation during chop must be rejected'],
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
    while (hits < TARGET_HITS) {
      attempts += 1;
      assert.ok(
        attempts <= (FULL_E2E ? 40 : 6),
        `${TARGET_HITS} LOG drop(s) exceeded the deterministic swing bound`,
      );
      const before = chopped.state;
      const beforeTree = before.trees.find(
        (tree) => tree.treeId === firstTree.treeId,
      );
      const previousTreeOutpoint = beforeTree.treeOutpoint;
      assert.ok(Number.isInteger(beforeTree.nextRollBucket));
      assert.ok(beforeTree.nextRollBucket >= 0 && beforeTree.nextRollBucket < 10_000);
      assert.equal(beforeTree.nextDrop, beforeTree.nextRollBucket < before.logDropBasisPoints);
      if (attempts === 6) {
        chopped = await assertInvalidXpRejected(
          firstTree.treeId,
          'missing XP counter increment on a LOG drop must be rejected',
        );
        chopped = await assertChopMutationRejected(
          'wrong-xp-delta',
          firstTree.treeId,
          'a successful chop without exactly one XP transfer must be rejected',
        );
      }
      if (FULL_E2E && attempts === 7) {
        chopped = await assertInvalidXpRejected(
          firstTree.treeId,
          'XP counter increment on a nonzero-XP miss must be rejected',
        );
      }
      if (FULL_E2E && attempts === 21) {
        chopped = await assertInvalidXpRejected(
          firstTree.treeId,
          'missing XP counter increment on a nonzero-XP LOG drop must be rejected',
        );
      }
      if (FULL_E2E && attempts === 37) {
        chopped = await assertChopMutationRejected(
          'noncanonical-health-zero',
          firstTree.treeId,
          'negative-zero stump health must be rejected',
        );
      }
      const result = await expectedChop(before, beforeTree);
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
      if (success) hits += 1;
      else sawMiss = true;
      const current = chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId);
      assert.equal(current.health, 5 - hits);
      assert.equal(
        current.logReserveRemaining,
        beforeTree.logReserveRemaining - Number(success),
      );
      assert.equal(
        current.xpRemaining,
        beforeTree.xpRemaining - Number(success),
      );
      assert.equal(chopped.state.playerLogs, hits);
      assert.equal(chopped.state.playerXp, before.playerXp + Number(success));
      assert.equal(chopped.state.playerXp, hits);
      assert.equal(chopped.state.playerLevel, 1);
      assert.equal(chopped.state.playerNextLevelXp, 83);
      assert.equal(chopped.state.logDropBasisPoints, 1_000);
      assert.equal(chopped.state.lastAttempt.treeId, firstTree.treeId);
      assert.equal(chopped.state.playerStateOutpoint === before.playerStateOutpoint, false);
      for (const tree of chopped.state.trees.filter((tree) => tree.treeId !== firstTree.treeId)) {
        assert.equal(tree.treeOutpoint, initialOutpoints.get(tree.treeId));
      }
      assert.equal(chopped.state.walletSats, chopped.state.dustSats);
      assert.equal(current.valueSats, chopped.state.fullTreeValueSats);
      assertLogSupply(chopped.state, 100, `swing ${attempts}`);
      assertXpAccounting(chopped.state, 100, `swing ${attempts}`);
      assertTreeValue(chopped.state, `swing ${attempts}`);
      assertFixedSats(chopped.state, fixedSats, `swing ${attempts}`);
      console.log(
        `tree ${firstTree.treeId} swing ${attempts}: ${success ? 'LOG' : 'miss'} ${current.lastAttemptTxid}`,
      );
    }
    assert.equal(
      attempts,
      FULL_E2E ? 37 : 6,
      `clean tree 417 ${E2E_PROFILE} roll sequence changed`,
    );
    assert.ok(sawMiss, 'deterministic sequence exercises at least one miss');
    chopped = await assertPlayerRenewed(chopped, 'nonzero-XP player renewal');
    if (!FULL_E2E) {
      assert.equal(chopped.state.playerXp, 1);
      assert.equal(chopped.state.playerLogs, 1);
      assert.equal(
        chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId).health,
        4,
      );
      chopped = await assertAutoChopUntilLog(chopped, 'continuous chopping UX');
      assert.equal(chopped.bagLogs, String(chopped.state.playerLogs));
      assert.equal(
        chopped.logSlotLabel,
        `${chopped.state.playerLogs} LOG in inventory`,
      );
      console.log(JSON.stringify({
        profile: E2E_PROFILE,
        address: chopped.state.address,
        treeId: firstTree.treeId,
        rollAdvances: attempts,
        playerXp: chopped.state.playerXp,
        playerLogs: chopped.state.playerLogs,
      }));
      return;
    }
    assert.ok(chopped.map.includes('+'));
    assert.equal(chopped.standingTreeGlyphs, 9);
    assert.equal(chopped.stumpGlyphs, 1);
    assert.equal(chopped.focusedTreeHealth, 'stump');
    assert.equal(chopped.bagLogs, '5');
    assert.equal(chopped.logSlotLabel, '5 LOG in inventory');
    const stumpXp = hits;

    const stump = chopped.state.trees.find(
      (tree) => tree.treeId === firstTree.treeId,
    );
    assert.ok(stump.respawnInSeconds > 0 && stump.respawnInSeconds <= 40);
    assert.equal(stump.logReserveRemaining, 5);
    assert.equal(stump.xpRemaining, 5);
    const stumpOutpoint = stump.treeOutpoint;
    await sleep(1_500);
    const waiting = await inspect();
    assert.equal(
      waiting.state.trees.find((tree) => tree.treeId === firstTree.treeId).health,
      0,
      'maintenance must not regrow before the deadline',
    );
    const playerStateBeforeRegrow = chopped.state.playerStateOutpoint;
    chopped = await waitFor(
      'staggered automatic stump respawn',
      inspect,
      (value) => !value.busy
        && value.state?.trees?.find((tree) => tree.treeId === firstTree.treeId)?.health === 5
        && value.state.playerLogs === 5
        && value.state.playerXp === stumpXp,
      90_000,
    );
    assert.equal(chopped.state.walletSats, 330);
    assert.equal(chopped.state.walletVtxos.length, 0);
    assert.equal(chopped.state.playerStateOutpoint, playerStateBeforeRegrow);
    assert.notEqual(
      chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId).treeOutpoint,
      stumpOutpoint,
    );
    const regrownTree = chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId);
    assert.equal(regrownTree.logReserveRemaining, 5);
    assert.equal(regrownTree.xpRemaining, 5);
    assertLogSupply(chopped.state, 100, 'regrown tree');
    assertXpAccounting(chopped.state, 100, 'regrown tree');
    assertTreeValue(chopped.state, 'regrown tree');
    assertFixedSats(chopped.state, fixedSats, 'regrown tree');
    await wd('POST', '/refresh', {});
    chopped = await waitFor(
      'player XP and regrown tree reconstruction after reload',
      inspect,
      (value) => value.ready
        && value.state?.playerActive
        && value.state.playerXp === stumpXp
        && value.state.playerLevel === 1
        && value.state.playerNextLevelXp === 83
        && value.state.logDropBasisPoints === 1_000
        && value.state.playerLogs === 5
        && value.state?.trees?.find((tree) => tree.treeId === firstTree.treeId)?.health === 5,
    );
    assertLogSupply(chopped.state, 100, 'post-regrowth reload');
    assertXpAccounting(chopped.state, 100, 'post-regrowth reload');
    assertTreeValue(chopped.state, 'post-regrowth reload');
    assertFixedSats(chopped.state, fixedSats, 'post-regrowth reload');
    let depletionAttempts = 0;
    let depletionHits = 0;
    while (
      chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId).health > 0
    ) {
      depletionAttempts += 1;
      assert.ok(depletionAttempts <= 40, 'second tree-417 harvest exceeded its swing bound');
      const before = chopped.state;
      const beforeTree = before.trees.find((tree) => tree.treeId === firstTree.treeId);
      const previousTreeOutpoint = beforeTree.treeOutpoint;
      const result = await expectedChop(before, beforeTree);
      assert.equal(result.ok, true, result.message);
      chopped = await waitFor(
        `reserve depletion swing ${depletionAttempts}`,
        inspect,
        (value) => !value.busy
          && value.state?.trees?.find((tree) => tree.treeId === firstTree.treeId)?.treeOutpoint
            !== previousTreeOutpoint,
        180_000,
      );
      const success = chopped.state.lastAttempt?.success === true;
      assert.equal(success, beforeTree.nextDrop, 'depletion roll prediction must be exact');
      if (success) depletionHits += 1;
      const current = chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId);
      assert.equal(current.health, 5 - depletionHits);
      assert.equal(current.logReserveRemaining, 5 - depletionHits);
      assert.equal(current.xpRemaining, 5 - depletionHits);
      assert.equal(chopped.state.playerLogs, stumpXp + depletionHits);
      assert.equal(chopped.state.playerXp, stumpXp + depletionHits);
      assert.equal(chopped.state.playerLevel, 1);
      assert.equal(chopped.state.playerNextLevelXp, 83);
      assertLogSupply(chopped.state, 100, `reserve depletion swing ${depletionAttempts}`);
      assertXpAccounting(chopped.state, 100, `reserve depletion swing ${depletionAttempts}`);
      assertTreeValue(chopped.state, `reserve depletion swing ${depletionAttempts}`);
      assertFixedSats(chopped.state, fixedSats, `reserve depletion swing ${depletionAttempts}`);
    }
    assert.equal(depletionAttempts, 32, 'tree 417 second deterministic harvest changed');
    assert.equal(depletionHits, 5);
    const exhaustedXp = stumpXp + depletionHits;
    const exhaustedLogs = chopped.state.playerLogs;
    assert.equal(exhaustedXp, 10);
    assert.equal(exhaustedLogs, 10);
    const exhaustedTree = chopped.state.trees.find(
      (tree) => tree.treeId === firstTree.treeId,
    );
    assert.equal(exhaustedTree.health, 0);
    assert.equal(exhaustedTree.logReserveRemaining, 0);
    assert.equal(exhaustedTree.xpRemaining, 0);
    assert.equal(exhaustedTree.nextDrop, false);
    assert.equal(exhaustedTree.respawnAt, null);
    assert.equal(exhaustedTree.respawnInSeconds, null);
    const exhaustedOutpoint = exhaustedTree.treeOutpoint;
    await sleep(45_000);
    chopped = await inspect();
    const permanentlyDepletedTree = chopped.state.trees.find(
      (tree) => tree.treeId === firstTree.treeId,
    );
    assert.equal(permanentlyDepletedTree.treeOutpoint, exhaustedOutpoint);
    assert.equal(permanentlyDepletedTree.health, 0);
    assert.equal(permanentlyDepletedTree.logReserveRemaining, 0);
    assert.equal(permanentlyDepletedTree.xpRemaining, 0);
    assert.equal(permanentlyDepletedTree.respawnAt, null);
    assert.equal(permanentlyDepletedTree.respawnInSeconds, null);
    assert.equal(chopped.state.playerXp, exhaustedXp);
    assert.match(chopped.mapHint, /exhausted its LOG and XP reserve/);
    assert.equal(chopped.state.playerLogs, exhaustedLogs);
    assertLogSupply(chopped.state, 100, 'permanently depleted tree');
    assertXpAccounting(chopped.state, 100, 'permanently depleted tree');
    assertTreeValue(chopped.state, 'permanently depleted tree');
    assertFixedSats(chopped.state, fixedSats, 'permanently depleted tree');


    const secondTree = chopped.state.trees.find((tree) => tree.treeId === 426);
    assert.ok(secondTree, 'deterministic tree 426 is unavailable');
    const beforeSecondOutpoints = new Map(
      chopped.state.trees.map((tree) => [tree.treeId, tree.treeOutpoint]),
    );
    await clickMapCell(secondTree.x, secondTree.y + 1);
    await waitFor(
      `player movement next to tree ${secondTree.treeId}`,
      inspect,
      (value) => value.adjacentTree?.treeId === secondTree.treeId,
    );
    let secondAttempts = 0;
    while (chopped.state.playerLogs === exhaustedLogs) {
      secondAttempts += 1;
      assert.ok(secondAttempts <= 15, 'second tree succeeds within 15 swings');
      const beforeTree = chopped.state.trees.find(
        (tree) => tree.treeId === secondTree.treeId,
      );
      const previous = beforeTree.treeOutpoint;
      const previousStateOutpoint = chopped.state.playerStateOutpoint;
      const result = await expectedChop(chopped.state, beforeTree);
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
    assert.equal(secondAttempts, 2, 'clean tree 426 roll must first drop LOG on swing 2');
    assert.equal(chopped.state.trees.find((tree) => tree.treeId === secondTree.treeId).health, 4);
    assert.equal(chopped.state.trees.find((tree) => tree.treeId === firstTree.treeId).health, 0);
    assert.equal(chopped.state.playerLogs, exhaustedLogs + 1);
    assert.equal(chopped.state.playerXp, exhaustedXp + 1);
    assert.equal(chopped.state.playerLevel, 1);
    assert.equal(chopped.state.playerNextLevelXp, 83);
    assert.equal(chopped.state.logDropBasisPoints, 1_000);
    for (const tree of chopped.state.trees.filter((tree) => tree.treeId !== secondTree.treeId)) {
      assert.equal(tree.treeOutpoint, beforeSecondOutpoints.get(tree.treeId));
    }
    assert.equal(chopped.state.walletSats, 330);
    assertLogSupply(chopped.state, 100, 'second tree chop');
    assertXpAccounting(chopped.state, 100, 'second tree chop');
    assertTreeValue(chopped.state, 'second tree chop');
    assertFixedSats(chopped.state, fixedSats, 'second tree chop');
    await wd('POST', '/refresh', {});
    chopped = await waitFor(
      'ten-tree reconstruction after final reload',
      inspect,
      (value) => value.ready
        && value.state?.trees?.length === 10
        && value.state.playerActive
        && value.state.playerXp === exhaustedXp + 1
        && value.state.playerLevel === 1
        && value.state.playerNextLevelXp === 83
        && value.state.logDropBasisPoints === 1_000
        && value.state.playerLogs === exhaustedLogs + 1
        && value.state.trees.find((tree) => tree.treeId === firstTree.treeId)?.health === 0
        && value.state.trees.find((tree) => tree.treeId === secondTree.treeId)?.health === 4,
    );
    assertLogSupply(chopped.state, 100, 'final reload');
    assertXpAccounting(chopped.state, 100, 'final reload');
    assert.equal(chopped.state.playerAsset, playerAsset);
    assertTreeValue(chopped.state, 'final reload');
    assertFixedSats(chopped.state, fixedSats, 'final reload');
    chopped = await assertAutoChopUntilLog(chopped, 'continuous chopping UX after full profile');
    console.log(JSON.stringify({
      profile: E2E_PROFILE,
      address: chopped.state.address,
      playerFundingSats: initial.state.fundingRequiredSats,
      treeAsset: chopped.state.treeAsset,
      logAsset: chopped.state.logAsset,
      xpAsset: chopped.state.xpAsset,
      playerAsset: chopped.state.playerAsset,
      playerXp: chopped.state.playerXp,
      playerLevel: chopped.state.playerLevel,
      trees: chopped.state.trees.map((tree) => ({ id: tree.treeId, health: tree.health })),
      playerLogs: chopped.state.playerLogs,
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
