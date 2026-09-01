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
const PLAYER_COUNT = FULL_E2E ? 4 : 2;
const DRIVER_CONFIGS = Array.from({ length: PLAYER_COUNT }, (_, index) => ({
  port: DRIVER_PORT + index,
  websocketPort: DRIVER_PORT + PLAYER_COUNT + index,
}));
const EXTERNAL_WEB_URL = process.env.WOODLAND_E2E_WEB_URL?.replace(/\/$/, '');
const WEB_URL = EXTERNAL_WEB_URL || `http://127.0.0.1:${WEB_PORT}`;
const SERVER_URL = process.env.WOODLAND_SERVER_URL?.replace(/\/$/, '')
  || 'http://127.0.0.1:8090';

async function request(driverUrl, method, pathName, body, timeoutMs = 130_000) {
  return webdriverRequest(driverUrl, method, pathName, body, timeoutMs);
}

async function createPlayer(driverUrl, label, sessions) {
  const session = await request(driverUrl, 'POST', '/session', {
    capabilities: {
      alwaysMatch: {
        browserName: 'firefox',
        unhandledPromptBehavior: 'accept',
        'moz:firefoxOptions': { args: ['-headless'] },
      },
    },
  });
  assert.ok(session.sessionId, `${label} WebDriver session has no ID`);
  const sessionId = session.sessionId;
  const wd = (method, suffix, body) => (
    request(driverUrl, method, `/session/${sessionId}${suffix}`, body)
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
      fundingInstruction: document.getElementById('funding-instruction')?.textContent || '',
      status: document.getElementById('status')?.textContent || '',
      log: document.getElementById('log')?.textContent || '',
      leaderboard: globalThis.__WOODLAND_E2E_LEADERBOARD || [],
      leaderboardStatus: document.getElementById('leaderboard-status')?.textContent || '',
      serverRegistered: Boolean(globalThis.__WOODLAND_E2E_SERVER_REGISTERED),
      social: globalThis.__WOODLAND_E2E_SOCIAL || null,
      mapFrame: globalThis.__WOODLAND_E2E_MAP_FRAME || null,
      remotePlayers: globalThis.__WOODLAND_E2E_MAP_FRAME?.remotePlayerCount || 0,
      chat: document.getElementById('chat-messages')?.textContent || '',
      delegateHidden: document.getElementById('delegate-renewal')?.hidden ?? true,
      delegateText: document.getElementById('delegate-renewal')?.textContent || '',
    };
  `);
  const click = (id) => execute(`
    const button = document.getElementById(arguments[0]);
    if (!button) throw new Error('missing button ' + arguments[0]);
    if (button.disabled) throw new Error('disabled button ' + arguments[0]);
    button.click();
  `, [id]);
  const moveTo = (x, y) => execute(`
    if (!globalThis.__WOODLAND_E2E_CLICK_MAP) throw new Error('missing canvas map hook');
    globalThis.__WOODLAND_E2E_CLICK_MAP(arguments[0], arguments[1]);
  `, [x, y]);
  const chopExpected = (snapshot, tree) => executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_CHOP_EXPECTED(...Array.from(arguments).slice(0, -1)).then(done);
  `, [
    tree.treeId,
    tree.treeOutpoint,
    snapshot.playerStateOutpoint,
    tree.nextDrop,
  ]);
  const player = {
    label,
    driverUrl,
    sessionId,
    wd,
    execute,
    executeAsync,
    inspect,
    click,
    moveTo,
    chopExpected,
  };
  sessions.push(player);
  await wd('POST', '/timeouts', { implicit: 0, pageLoad: 30_000, script: 120_000 });
  return player;
}

async function refreshPlayers(players, label, accept) {
  await Promise.all(players.map((player) => player.click('refresh')));
  return Promise.all(players.map((player, index) => waitFor(
    `${label} (${player.label})`,
    player.inspect,
    (value) => value.ready && !value.busy && accept(value, index),
    180_000,
  )));
}

async function refreshWorldPlayers(players, label, accept) {
  await Promise.all(players.map((player) => player.executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_REFRESH_WORLD().then(done);
  `)));
  return Promise.all(players.map((player, index) => waitFor(
    `${label} (${player.label})`,
    player.inspect,
    (value) => value.ready && !value.busy && accept(value, index),
    180_000,
  )));
}

function treeProjection(state) {
  return state.trees.map((tree) => ({
    treeId: tree.treeId,
    x: tree.x,
    y: tree.y,
    health: tree.health,
    logReserveRemaining: tree.logReserveRemaining,
    xpRemaining: tree.xpRemaining,
    valueSats: tree.valueSats,
    treeOutpoint: tree.treeOutpoint,
    lastAttemptTxid: tree.lastAttemptTxid,
  }));
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

function assertSharedWorld(views, label) {
  const shared = views[0].state;
  for (const view of views.slice(1)) {
    assert.equal(view.state.treeAsset, shared.treeAsset, `${label}: TREE asset differs`);
    assert.equal(view.state.logAsset, shared.logAsset, `${label}: LOG asset differs`);
    assert.equal(
      view.state.xpAsset,
      shared.xpAsset,
      `${label}: XP asset differs`,
    );
    assert.deepEqual(treeProjection(view.state), treeProjection(shared), `${label}: tree state differs`);
  }
  return shared;
}

function assertPlayersActive(views, label) {
  for (const view of views) {
    assert.equal(view.state.playerActive, true, `${label}: player is inactive`);
    assert.equal(view.state.fundingReady, true, `${label}: player state is unavailable`);
  }
}

async function assertLeaderboardMatches(views, label) {
  const expected = new Map(views.map((view) => [
    view.state.playerAsset,
    { xp: view.state.playerXp, logs: view.state.playerLogs },
  ]));
  const payload = await waitFor(
    label,
    async () => {
      const response = await fetch(`${SERVER_URL}/v1/leaderboard`, { cache: 'no-store' });
      if (!response.ok) throw new Error(`leaderboard returned ${response.status}`);
      return response.json();
    },
    (value) => [...expected.entries()].every(([playerAsset, score]) => {
      const entry = value.players?.find((candidate) => candidate.playerAsset === playerAsset);
      return entry?.active === true && entry.xp === score.xp && entry.logs === score.logs;
    }),
    180_000,
  );
  assert.ok(payload.players.length >= expected.size, `${label}: missing verified players`);
  return payload;
}

async function assertSubmissionRecovery(player, view, treeId) {
  const beforeTree = view.state.trees.find((tree) => tree.treeId === treeId);
  const result = await player.executeAsync(`
    const done = arguments[arguments.length - 1];
    globalThis.__WOODLAND_E2E_SUBMISSION_RECOVERY(arguments[0])
      .then((state) => done({ state }))
      .catch(async (failure) => {
        try {
          const state = await globalThis.__WOODLAND_E2E_REFRESH();
          done({ failure: String(failure), state });
        } catch (refreshFailure) {
          done({ failure: String(failure), refreshFailure: String(refreshFailure) });
        }
      });
  `, [treeId]);
  assert.equal(result.refreshFailure, undefined, result.refreshFailure);
  assert.equal(result.state.pendingChopTxid ?? null, null);
  if (!result.failure) {
    assert.notEqual(result.state.playerStateOutpoint, view.state.playerStateOutpoint);
    assert.notEqual(
      result.state.trees.find((tree) => tree.treeId === treeId).treeOutpoint,
      beforeTree.treeOutpoint,
    );
    return result.state;
  }

  assert.match(
    result.failure,
    /chop conflicted while recovering from submission failure/,
    result.failure,
  );
  assert.equal(result.state.playerStateOutpoint, view.state.playerStateOutpoint);
  const refreshedTree = result.state.trees.find((tree) => tree.treeId === treeId);
  assert.equal(refreshedTree.treeOutpoint, beforeTree.treeOutpoint);
  const retry = await player.chopExpected(result.state, refreshedTree);
  assert.equal(retry.ok, true, retry.message);
  const recovered = await waitFor(
    `fresh swing after stale pending recovery (${player.label})`,
    player.inspect,
    (next) => !next.busy
      && next.state?.pendingChopTxid == null
      && next.state.playerStateOutpoint !== view.state.playerStateOutpoint
      && next.state.trees.find((tree) => tree.treeId === treeId)?.treeOutpoint
        !== beforeTree.treeOutpoint,
    180_000,
  );
  return recovered.state;
}

async function main() {
  const driverUrls = DRIVER_CONFIGS.map(({ port }) => `http://127.0.0.1:${port}`);
  await Promise.all([
    waitForHttp('http://127.0.0.1:7070/v1/info', 5_000),
    waitForHttp('http://127.0.0.1:7073/v1/info', 5_000),
    waitForHttp(`${SERVER_URL}/health.json`, 5_000),
    ...(EXTERNAL_WEB_URL ? [] : [assertPortAvailable(WEB_PORT, 'web server')]),
    ...DRIVER_CONFIGS.flatMap(({ port, websocketPort }, index) => [
      assertPortAvailable(port, `WebDriver ${index + 1}`),
      assertPortAvailable(websocketPort, `WebDriver ${index + 1} WebSocket`),
    ]),
  ]);
  const web = EXTERNAL_WEB_URL
    ? null
    : startProcess('node', ['scripts/dev-server.mjs', '--port', String(WEB_PORT)], ROOT);
  const drivers = [];
  const sessions = [];

  try {
    for (const [index, { port, websocketPort }] of DRIVER_CONFIGS.entries()) {
      drivers.push(await startGeckodriver(
        ['--port', String(port), '--websocket-port', String(websocketPort)],
        ROOT,
        `${driverUrls[index]}/status`,
        `WebDriver on port ${port}`,
      ));
    }
    await Promise.all([
      waitForHttp(`${WEB_URL}/`, 20_000, web),
      waitForHttp(`${WEB_URL}/health.json`, 30_000, web),
    ]);
    const manifestResponse = await fetch(`${WEB_URL}/world.json`);
    assert.equal(manifestResponse.ok, true, 'world manifest is unavailable');
    const manifest = await manifestResponse.json();
    assert.equal(manifest.woodcuttingXpPerLog, 25);
    const players = [];
    for (const [index, driverUrl] of driverUrls.entries()) {
      players.push(await createPlayer(driverUrl, `player ${index + 1}`, sessions));
    }

    await Promise.all(players.map((player) => (
      player.wd('POST', '/url', { url: `${WEB_URL}/` })
    )));
    const initial = await Promise.all(players.map((player) => waitFor(
      `shared world initialization (${player.label})`,
      player.inspect,
      (value) => value.ready
        && !value.busy
        && value.state?.address?.startsWith('tark1')
        && value.state?.trees?.length === manifest.trees.length
        && !value.state.playerActive
        && !value.state.fundingReady,
      300_000,
    )));

    assert.notEqual(initial[0].state.address, initial[1].state.address);
    const initialShared = assertSharedWorld(initial, 'initial shared world');
    assert.equal(initialShared.fullTreeValueSats, 330);
    assert.ok(
      initialShared.trees.every((tree) => tree.health >= 0 && tree.health <= 10),
      'shared world contains invalid tree health',
    );
    assert.ok(
      initialShared.trees.filter((tree) => tree.health === 10).length >= 2,
      'shared world has fewer than two full trees',
    );
    for (const view of initial) {
      assert.equal(view.state.playerLogs, 0);
      assert.equal(view.state.playerXp, 0);
      assert.equal(view.state.woodcuttingXpPerLog, manifest.woodcuttingXpPerLog);
      assert.equal(view.state.playerLevel, 1);
      assert.equal(view.state.playerNextLevelXp, 83);
      assert.equal(view.state.logDropBasisPoints, manifest.baseLogDropBasisPoints);
      assert.equal(view.state.playerAsset, null);
      assert.equal(view.state.fundingRequiredSats, 330);
      assert.equal(
        view.fundingInstruction,
        `Deposit ${view.state.fundingRequiredSats} sats to the Arkade address above`,
      );
    }
    assertTreeValue(initialShared, 'initial multiplayer state');

    const activationAmounts = initial.map((view) => view.state.fundingRequiredSats);
    for (const [index, view] of initial.entries()) {
      const funding = execFileSync(
        path.join(ROOT, 'scripts/regtest.sh'),
        ['fund', view.state.address, String(activationAmounts[index])],
        { cwd: ROOT, stdio: 'pipe', encoding: 'utf8' },
      );
      console.log(`funded ${view.state.address}: ${funding.trim()}`);
    }
    await refreshPlayers(
      players,
      'player activation funding',
      (value, index) => !value.state?.fundingReady
        && !value.state?.playerActive
        && value.state?.walletSats === activationAmounts[index],
    );

    await Promise.all(players.map((player) => player.click('activate')));
    await Promise.all(players.map((player, index) => waitFor(
      `concurrent recursive activation (${player.label})`,
      player.inspect,
      (value) => !value.busy
        && value.state?.fundingReady
        && value.state.playerActive
        && Boolean(value.state.playerAsset)
        && value.state.playerXp === 0
        && value.state.playerLevel === 1
        && value.state.playerNextLevelXp === 83
        && value.state.logDropBasisPoints === manifest.baseLogDropBasisPoints
        && value.state.playerLogs === 0
        && value.state.walletSats === activationAmounts[index]
        && value.state.fundingRequiredSats === 0,
      180_000,
    )));
    let activated = await refreshPlayers(
      players,
      'activated player synchronization',
      (value) => value.state?.fundingReady
        && value.state.playerActive
        && value.state.playerXp === 0
        && value.state.playerLogs === 0,
    );
    assertPlayersActive(activated, 'post-activation state');
    assert.notEqual(activated[0].state.playerStateOutpoint, activated[1].state.playerStateOutpoint);
    const playerAssets = activated.map((view) => view.state.playerAsset);
    assert.ok(playerAssets.every(Boolean));
    assert.equal(new Set(playerAssets).size, players.length);
    assertTreeValue(activated[0].state, 'post-activation state');

    const forgedRegistration = await players[0].executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_SERVER_REGISTRATION().then(done);
    `);
    forgedRegistration.signature = `${forgedRegistration.signature.slice(0, -1)}${
      forgedRegistration.signature.endsWith('0') ? '1' : '0'
    }`;
    const forgedResponse = await fetch(`${SERVER_URL}/v1/players`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(forgedRegistration),
    });
    assert.equal(forgedResponse.status, 400, 'forged server consent was accepted');

    const joined = await Promise.all(players.map((player) => waitFor(
      `automatic server registration (${player.label})`,
      player.inspect,
      (value) => value.serverRegistered
        && value.leaderboard.length >= PLAYER_COUNT
        && value.leaderboardStatus.includes('verified'),
      180_000,
    )));
    assert.ok(joined.every((view) => view.leaderboard.length >= PLAYER_COUNT));
    await assertLeaderboardMatches(activated, 'verified activation leaderboard');

    const socialJoined = await Promise.all(players.map((player) => waitFor(
      `authenticated presence (${player.label})`,
      player.inspect,
      (value) => value.social?.delegationAvailable === true
        && value.social.locations?.length >= PLAYER_COUNT,
      180_000,
    )));
    const forgedLocation = await players[0].executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_SERVER_LOCATION(arguments[0], arguments[1]).then(done);
    `, [socialJoined[0].player.x, socialJoined[0].player.y]);
    forgedLocation.x = (forgedLocation.x + 1) % socialJoined[0].state.mapWidth;
    const forgedLocationResponse = await fetch(`${SERVER_URL}/v1/location`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(forgedLocation),
    });
    assert.equal(forgedLocationResponse.status, 400, 'forged player location was accepted');

    const chatText = `hello from ${playerAssets[0].slice(0, 8)}`;
    await players[0].execute(`
      const input = document.getElementById('chat-input');
      input.value = arguments[0];
      input.dispatchEvent(new Event('input', { bubbles: true }));
      document.getElementById('chat-form').requestSubmit();
    `, [chatText]);
    await Promise.all(players.map((player) => waitFor(
      `authenticated chat (${player.label})`,
      player.inspect,
      (value) => value.chat.includes(chatText)
        && value.social?.messages?.some((message) => message.message === chatText),
      180_000,
    )));

    const delegatedInput = socialJoined[0].state.playerStateOutpoint;
    await players[0].click('delegate-renewal');
    const delegated = await waitFor(
      'delegated player renewal',
      players[0].inspect,
      (value) => value.delegateText.startsWith('Stop delegated')
        && value.social?.delegatedPlayerAssets?.includes(playerAssets[0])
        && value.state?.playerStateOutpoint !== delegatedInput
        && value.state.playerXp === 0
        && value.state.playerLogs === 0,
      180_000,
    );
    assert.equal(delegated.delegateHidden, false);
    activated = await refreshPlayers(
      players,
      'delegated renewal synchronization',
      (value, index) => value.state?.playerActive
        && value.state.playerAsset === playerAssets[index]
        && value.state.playerXp === 0
        && value.state.playerLogs === 0,
    );

    await Promise.all(players.map((player) => player.wd('POST', '/refresh', {})));
    const restoredOptIns = await Promise.all(players.map((player, index) => waitFor(
      `automatic server registration reload (${player.label})`,
      player.inspect,
      (value) => value.ready
        && !value.busy
        && value.state?.playerAsset === playerAssets[index]
        && value.serverRegistered
        && value.leaderboard.some((entry) => entry.playerAsset === playerAssets[index])
        && (index !== 0 || value.delegateText.startsWith('Stop delegated')),
      180_000,
    )));
    assert.ok(restoredOptIns.every((view) => view.serverRegistered));
    await players[0].click('delegate-renewal');
    await waitFor(
      'delegated renewal revocation',
      players[0].inspect,
      (value) => value.delegateText === 'Delegate renewals'
        && !value.social?.delegatedPlayerAssets?.includes(playerAssets[0]),
      180_000,
    );

    const synchronizedViewportMaxX = Math.min(64, manifest.mapWidth - 1);
    const synchronizedViewportMaxY = Math.min(64, manifest.mapHeight - 1);
    await Promise.all(players.map((player) => player.execute(`
      globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT(0, 0, arguments[0], arguments[1]);
    `, [synchronizedViewportMaxX, synchronizedViewportMaxY])));
    activated = await refreshWorldPlayers(
      players,
      'pre-chop viewport synchronization',
      (value) => value.state?.playerActive,
    );

    const sharedBeforeChops = assertSharedWorld(activated, 'pre-chop shared world');
    const occupied = new Set(sharedBeforeChops.trees.map((tree) => `${tree.x}:${tree.y}`));
    const selectableTrees = sharedBeforeChops.trees.filter((tree) => (
      tree.health === 10
      && tree.y + 1 < sharedBeforeChops.mapHeight
      && !occupied.has(`${tree.x}:${tree.y + 1}`)
    ));
    assert.ok(
      selectableTrees.length >= PLAYER_COUNT,
      `fewer than ${PLAYER_COUNT} full trees have an adjacent map tile`,
    );
    const selectedTrees = [422, 421, 423, 424, 425, 426, 419, 420]
      .map((treeId) => selectableTrees.find((tree) => tree.treeId === treeId))
      .filter(Boolean)
      .slice(0, PLAYER_COUNT);
    assert.equal(
      selectedTrees.length,
      PLAYER_COUNT,
      'deterministic multiplayer trees are unavailable',
    );
    assert.equal(new Set(selectedTrees.map((tree) => tree.treeId)).size, PLAYER_COUNT);
    const treesBeforeChops = new Map(
      sharedBeforeChops.trees.map((tree) => [tree.treeId, tree]),
    );

    await Promise.all(players.map((player, index) => (
      player.moveTo(selectedTrees[index].x, selectedTrees[index].y + 1)
    )));
    await Promise.all(players.map((player, index) => waitFor(
      `movement next to tree ${selectedTrees[index].treeId} (${player.label})`,
      player.inspect,
      (value) => !value.busy
        && value.state?.playerActive
        && value.adjacentTree?.treeId === selectedTrees[index].treeId
        && value.adjacentTree.health === 10
        && value.player?.x === selectedTrees[index].x
        && value.player?.y === selectedTrees[index].y + 1,
    )));
    await Promise.all(players.map((browserPlayer, viewerIndex) => waitFor(
      `remote map presence (${browserPlayer.label})`,
      browserPlayer.inspect,
      (value) => {
        if (!value.mapFrame) return false;
        const visibleIndexes = selectedTrees
          .map((tree, index) => ({ tree, index }))
          .filter(({ tree }) => (
            tree.x >= value.mapFrame.minX
            && tree.x <= value.mapFrame.maxX
            && tree.y + 1 >= value.mapFrame.minY
            && tree.y + 1 <= value.mapFrame.maxY
          ));
        const visibleOthers = visibleIndexes.filter(({ index }) => index !== viewerIndex).length;
        return value.remotePlayers === visibleOthers
          && value.social?.truncated === false
          && visibleIndexes.every(({ tree, index }) => value.social.locations.some((location) => (
            location.playerAsset === playerAssets[index]
            && location.x === tree.x
            && location.y === tree.y + 1
          )));
      },
      180_000,
    )));

    await Promise.all(players.map((player) => player.executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_REFRESH_WORLD().then(done);
    `)));

    const chopInputs = await Promise.all(players.map((player, index) => waitFor(
      `current chop preconditions (${player.label})`,
      player.inspect,
      (value) => !value.busy
        && value.state?.playerActive
        && value.adjacentTree?.treeId === selectedTrees[index].treeId
        && value.adjacentTree.health === 10,
    )));
    for (const view of chopInputs) {
      treesBeforeChops.set(view.adjacentTree.treeId, view.adjacentTree);
    }
    const stateOutpointsBeforeChops = chopInputs.map(
      (view) => view.state.playerStateOutpoint,
    );
    const initialChopResults = await Promise.all(players.map((player, index) => (
      player.chopExpected(chopInputs[index].state, chopInputs[index].adjacentTree)
    )));
    for (let retry = 0; retry < 5 && initialChopResults.some((result) => !result.ok); retry += 1) {
      const rejected = initialChopResults
        .map((result, index) => ({ result, index }))
        .filter(({ result }) => !result.ok);
      for (const { result } of rejected) {
        assert.match(
          result.message,
          /chop precondition changed|tree successor is not indexed yet/,
          'disjoint swing failed for a non-transient reason',
        );
      }
      await sleep(250);
      await Promise.all(rejected.map(async ({ index }) => {
        const player = players[index];
        await player.executeAsync(`
          const done = arguments[arguments.length - 1];
          globalThis.__WOODLAND_E2E_REFRESH_WORLD().then(done);
        `);
        const input = await waitFor(
          `refreshed chop preconditions (${player.label})`,
          player.inspect,
          (value) => !value.busy
            && value.state?.playerActive
            && value.adjacentTree?.treeId === selectedTrees[index].treeId,
        );
        const previousTree = treesBeforeChops.get(selectedTrees[index].treeId);
        if (input.adjacentTree.treeOutpoint !== previousTree.treeOutpoint) {
          initialChopResults[index] = { ok: true, state: input.state };
          return;
        }
        assert.equal(input.adjacentTree.health, 10);
        stateOutpointsBeforeChops[index] = input.state.playerStateOutpoint;
        initialChopResults[index] = await player.chopExpected(
          input.state,
          input.adjacentTree,
        );
      }));
    }
    assert.ok(
      initialChopResults.every((result) => result.ok),
      `disjoint swing was rejected: ${JSON.stringify(initialChopResults.map(
        ({ ok, message }) => ({ ok, message }),
      ))}`,
    );
    let ownChops = await Promise.all(players.map((player, index) => waitFor(
      `concurrent tree ${selectedTrees[index].treeId} swing (${player.label})`,
      player.inspect,
      (value) => !value.busy
        && value.state?.playerActive
        && value.state.walletSats === 330
        && value.state.lastAttempt?.treeId === selectedTrees[index].treeId
        && value.state.trees.find((tree) => tree.treeId === selectedTrees[index].treeId)?.treeOutpoint
          !== treesBeforeChops.get(selectedTrees[index].treeId).treeOutpoint,
      180_000,
    )));
    for (const [index, view] of ownChops.entries()) {
      const reward = Number(view.state.lastAttempt.success);
      assert.equal(view.state.playerXp, reward * manifest.woodcuttingXpPerLog);
      assert.equal(view.state.playerLevel, 1);
      assert.equal(view.state.playerNextLevelXp, 83);
      assert.equal(view.state.logDropBasisPoints, manifest.baseLogDropBasisPoints);
      assert.equal(view.state.playerLogs, reward);
      assert.equal(view.state.playerAsset, playerAssets[index]);
      assert.notEqual(view.state.playerStateOutpoint, stateOutpointsBeforeChops[index]);
      assert.equal(
        view.state.trees.find((tree) => tree.treeId === selectedTrees[index].treeId).health,
        10 - reward,
      );
    }
    const disjointRewards = ownChops.map((view) => Number(view.state.lastAttempt.success));
    const beforeRace = await refreshWorldPlayers(
      players,
      'same-tree race preparation',
      (value) => value.state?.fundingReady,
    );
    const raceShared = assertSharedWorld(beforeRace, 'same-tree race preparation');
    const raceTree = raceShared.trees.find((tree) => (
      tree.x <= synchronizedViewportMaxX
      && tree.y <= synchronizedViewportMaxY
      && tree.health === 10
      && !selectedTrees.some((selected) => selected.treeId === tree.treeId)
    ));
    assert.ok(raceTree, 'no common healthy tree is available for a race');
    const racePredictions = beforeRace.map((view) => (
      view.state.trees.find((tree) => tree.treeId === raceTree.treeId).nextDrop
    ));
    const racePlayerStateInputs = beforeRace.map((view) => view.state.playerStateOutpoint);
    const raceScoresBefore = beforeRace.map((view) => ({
      xp: view.state.playerXp,
      logs: view.state.playerLogs,
    }));
    const raceResults = await Promise.all(players.map((player, index) => {
      const playerTree = beforeRace[index].state.trees.find(
        (tree) => tree.treeId === raceTree.treeId,
      );
      return player.executeAsync(`
        const done = arguments[arguments.length - 1];
        globalThis.__WOODLAND_E2E_CHOP_EXPECTED(...Array.from(arguments).slice(0, -1)).then(done);
      `, [
        raceTree.treeId,
        raceTree.treeOutpoint,
        racePlayerStateInputs[index],
        playerTree.nextDrop,
      ]);
    }));
    assert.equal(
      raceResults.filter((result) => result.ok).length,
      1,
      'exactly one same-tree swing must win',
    );
    const winner = raceResults.findIndex((result) => result.ok);
    const raced = await refreshWorldPlayers(
      players,
      'same-tree race reconciliation',
      (value) => value.state?.fundingReady
        && !value.state.pendingChopTxid
        && value.state.trees.find((tree) => tree.treeId === raceTree.treeId)?.treeOutpoint
          !== raceTree.treeOutpoint,
    );
    const racedShared = assertSharedWorld(raced, 'same-tree race reconciliation');
    const racedTree = racedShared.trees.find((tree) => tree.treeId === raceTree.treeId);
    assert.equal(
      racedTree.health,
      10 - Number(racePredictions[winner]),
      'same-tree race must apply only the winning player outcome',
    );
    for (const [index, view] of raced.entries()) {
      const reward = index === winner ? Number(racePredictions[index]) : 0;
      assert.equal(
        view.state.playerXp,
        raceScoresBefore[index].xp + reward * manifest.woodcuttingXpPerLog,
      );
      assert.equal(view.state.playerLogs, raceScoresBefore[index].logs + reward);
      assert.equal(
        view.state.playerStateOutpoint !== racePlayerStateInputs[index],
        index === winner,
        `${players[index].label} race state transition`,
      );
    }
    treesBeforeChops.set(raceTree.treeId, racedTree);
    ownChops = raced;
    const raceRewards = racePredictions.map((drop, index) => (
      index === winner ? Number(drop) : 0
    ));

    if (!FULL_E2E) {
      const chopped = await refreshWorldPlayers(
        players,
        'smoke concurrent chop synchronization',
        (value, index) => value.state?.playerActive
          && value.state.playerXp
            === (disjointRewards[index] + raceRewards[index]) * manifest.woodcuttingXpPerLog
          && value.state.playerLogs === disjointRewards[index] + raceRewards[index]
          && selectedTrees.every((selected) => (
            value.state.trees.find((tree) => tree.treeId === selected.treeId)?.treeOutpoint
              !== treesBeforeChops.get(selected.treeId).treeOutpoint
          )),
      );
      const shared = assertSharedWorld(chopped, 'smoke post-chop shared world');
      for (const [index, selected] of selectedTrees.entries()) {
        const tree = shared.trees.find((candidate) => candidate.treeId === selected.treeId);
        assert.equal(tree.health, 10 - disjointRewards[index]);
        assert.ok(tree.lastAttemptTxid);
      }
      await assertLeaderboardMatches(chopped, 'verified smoke leaderboard');
      await assertSubmissionRecovery(players[0], chopped[0], selectedTrees[0].treeId);
      console.log(JSON.stringify({
        profile: E2E_PROFILE,
        players: chopped.map((view, index) => ({
          address: view.state.address,
          treeId: selectedTrees[index].treeId,
          xp: view.state.playerXp,
          playerAsset: view.state.playerAsset,
          logs: view.state.playerLogs,
        })),
      }));
      return;
    }

    const attemptCounts = Array(players.length).fill(1);
    ownChops = await Promise.all(players.map(async (player, index) => {
      let view = ownChops[index];
      let attempts = 1;
      while (
        view.state.trees.find((tree) => tree.treeId === selectedTrees[index].treeId).health === 10
      ) {
        attempts += 1;
        assert.ok(attempts <= 11, `${player.label} exceeded the luck-protection bound`);
        const beforeTree = view.state.trees.find(
          (tree) => tree.treeId === selectedTrees[index].treeId,
        );
        const previous = beforeTree.treeOutpoint;
        const previousStateOutpoint = view.state.playerStateOutpoint;
        const result = await player.chopExpected(view.state, beforeTree);
        assert.equal(result.ok, true, result.message);
        view = await waitFor(
          `follow-up swing ${attempts} (${player.label})`,
          player.inspect,
          (value) => !value.busy
            && value.state?.trees.find((tree) => tree.treeId === selectedTrees[index].treeId)
              ?.treeOutpoint !== previous,
          180_000,
        );
        assert.notEqual(view.state.playerStateOutpoint, previousStateOutpoint);
      }
      attemptCounts[index] = attempts;
      return view;
    }));
    for (const [index, view] of ownChops.entries()) {
      const expectedRewards = 1 + raceRewards[index];
      assert.equal(
        view.state.playerXp,
        expectedRewards * manifest.woodcuttingXpPerLog,
      );
      assert.equal(view.state.playerLevel, 1);
      assert.equal(view.state.playerNextLevelXp, 83);
      assert.equal(view.state.logDropBasisPoints, manifest.baseLogDropBasisPoints);
      assert.equal(view.state.playerLogs, expectedRewards);
    }
    assert.ok(
      attemptCounts.every((attempts) => attempts >= 1 && attempts <= 11),
      'multiplayer luck exceeded its swing bound',
    );

    const chopped = await refreshWorldPlayers(
      players,
      'concurrent chop synchronization',
      (value, index) => value.state?.playerActive
        && value.state.playerXp
          === (1 + raceRewards[index]) * manifest.woodcuttingXpPerLog
        && value.state.playerLevel === 1
        && value.state.playerNextLevelXp === 83
        && value.state.logDropBasisPoints === manifest.baseLogDropBasisPoints
        && value.state.playerLogs === 1 + raceRewards[index]
        && selectedTrees.every((selected) => (
          value.state.trees.find((tree) => tree.treeId === selected.treeId)?.health === 9
        )),
    );
    assertPlayersActive(chopped, 'post-chop state');
    const sharedAfterChops = assertSharedWorld(chopped, 'post-chop shared world');
    assert.ok(
      selectedTrees.every((selected) => (
        sharedAfterChops.trees.find((tree) => tree.treeId === selected.treeId)?.health > 0
      )),
      'multiplayer chops unexpectedly depleted a selected tree',
    );
    const selectedIds = new Set(selectedTrees.map((tree) => tree.treeId));
    for (const tree of sharedAfterChops.trees) {
      const before = treesBeforeChops.get(tree.treeId);
      if (selectedIds.has(tree.treeId)) {
        assert.equal(before.health, 10);
        assert.equal(tree.health, 9);
        assert.equal(before.valueSats, 330);
        assert.equal(tree.valueSats, 330);
        assert.notEqual(tree.treeOutpoint, before.treeOutpoint);
        assert.ok(tree.lastAttemptTxid);
      } else {
        const {
          expiresInSeconds: beforeExpiry,
          nextRollBucket: _beforeRollBucket,
          nextDrop: _beforeDrop,
          ...beforeStable
        } = before;
        const {
          expiresInSeconds: afterExpiry,
          nextRollBucket: _afterRollBucket,
          nextDrop: _afterDrop,
          ...afterStable
        } = tree;
        assert.deepEqual(afterStable, beforeStable);
        if (beforeExpiry != null && afterExpiry != null) {
          assert.ok(afterExpiry <= beforeExpiry && afterExpiry > 300);
        }
      }
    }
    const chopTxids = selectedTrees.map((selected) => (
      sharedAfterChops.trees.find((tree) => tree.treeId === selected.treeId).lastAttemptTxid
    ));
    assert.notEqual(chopTxids[0], chopTxids[1]);
    assertTreeValue(sharedAfterChops, 'post-chop state');

    await assertLeaderboardMatches(chopped, 'verified XP leaderboard');
    await assertSubmissionRecovery(players[0], chopped[0], selectedTrees[0].treeId);
    console.log(JSON.stringify({
      profile: E2E_PROFILE,
      players: chopped.map((view, index) => ({
        address: view.state.address,
        treeId: selectedTrees[index].treeId,
        xp: view.state.playerXp,
        playerAsset: view.state.playerAsset,
        logs: view.state.playerLogs,
      })),
      treeTxids: chopTxids,
    }));
  } catch (error) {
    console.error(error);
    for (const session of sessions) {
      try {
        console.error(`${session.label} snapshot: ${JSON.stringify(await session.inspect())}`);
      } catch (snapshotError) {
        console.error(`${session.label} snapshot unavailable: ${snapshotError.message}`);
      }
    }
    await Promise.all(sessions.map((session, index) => saveScreenshot(
      session.driverUrl,
      session.sessionId,
      `multiplayer-player-${index + 1}-failure.png`,
    )));
    if (web) console.error(`web process output:\n${web.output()}`);
    drivers.forEach((driver, index) => {
      console.error(`geckodriver ${index + 1} output:\n${driver.output()}`);
    });
    process.exitCode = 1;
  } finally {
    await Promise.allSettled(sessions.map((session) => (
      request(session.driverUrl, 'DELETE', `/session/${session.sessionId}`, undefined, 10_000)
    )));
    await Promise.all([
      ...drivers.map((driver) => stopProcess(driver)),
      stopProcess(web),
    ]);
  }
}

await main();
