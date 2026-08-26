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
      joinLeaderboardHidden: document.getElementById('join-leaderboard')?.hidden ?? true,
      social: globalThis.__WOODLAND_E2E_SOCIAL || null,
      remotePlayers: document.querySelectorAll('#map .remote-player').length,
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
    const cell = document.querySelector(
      '#map .map-cell[data-x="' + arguments[0] + '"][data-y="' + arguments[1] + '"]',
    );
    if (!cell) throw new Error('missing map cell');
    cell.click();
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
        && value.state?.trees?.length === 10
        && !value.state.playerActive
        && !value.state.fundingReady,
    )));

    assert.notEqual(initial[0].state.address, initial[1].state.address);
    const initialShared = assertSharedWorld(initial, 'initial shared world');
    assert.equal(initialShared.fullTreeValueSats, 1_980);
    assert.ok(
      initialShared.trees.every((tree) => tree.health >= 0 && tree.health <= 5),
      'shared world contains invalid tree health',
    );
    assert.ok(
      initialShared.trees.filter((tree) => tree.health === 5).length >= 2,
      'shared world has fewer than two full trees',
    );
    for (const view of initial) {
      assert.equal(view.state.playerLogs, 0);
      assert.equal(view.state.playerXp, 0);
      assert.equal(view.state.playerLevel, 1);
      assert.equal(view.state.playerNextLevelXp, 83);
      assert.equal(view.state.logDropBasisPoints, 1_000);
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
      execFileSync(
        path.join(ROOT, 'scripts/regtest.sh'),
        ['fund', view.state.address, String(activationAmounts[index])],
        { cwd: ROOT, stdio: 'pipe', encoding: 'utf8' },
      );
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
        && value.state.logDropBasisPoints === 1_000
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

    await Promise.all(players.map((player) => player.click('join-leaderboard')));
    const joined = await Promise.all(players.map((player) => waitFor(
      `server opt-in (${player.label})`,
      player.inspect,
      (value) => value.joinLeaderboardHidden
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
      `world-scoped leaderboard opt-in reload (${player.label})`,
      player.inspect,
      (value) => value.ready
        && !value.busy
        && value.state?.playerAsset === playerAssets[index]
        && value.joinLeaderboardHidden
        && value.leaderboard.some((entry) => entry.playerAsset === playerAssets[index])
        && (index !== 0 || value.delegateText.startsWith('Stop delegated')),
      180_000,
    )));
    assert.ok(restoredOptIns.every((view) => view.joinLeaderboardHidden));
    await players[0].click('delegate-renewal');
    await waitFor(
      'delegated renewal revocation',
      players[0].inspect,
      (value) => value.delegateText === 'Delegate renewals'
        && !value.social?.delegatedPlayerAssets?.includes(playerAssets[0]),
      180_000,
    );

    const sharedBeforeChops = assertSharedWorld(activated, 'pre-chop shared world');
    const occupied = new Set(sharedBeforeChops.trees.map((tree) => `${tree.x}:${tree.y}`));
    const selectableTrees = sharedBeforeChops.trees.filter((tree) => (
      tree.health === 5
      && tree.y + 1 < sharedBeforeChops.mapHeight
      && !occupied.has(`${tree.x}:${tree.y + 1}`)
    ));
    assert.ok(
      selectableTrees.length >= PLAYER_COUNT,
      `fewer than ${PLAYER_COUNT} full trees have an adjacent map tile`,
    );
    const selectedTrees = [422, 421, 423, 424]
      .slice(0, PLAYER_COUNT)
      .map((treeId) => selectableTrees.find((tree) => tree.treeId === treeId));
    assert.ok(selectedTrees.every(Boolean), 'deterministic multiplayer trees are unavailable');
    assert.equal(new Set(selectedTrees.map((tree) => tree.treeId)).size, PLAYER_COUNT);
    const treesBeforeChops = new Map(
      sharedBeforeChops.trees.map((tree) => [tree.treeId, tree]),
    );
    const stateOutpointsBeforeChops = activated.map((view) => view.state.playerStateOutpoint);

    await Promise.all(players.map((player, index) => (
      player.moveTo(selectedTrees[index].x, selectedTrees[index].y + 1)
    )));
    await Promise.all(players.map((player, index) => waitFor(
      `movement next to tree ${selectedTrees[index].treeId} (${player.label})`,
      player.inspect,
      (value) => !value.busy
        && value.state?.playerActive
        && value.adjacentTree?.treeId === selectedTrees[index].treeId
        && value.adjacentTree.health === 5
        && value.player?.x === selectedTrees[index].x
        && value.player?.y === selectedTrees[index].y + 1,
    )));
    await Promise.all(players.map((browserPlayer) => waitFor(
      `remote map presence (${browserPlayer.label})`,
      browserPlayer.inspect,
      (value) => value.remotePlayers >= PLAYER_COUNT - 1
        && selectedTrees.every((tree, index) => value.social?.locations?.some((location) => (
          location.playerAsset === playerAssets[index]
          && location.x === tree.x
          && location.y === tree.y + 1
        ))),
      180_000,
    )));

    const initialChopResults = await Promise.all(players.map((player, index) => (
      player.chopExpected(activated[index].state, selectedTrees[index])
    )));
    assert.ok(initialChopResults.every((result) => result.ok), 'disjoint swing was rejected');
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
      assert.equal(view.state.playerXp, reward);
      assert.equal(view.state.playerLevel, 1);
      assert.equal(view.state.playerNextLevelXp, 83);
      assert.equal(view.state.logDropBasisPoints, 1_000);
      assert.equal(view.state.playerLogs, reward);
      assert.equal(view.state.playerAsset, playerAssets[index]);
      assert.notEqual(view.state.playerStateOutpoint, stateOutpointsBeforeChops[index]);
      assert.equal(
        view.state.trees.find((tree) => tree.treeId === selectedTrees[index].treeId).health,
        5 - reward,
      );
    }

    const beforeRace = await refreshPlayers(
      players,
      'same-tree race preparation',
      (value) => value.state?.fundingReady && value.state.playerXp === 0,
    );
    const raceShared = assertSharedWorld(beforeRace, 'same-tree race preparation');
    const raceTree = raceShared.trees.find((tree) => (
      tree.health === 5
      && tree.nextDrop === false
      && !selectedTrees.some((selected) => selected.treeId === tree.treeId)
    ));
    assert.ok(raceTree, 'no untouched deterministic-miss tree is available for a race');
    const racePlayerStateInputs = beforeRace.map((view) => view.state.playerStateOutpoint);
    const raceResults = await Promise.all(players.map((player, index) => player.executeAsync(`
      const done = arguments[arguments.length - 1];
      globalThis.__WOODLAND_E2E_CHOP_EXPECTED(...Array.from(arguments).slice(0, -1)).then(done);
    `, [
      raceTree.treeId,
      raceTree.treeOutpoint,
      racePlayerStateInputs[index],
      raceTree.nextDrop,
    ])));
    assert.equal(
      raceResults.filter((result) => result.ok).length,
      1,
      'exactly one same-tree swing must win',
    );
    const winner = raceResults.findIndex((result) => result.ok);
    const raced = await refreshPlayers(
      players,
      'same-tree race reconciliation',
      (value) => value.state?.fundingReady
        && !value.state.pendingChopTxid
        && value.state.trees.find((tree) => tree.treeId === raceTree.treeId)?.treeOutpoint
          !== raceTree.treeOutpoint,
    );
    const racedShared = assertSharedWorld(raced, 'same-tree race reconciliation');
    const racedTree = racedShared.trees.find((tree) => tree.treeId === raceTree.treeId);
    assert.equal(racedTree.health, 5, 'deterministic race miss changed tree health');
    for (const [index, view] of raced.entries()) {
      assert.equal(view.state.playerXp, 0);
      assert.equal(view.state.playerLogs, 0);
      assert.equal(
        view.state.playerStateOutpoint !== racePlayerStateInputs[index],
        index === winner,
        `${players[index].label} race state transition`,
      );
    }
    treesBeforeChops.set(raceTree.treeId, racedTree);
    ownChops = raced;

    if (!FULL_E2E) {
      assert.ok(
        ownChops.every((view) => view.state.lastAttempt.success === false),
        'smoke profile expects both deterministic first swings to miss',
      );
      const chopped = await refreshPlayers(
        players,
        'smoke concurrent chop synchronization',
        (value) => value.state?.playerActive
          && value.state.playerXp === 0
          && value.state.playerLogs === 0
          && selectedTrees.every((selected) => (
            value.state.trees.find((tree) => tree.treeId === selected.treeId)?.treeOutpoint
              !== treesBeforeChops.get(selected.treeId).treeOutpoint
          )),
      );
      const shared = assertSharedWorld(chopped, 'smoke post-chop shared world');
      for (const selected of selectedTrees) {
        const tree = shared.trees.find((candidate) => candidate.treeId === selected.treeId);
        assert.equal(tree.health, 5);
        assert.ok(tree.lastAttemptTxid);
      }
      await assertLeaderboardMatches(chopped, 'verified smoke leaderboard');
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
      while (view.state.playerLogs === 0) {
        attempts += 1;
        assert.ok(attempts <= 25, `${player.label} did not receive LOG within 25 swings`);
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
      assert.equal(view.state.playerXp, 1);
      assert.equal(view.state.playerLevel, 1);
      assert.equal(view.state.playerNextLevelXp, 83);
      assert.equal(view.state.logDropBasisPoints, 1_000);
      assert.equal(view.state.playerLogs, 1);
    }
    const expectedHitAttempts = new Map([
      [422, 12],
      [421, 16],
      [423, 22],
      [424, 3],
    ]);
    assert.deepEqual(
      attemptCounts,
      selectedTrees.map((tree) => expectedHitAttempts.get(tree.treeId)),
      'clean multiplayer rolls changed unexpectedly',
    );

    const chopped = await refreshPlayers(
      players,
      'concurrent chop synchronization',
      (value, index) => value.state?.playerActive
        && value.state.playerXp === 1
        && value.state.playerLevel === 1
        && value.state.playerNextLevelXp === 83
        && value.state.logDropBasisPoints === 1_000
        && value.state.playerLogs === 1
        && selectedTrees.every((selected) => (
          value.state.trees.find((tree) => tree.treeId === selected.treeId)?.health === 4
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
        assert.equal(before.health, 5);
        assert.equal(tree.health, 4);
        assert.equal(before.valueSats, 1_980);
        assert.equal(tree.valueSats, 1_980);
        assert.notEqual(tree.treeOutpoint, before.treeOutpoint);
        assert.ok(tree.lastAttemptTxid);
      } else {
        const { expiresInSeconds: beforeExpiry, ...beforeStable } = before;
        const { expiresInSeconds: afterExpiry, ...afterStable } = tree;
        assert.deepEqual(afterStable, beforeStable);
        assert.ok(afterExpiry <= beforeExpiry && afterExpiry > 300);
      }
    }
    const chopTxids = selectedTrees.map((selected) => (
      sharedAfterChops.trees.find((tree) => tree.treeId === selected.treeId).lastAttemptTxid
    ));
    assert.notEqual(chopTxids[0], chopTxids[1]);
    assertTreeValue(sharedAfterChops, 'post-chop state');

    await assertLeaderboardMatches(chopped, 'verified XP leaderboard');
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
