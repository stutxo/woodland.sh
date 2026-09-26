#!/usr/bin/env node
// Client-only regressions; no chain, browser binary, or production keys needed.
// Run: node scripts/test-browser-recovery.mjs
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

// Keep the CI entry point ordinary Node while using its real ES-module loader.
if (!vm.SourceTextModule) {
  const child = spawnSync(process.execPath, [
    '--experimental-vm-modules', fileURLToPath(import.meta.url),
  ], { stdio: 'inherit' });
  if (child.error) throw child.error;
  process.exit(child.status ?? 1);
}

const sourceUrl = new URL('../web/app.js', import.meta.url);
const source = await readFile(sourceUrl, 'utf8');
const genesisTxid = '11'.repeat(32);
const playerAsset = `${'22'.repeat(32)}0000`;
const secretKey = '06'.repeat(32);

function browserOrigin() {
  const storage = new Map();
  const locks = new Map();
  return {
    storage,
    openTab() {
      const owner = Symbol('tab');
      return {
        navigator: {
          locks: {
            async request(name, options, callback) {
              assert.equal(options.ifAvailable, true);
              if (locks.has(name)) return callback(null);
              locks.set(name, owner);
              try {
                return await callback({ name });
              } finally {
                if (locks.get(name) === owner) locks.delete(name);
              }
            },
          },
        },
        // Browsers release a document's locks when that document is destroyed.
        close() {
          for (const [name, holder] of locks) if (holder === owner) locks.delete(name);
        },
      };
    },
  };
}

async function fixture({
  trees = [], position = { x: 3, y: 17 }, profile = {},
  coordination = browserOrigin(), installWallet = true, network = 'regtest', webLocks = true,
} = {}) {
  const elements = new Map();
  const intervals = new Map();
  const timers = new Map();
  const storage = coordination.storage;
  const tab = coordination.openTab();
  const blobs = [];
  const locations = [];
  const chops = [];
  const regrowths = [];
  let now = 1_000_000;
  let timerId = 0;
  let initializations = 0;
  let confirmations = 0;
  let reloads = 0;
  let failedStorageKey = null;
  let restoreInitializations = 0;
  const flush = async () => {
    // Drain the real promise-based WASM queue without advancing the fake clock.
    for (let turn = 0; turn < 40; turn += 1) await Promise.resolve();
  };
  async function advance(milliseconds) {
    const target = now + milliseconds;
    for (;;) {
      const next = [...timers.entries()]
        .filter(([, timer]) => timer.at <= target)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      now = next[1].at;
      timers.delete(next[0]);
      next[1].callback();
      await flush();
    }
    now = target;
    await flush();
  }
  function element(id) {
    if (elements.has(id)) return elements.get(id);
    const listeners = new Map();
    const classes = new Set();
    let text = '';
    const node = {
      children: [], value: '', hidden: false, disabled: false,
      clientWidth: 0, clientHeight: 0, style: {},
      get textContent() { return text + this.children.map((child) => child.textContent).join(''); },
      set textContent(value) { text = String(value); this.children = []; },
      classList: {
        add: (name) => classes.add(name),
        remove: (name) => classes.delete(name),
        contains: (name) => classes.has(name),
        toggle(name, enabled) {
          if (enabled) classes.add(name);
          else classes.delete(name);
        },
      },
      addEventListener: (event, action) => listeners.set(event, action),
      click: () => listeners.get('click')?.(),
      append(...children) { this.children.push(...children); },
      replaceChildren(...children) { text = ''; this.children = children; },
      remove() {}, setAttribute() {},
    };
    elements.set(id, node);
    return node;
  }
  let available = true;
  let leaderboardMode = 'ok';
  let releaseLeaderboard;
  let messages = [];
  let players = [];
  let renewalError = null;
  let renewals = 0;
  let holdRefresh = false;
  let releaseRefresh;
  let chopFailure = null;
  const current = {
    genesisTxid, address: 'tark1localfixture', playerAsset,
    playerActive: true, playerStateOutpoint: `${'33'.repeat(32)}:0`,
    playerStateExpiresInSeconds: 3_600, playerRolloverMarginSeconds: 7_200,
    playerXp: 0, playerLogs: 0, playerStone: 0, playerIronOre: 0,
    playerAxe: 'none', playerLevel: 1, mapWidth: 425, mapHeight: 425,
    trees, fundingRequiredSats: 0, walletSats: 350, fundingReady: true,
    dustSats: 330, walletVtxos: [{ amountSats: 20, assets: [] }],
    emulatorVersion: 'fixture', emulatorSigner: '44'.repeat(32),
  };
  const app = {
    exportKey: () => secretKey,
    exportProfile: () => JSON.stringify({ genesisTxid, playerAsset, ...profile }),
    setTreeViewport() {},
    refresh: async () => {
      if (holdRefresh) await new Promise((resolve) => { releaseRefresh = resolve; });
      return { ...current };
    },
    refreshWorld: async () => {
      if (holdRefresh) await new Promise((resolve) => { releaseRefresh = resolve; });
      return { ...current };
    },
    activate: async () => ({ ...current }),
    renewPlayer: async () => {
      renewals += 1;
      if (renewalError) throw new Error(renewalError);
      return { ...current };
    },
    chop: async (treeId) => {
      chops.push(treeId);
      if (chopFailure) {
        storage.set(`woodland.sh:web:v2:pending:https://arkade.example:${genesisTxid}`, 'signed pending swing');
        throw new Error(chopFailure);
      }
      return { ...current, lastAttempt: { success: true, material: 'none' } };
    },
    regrow: async (treeId) => {
      regrowths.push(treeId);
      return { ...current };
    },
    serverLocation: (_server, x, y, timestamp) => ({ x, y, timestamp }),
    serverRegistration: () => ({ playerAsset }),
    address: () => current.address,
  };
  class HostURL extends URL {
    static createObjectURL(blob) { blobs.push(blob); return 'blob:local-backup'; }
    static revokeObjectURL() {}
  }
  class ClockDate extends Date {
    constructor(...args) { super(...(args.length ? args : [now])); }
    static now() { return now; }
  }
  const context = vm.createContext({
    console, URL: HostURL, URLSearchParams, Blob, Date: ClockDate,
    performance: { now: () => now },
    location: { origin: 'http://127.0.0.1:8090', reload() { reloads += 1; } },
    navigator: webLocks ? tab.navigator : {},
    window: { addEventListener() {}, confirm() { confirmations += 1; return true; } },
    document: {
      getElementById: element,
      querySelector: () => ({ content: 'self' }),
      createElement: (tag) => element(Symbol(tag)),
      body: { append() {} },
    },
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => {
        if (key === failedStorageKey) throw new Error('QuotaExceededError');
        storage.set(key, value);
      },
      removeItem: (key) => storage.delete(key),
    },
    setTimeout: (callback, delay = 0) => {
      const id = ++timerId;
      timers.set(id, { callback, at: now + delay });
      return id;
    },
    clearTimeout: (id) => timers.delete(id),
    setInterval: (callback, delay) => {
      const id = ++timerId;
      intervals.set(id, { callback, delay });
      return id;
    },
    clearInterval: (id) => intervals.delete(id),
    fetch: async (url, options) => {
      const path = new URL(url).pathname;
      if (path === new URL('./world.json', sourceUrl).pathname) {
        // Boot starts this download alongside the intentionally held WASM init.
        return { ok: true, text: async () => '{}' };
      }
      let payload;
      if (path === '/v1/leaderboard') {
        if (leaderboardMode === 'failed') throw new Error('leaderboard unavailable');
        if (leaderboardMode === 'hung') {
          await new Promise((resolve) => { releaseLeaderboard = resolve; });
        }
        payload = { delegationAvailable: available, delegatedPlayerAssets: [playerAsset], players };
      } else if (path === '/v1/location') {
        locations.push(JSON.parse(options.body));
        payload = {};
      } else if (path === '/v1/presence') {
        payload = { locations: [] };
      } else if (path === '/v1/chat') {
        payload = { messages };
      } else if (path === '/v1/players') {
        payload = {};
      } else {
        assert.fail(`Unexpected fixture request: ${path}`);
      }
      return { ok: true, json: async () => payload };
    },
  });
  // These exports exist only in this VM fixture, not in the shipped browser module.
  const module = new vm.SourceTextModule(`${source}\n
    export function installFixture(wallet, snapshot, manifest, position) {
      app = wallet;
      profileStorageKey = PROFILE + ':' + snapshot.genesisTxid;
      pendingStorageKey = 'woodland.sh:web:v2:pending:https://arkade.example:' + snapshot.genesisTxid;
      worldManifest = manifest;
      treeLayout = snapshot.trees;
      busy = false;
      serverRegistered = true;
      serverRegistrationOutpoint = snapshot.playerStateOutpoint;
      Object.assign(player, position);
      adoptState(snapshot);
      renderSocialControls();
      render();
    }
    export {
      refreshLeaderboard, refreshChat, handleMapPosition, attemptChop,
      publishLocation, syncServerRegistration, parsePlayerBackup,
      persistProfile, withApp, acquireWalletWriter,
      restorePlayerBackup,
    };
  `, {
    context,
    identifier: sourceUrl.href,
    initializeImportMeta: (meta) => { meta.url = sourceUrl.href; },
  });
  const wasm = new vm.SyntheticModule(['default', 'WoodlandApp'], function () {
    // Leave automatic boot waiting; exercise real UI flows against a wallet boundary.
    this.setExport('default', () => {
      initializations += 1;
      return new Promise(() => {});
    });
    this.setExport('WoodlandApp', {
      async init() {
        restoreInitializations += 1;
        return {
          address: () => 'tark1restored',
          refreshPlayer: async () => ({ ...current }),
        };
      },
    });
  }, { context });
  await module.link((specifier) => {
    assert.equal(specifier, './pkg/woodland.js');
    return wasm;
  });
  await module.evaluate();
  const install = (snapshot) => module.namespace.installFixture(app, snapshot, {
    gameId: 'woodland.sh', protocolVersion: 4, network, genesisTxid,
    mapWidth: 425, mapHeight: 425, activeLogsPerTree: 10,
    arkadeServiceUrl: 'https://arkade.example', emulatorUrl: 'https://emulator.example',
  }, position);
  await flush();
  if (installWallet) install({ ...current });
  const interval = (delay) => [...intervals.values()].find((entry) => entry.delay === delay).callback();
  return {
    current, install, element, blobs, locations, chops, regrowths, advance, flush,
    storage,
    close: () => tab.close(),
    initializations: () => initializations,
    confirmations: () => confirmations,
    reloads: () => reloads,
    persistProfile: () => module.namespace.persistProfile(),
    runWalletAction: (action) => module.namespace.withApp(action),
    acquireWalletWriter: () => module.namespace.acquireWalletWriter(),
    holdRefresh: () => { holdRefresh = true; },
    releaseRefresh: () => { holdRefresh = false; releaseRefresh(); },
    failChopAfterJournal: () => { chopFailure = 'submission response lost after saving the journal'; },
    failStorageWrite: (key) => { failedStorageKey = key; },
    restoreInitializations: () => restoreInitializations,
    restoreBackup: (value) => module.namespace.restorePlayerBackup({
      size: Buffer.byteLength(JSON.stringify(value)), text: async () => JSON.stringify(value),
    }),
    renewals: () => renewals,
    failRenewal: (message) => { renewalError = message; },
    setDelegationAvailable: async (value) => {
      available = value;
      await module.namespace.refreshLeaderboard();
    },
    setLeaderboardMode: (mode) => { leaderboardMode = mode; },
    releaseLeaderboard: () => releaseLeaderboard(),
    refreshLeaderboard: () => module.namespace.refreshLeaderboard(),
    setSocial: async (nextPlayers, nextMessages) => {
      players = nextPlayers;
      messages = nextMessages;
      await module.namespace.refreshLeaderboard();
      await module.namespace.refreshChat();
    },
    clickMap: (x, y) => module.namespace.handleMapPosition(x, y),
    chopAdjacent: () => module.namespace.attemptChop(),
    publishLocation: (force) => module.namespace.publishLocation(force),
    register: () => module.namespace.syncServerRegistration(true),
    parseBackup: (value) => module.namespace.parsePlayerBackup(value),
    poll: () => interval(10_000),
    presenceTick: async () => { interval(1_000); await flush(); },
  };
}

const renewal = await fixture();
await renewal.setDelegationAvailable(true);
await renewal.poll();
assert.equal(renewal.renewals(), 0, 'available delegated renewal must not double-renew');
await renewal.setDelegationAvailable(false);
await renewal.poll();
assert.equal(renewal.renewals(), 1, 'unavailable delegation must fall back to owner renewal');
renewal.failRenewal('local renewal fee funding required');
await renewal.poll();
assert.equal(renewal.renewals(), 2, 'owner fallback must still be attempted');
assert.ok(
  renewal.element('status').textContent.includes('local renewal fee funding required'),
  'automatic owner-renewal failure must be visible',
);

const failedDelegation = await fixture();
await failedDelegation.setDelegationAvailable(true);
failedDelegation.setLeaderboardMode('failed');
await failedDelegation.refreshLeaderboard();
await failedDelegation.poll();
assert.equal(failedDelegation.renewals(), 1, 'failed delegation lookup must permit owner renewal');

const staleDelegation = await fixture();
await staleDelegation.setDelegationAvailable(true);
staleDelegation.setLeaderboardMode('hung');
const delayedLeaderboard = staleDelegation.refreshLeaderboard();
await staleDelegation.advance(44_999);
await staleDelegation.poll();
assert.equal(staleDelegation.renewals(), 0, 'fresh delegated observation must prevent competing renewal');
await staleDelegation.advance(1);
await staleDelegation.poll();
assert.equal(staleDelegation.renewals(), 1, 'hung leaderboard must not suppress owner renewal past freshness');
staleDelegation.releaseLeaderboard();
await delayedLeaderboard;
await staleDelegation.poll();
assert.equal(staleDelegation.renewals(), 2, 'late stale response must not extend delegated freshness');
staleDelegation.setLeaderboardMode('ok');
await staleDelegation.refreshLeaderboard();
await staleDelegation.poll();
assert.equal(staleDelegation.renewals(), 2, 'new successful observation must restore delegated ownership');

const overlappingDelegation = await fixture();
overlappingDelegation.setLeaderboardMode('hung');
const oldLeaderboard = overlappingDelegation.refreshLeaderboard();
await overlappingDelegation.advance(45_000);
overlappingDelegation.setLeaderboardMode('ok');
await overlappingDelegation.refreshLeaderboard();
overlappingDelegation.releaseLeaderboard();
await oldLeaderboard;
await overlappingDelegation.poll();
assert.equal(overlappingDelegation.renewals(), 0, 'older response must not displace a fresh successful observation');

const stalledDelegation = await fixture();
for (const secondsRemaining of [3_600, 1_801, 1_800, 900, 301]) {
  stalledDelegation.current.playerStateExpiresInSeconds = secondsRemaining;
  // The leaderboard remains responsive even though the worker never extends expiry.
  await stalledDelegation.setDelegationAvailable(true);
  await stalledDelegation.poll();
  await stalledDelegation.advance(15_000);
}
assert.equal(stalledDelegation.renewals(), 3, 'responsive delegation must yield to owner recovery before expiry');
assert.ok(stalledDelegation.current.walletVtxos.length > 0, 'owner fee funding must be available in fallback scenario');

const sharedOrigin = browserOrigin();
const activeTab = await fixture({ coordination: sharedOrigin });
const staleTab = await fixture({
  coordination: sharedOrigin, profile: { playerAsset: null }, installWallet: false,
});
assert.equal(activeTab.initializations(), 1, 'first tab must initialize WASM under its lock');
assert.equal(staleTab.initializations(), 0, 'second tab must be blocked before WASM or key initialization');
assert.match(staleTab.element('status').textContent, /already open in another tab/);
await activeTab.poll();
const profileKey = `woodland.sh:web:v2:profile:http://127.0.0.1:8090:${genesisTxid}`;
const keyStorageKey = 'woodland.sh:web:v2:key:http://127.0.0.1:8090';
const pendingKey = `woodland.sh:web:v2:pending:https://arkade.example:${genesisTxid}`;
sharedOrigin.storage.set(keyStorageKey, secretKey);
sharedOrigin.storage.set(pendingKey, 'submitted signed transaction');
const savedWallet = [...sharedOrigin.storage];
await staleTab.poll();
assert.throws(() => staleTab.persistProfile(), /does not control the player wallet/);
let submittedFromStaleTab = false;
await assert.rejects(staleTab.runWalletAction(() => { submittedFromStaleTab = true; }), /does not control/);
assert.equal(submittedFromStaleTab, false, 'blocked tab must not submit any WASM action');
assert.deepEqual([...sharedOrigin.storage], savedWallet, 'blocked tab must preserve the key, profile, and pending transaction');
assert.equal(JSON.parse(sharedOrigin.storage.get(profileKey)).playerAsset, playerAsset, 'old tab must not erase activated PLAYER_ID');
activeTab.close();
await staleTab.acquireWalletWriter();
staleTab.install({ ...staleTab.current });
await staleTab.runWalletAction(() => {});

const unsupportedBrowser = await fixture({ webLocks: false, installWallet: false });
assert.equal(unsupportedBrowser.initializations(), 0, 'unsupported browser must fail before creating a wallet');
assert.match(unsupportedBrowser.element('status').textContent, /Web Locks support/);

const presence = await fixture();
await presence.presenceTick();
assert.equal(presence.locations.length, 1, 'registered player must publish its initial location');
await presence.advance(29_999);
await presence.presenceTick();
assert.equal(presence.locations.length, 1, 'stationary presence must not post every poll');
await presence.advance(1);
await presence.presenceTick();
assert.equal(presence.locations.length, 2, 'stationary presence must refresh before the 60-second TTL');
assert.deepEqual(
  presence.locations.map(({ x, y }) => [x, y]),
  [[3, 17], [3, 17]],
  'heartbeat must preserve the stationary position',
);
assert.equal(presence.locations[1].timestamp - presence.locations[0].timestamp, 30_000);
await presence.publishLocation(true);
assert.equal(presence.locations.length, 2, 'forced posts must still respect the posting throttle');
await presence.advance(750);
await presence.register();
await presence.flush();
assert.equal(presence.locations.length, 3, 'registration must force a fresh unchanged location');

// Canonical woodland shared-neighbor case: both trees touch player tile (29,36),
// and tree #470 precedes the clicked #679 in the manifest.
const sharedNeighborTrees = [
  { treeId: 470, x: 28, y: 36 },
  { treeId: 679, x: 30, y: 36 },
].map((tree) => ({
  ...tree, health: 10, logReserveRemaining: 50_000, xpRemaining: 50_000,
  stoneRemaining: 50_000, ironOreRemaining: 50_000, valueSats: 330,
}));
const selected = await fixture({ trees: sharedNeighborTrees, position: { x: 29, y: 36 } });
await selected.clickMap(30, 36);
await selected.flush();
assert.deepEqual(selected.chops, [679], 'click must chop the selected tree, not the first adjacent tree');

const walking = await fixture({ trees: sharedNeighborTrees, position: { x: 29, y: 37 } });
const arrival = walking.clickMap(30, 36);
assert.equal(walking.element('hud-position').textContent, '(29, 36)');
await walking.advance(90);
await arrival;
await walking.flush();
assert.deepEqual(walking.chops, [679], 'walking must preserve the clicked tree at a shared neighbor');

const manual = await fixture({ trees: sharedNeighborTrees, position: { x: 29, y: 36 } });
manual.chopAdjacent();
await manual.flush();
assert.deepEqual(manual.chops, [470], 'manual chopping must retain the adjacent-tree fallback');

const superseded = await fixture({
  trees: [...sharedNeighborTrees, { treeId: 900, x: 27, y: 35, health: 0, depleted: false }],
  position: { x: 29, y: 38 },
});
const oldMovement = superseded.clickMap(30, 36);
await superseded.clickMap(27, 35);
await superseded.flush();
await superseded.advance(180);
await oldMovement;
assert.deepEqual(superseded.regrowths, [900], 'new stump action must execute');
assert.deepEqual(superseded.chops, [], 'superseded movement must not submit its old tree chop');
assert.equal(superseded.element('hud-position').textContent, '(29, 37)', 'stump action must stop old movement');

const social = await fixture();
await social.setSocial(
  [{ playerAsset, xp: 25, level: 2, active: true }],
  [{ id: 1, playerAsset, message: 'hello woodland' }],
);
const leaderboardRow = social.element('leaderboard-rows').children[0];
const chatRow = social.element('chat-messages').children[0];
assert.ok(leaderboardRow.textContent.includes('(you)'));
assert.ok(chatRow.textContent.includes('hello woodland'));
const socialWalk = social.clickMap(3, 20);
await social.advance(270);
await socialWalk;
assert.equal(social.element('hud-position').textContent, '(3, 20)');
assert.equal(social.element('leaderboard-rows').children[0], leaderboardRow, 'walking must preserve leaderboard DOM');
assert.equal(social.element('chat-messages').children[0], chatRow, 'walking must preserve chat DOM and selection');
await social.setSocial(
  [{ playerAsset, xp: 75, level: 3, active: true }],
  [{ id: 2, playerAsset, message: 'new message' }],
);
assert.ok(social.element('leaderboard-rows').textContent.includes('75'), 'leaderboard updates must still render');
assert.ok(social.element('chat-messages').textContent.includes('new message'), 'chat updates must still render');
social.install({ ...social.current, playerAsset: null, playerActive: false });
assert.ok(!social.element('leaderboard-rows').textContent.includes('(you)'), 'identity changes must update social labels');

const backup = await fixture();
// A player lookup can temporarily render an inactive/null PLAYER_ID snapshot
// even though the wallet still owns the persistent recovery profile.
backup.install({ ...backup.current, playerAsset: null, playerActive: false });
backup.element('download-backup').click();
assert.equal(backup.blobs.length, 1, 'backup download was not produced');
const exported = JSON.parse(await backup.blobs[0].text());
assert.equal(exported.playerAsset, playerAsset, 'backup lost the persistent PLAYER_ID');
assert.equal(exported.secretKey, secretKey, 'backup changed the wallet key');
assert.equal(exported.genesisTxid, genesisTxid, 'backup changed worlds');
assert.equal(exported.walletAddress, backup.current.address, 'backup changed the receive address');

const pendingActivation = {
  playerAsset,
  transaction: { schemaVersion: 1, arkTx: 'fixture-signed-ark', checkpointTxs: ['fixture-checkpoint'] },
};
const activationBackup = await fixture({ profile: { pendingActivation } });
Object.assign(activationBackup.current, {
  playerActive: false, pendingActivationTxid: '55'.repeat(32), activationReady: true,
  activationBlockedReason: 'Activation pending; refresh or retry to recover. Do not deposit again.',
});
activationBackup.install({ ...activationBackup.current });
await activationBackup.element('activate').click();
assert.match(
  activationBackup.element('status').textContent,
  /pending.*recover/i,
  'a journaled but unresolved activation must not be reported as successful',
);
assert.equal(activationBackup.element('activate').disabled, false, 'pending activation must remain recoverable');
assert.equal(activationBackup.element('reset-profile').disabled, true, 'pending activation profile must not be discarded');
assert.equal(activationBackup.element('restore-backup').disabled, true, 'pending activation must not allow key replacement');
activationBackup.element('download-backup').click();
const activationExport = JSON.parse(await activationBackup.blobs[0].text());
assert.deepEqual(activationExport.pendingActivation, pendingActivation, 'backup must retain the exact activation journal');
assert.deepEqual(
  JSON.parse(JSON.stringify(activationBackup.parseBackup(JSON.stringify(activationExport)))).pendingActivation,
  pendingActivation,
  'backup parsing must preserve the activation journal for Rust recovery',
);

const mainnetWallet = await fixture({ network: 'bitcoin' });
mainnetWallet.storage.set(keyStorageKey, secretKey);
mainnetWallet.persistProfile();
mainnetWallet.install({ ...mainnetWallet.current, playerActive: false });
assert.equal(mainnetWallet.element('reset').hidden, true, 'mainnet must hide destructive wallet reset');
assert.equal(mainnetWallet.element('reset-profile').hidden, true, 'mainnet must hide destructive profile reset');
const mainnetSaved = [...mainnetWallet.storage];
mainnetWallet.element('reset').click();
mainnetWallet.element('reset-profile').click();
assert.deepEqual([...mainnetWallet.storage], mainnetSaved, 'even programmatic reset clicks must preserve mainnet custody');
assert.equal(mainnetWallet.confirmations(), 0);
assert.equal(mainnetWallet.reloads(), 0);

const pendingSwing = await fixture();
pendingSwing.storage.set(keyStorageKey, secretKey);
pendingSwing.persistProfile();
pendingSwing.install({ ...pendingSwing.current, playerActive: false, pendingChopTxid: '77'.repeat(32) });
assert.equal(pendingSwing.element('reset').disabled, true);
assert.equal(pendingSwing.element('reset-profile').disabled, true);
const pendingSaved = [...pendingSwing.storage];
pendingSwing.element('reset').click();
pendingSwing.element('reset-profile').click();
assert.deepEqual([...pendingSwing.storage], pendingSaved, 'unresolved swing must block destructive resets on test networks too');
assert.equal(pendingSwing.confirmations(), 0);
assert.equal(pendingSwing.reloads(), 0);

const refreshingWallet = await fixture();
refreshingWallet.storage.set(keyStorageKey, secretKey);
refreshingWallet.persistProfile();
const refreshingSaved = [...refreshingWallet.storage];
refreshingWallet.holdRefresh();
const inFlightPoll = refreshingWallet.poll();
await refreshingWallet.flush();
assert.equal(refreshingWallet.element('reset').disabled, true);
assert.equal(refreshingWallet.element('restore-backup').disabled, true);
refreshingWallet.element('reset').click();
assert.deepEqual([...refreshingWallet.storage], refreshingSaved, 'background wallet work must block key replacement');
assert.equal(refreshingWallet.confirmations(), 0);
refreshingWallet.releaseRefresh();
await inFlightPoll;
assert.equal(refreshingWallet.element('reset').disabled, false, 'reset controls must recover after polling finishes');
assert.equal(refreshingWallet.element('restore-backup').disabled, false);

const restoredAsset = `${'88'.repeat(32)}0000`;
const restoredKey = '07'.repeat(32);
const restoreJournalKey = 'woodland.sh:web:v2:restore:http://127.0.0.1:8090';
const replacementBackup = {
  ...exported, secretKey: restoredKey, walletAddress: 'tark1restored',
  playerAsset: restoredAsset, position: { x: 8, y: 17 },
};

const lostSwing = await fixture({ trees: sharedNeighborTrees, position: { x: 29, y: 36 } });
lostSwing.storage.set(keyStorageKey, secretKey);
lostSwing.failChopAfterJournal();
lostSwing.chopAdjacent();
await lostSwing.flush();
assert.match(lostSwing.element('status').textContent, /submission response lost/);
assert.equal(lostSwing.current.pendingChopTxid, undefined, 'failed submission must leave the rendered snapshot stale in this regression');
assert.equal(lostSwing.element('reset').disabled, true);
assert.equal(lostSwing.element('restore-backup').disabled, true);
const lostSwingSaved = [...lostSwing.storage];
lostSwing.element('reset').click();
await lostSwing.restoreBackup(replacementBackup);
assert.equal(lostSwing.restoreInitializations(), 0, 'durable pending swing must block restore before candidate initialization');
assert.equal(lostSwing.confirmations(), 0);
assert.deepEqual([...lostSwing.storage], lostSwingSaved, 'stale snapshots must not permit deleting submitted journals');
lostSwing.storage.delete(pendingKey); // Successful reconciliation removes its own journal.
lostSwing.install({ ...lostSwing.current });
assert.equal(lostSwing.element('restore-backup').disabled, false, 'reconciliation must release the replacement guard');
await lostSwing.runWalletAction(() => {});

const staleActivation = await fixture({ profile: { pendingActivation } });
staleActivation.persistProfile();
staleActivation.install({ ...staleActivation.current, playerActive: false });
assert.equal(staleActivation.current.pendingActivationTxid, undefined);
assert.equal(staleActivation.element('reset-profile').disabled, true, 'durable activation must protect a stale inactive snapshot');
await staleActivation.restoreBackup(replacementBackup);
assert.equal(staleActivation.restoreInitializations(), 0);
await staleActivation.runWalletAction(() => {}); // Recovery itself remains permitted.

const stagingFailure = await fixture();
stagingFailure.storage.set(keyStorageKey, secretKey);
stagingFailure.persistProfile();
const beforeFailedStage = [...stagingFailure.storage];
stagingFailure.failStorageWrite(restoreJournalKey);
await stagingFailure.restoreBackup(replacementBackup);
assert.deepEqual([...stagingFailure.storage], beforeFailedStage, 'failure to stage a restore must leave the original wallet untouched');
assert.match(stagingFailure.element('backup-status').textContent, /QuotaExceededError/);
assert.equal(stagingFailure.reloads(), 0);
await stagingFailure.runWalletAction(() => {});

const restoreOrigin = browserOrigin();
const interruptedRestore = await fixture({ coordination: restoreOrigin });
interruptedRestore.storage.set(keyStorageKey, secretKey);
interruptedRestore.persistProfile();
interruptedRestore.failStorageWrite(profileKey);
await interruptedRestore.restoreBackup(replacementBackup);
const journal = JSON.parse(interruptedRestore.storage.get(restoreJournalKey));
assert.equal(journal.secretKey, restoredKey);
assert.equal(JSON.parse(journal.profile).playerAsset, restoredAsset);
assert.equal(interruptedRestore.storage.has(profileKey), false, 'fixture must interrupt between key and profile writes');
assert.match(interruptedRestore.element('backup-status').textContent, /saved but unfinished/);
assert.equal(interruptedRestore.element('restore-backup').disabled, true);
assert.equal(interruptedRestore.element('address').textContent, '', 'failed restore must stop advertising the retired wallet address');
assert.equal(interruptedRestore.element('copy-address').disabled, true);
assert.equal(interruptedRestore.element('funding-instruction').textContent, '');
assert.equal(interruptedRestore.element('wallet-sats').textContent, '-');
assert.equal(interruptedRestore.element('player-asset').textContent, '');
assert.equal(interruptedRestore.element('player-state').textContent, 'Unavailable');
assert.equal(interruptedRestore.element('hud-logs').textContent, '-');
assert.equal(interruptedRestore.element('bag-panel').hidden, true);
assert.equal(interruptedRestore.element('stats-panel').hidden, true);
await assert.rejects(interruptedRestore.runWalletAction(() => {}), /restore is unfinished/);
assert.throws(() => interruptedRestore.persistProfile(), /restore is unfinished/);
const interruptedSaved = [...interruptedRestore.storage];
await interruptedRestore.poll();
assert.deepEqual([...interruptedRestore.storage], interruptedSaved, 'old in-memory wallet must not write after a staged restore');
interruptedRestore.close();
const recoveredRestore = await fixture({ coordination: restoreOrigin, installWallet: false });
assert.equal(recoveredRestore.initializations(), 1, 'bootstrap must recover the journal before initializing WASM');
assert.equal(recoveredRestore.storage.has(restoreJournalKey), false, 'only completed restore may clear its journal');
assert.equal(recoveredRestore.storage.get(keyStorageKey), restoredKey);
assert.equal(JSON.parse(recoveredRestore.storage.get(profileKey)).playerAsset, restoredAsset);
assert.equal(recoveredRestore.storage.has(pendingKey), false);

const woodenRecipe = await fixture();
woodenRecipe.install({
  ...woodenRecipe.current,
  nextAxeRecipe: { axe: 'wooden', requiredLevel: 1, logCost: 1, stoneCost: 0, ironOreCost: 0 },
});
assert.match(woodenRecipe.element('axe-recipe').textContent, /First successful chop \(25 XP\).*1 LOG/);
console.log('browser recovery: wallet writer coordination, durable journal guards, interrupted restore recovery, mainnet resets, renewal fallback, movement, presence, social rendering, and backups passed');
