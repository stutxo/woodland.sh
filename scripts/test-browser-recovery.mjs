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

async function fixture() {
  const elements = new Map();
  const intervals = new Map();
  const storage = new Map();
  const blobs = [];
  function element(id) {
    if (elements.has(id)) return elements.get(id);
    const listeners = new Map();
    const classes = new Set();
    const node = {
      textContent: '', value: '', hidden: false, disabled: false,
      clientWidth: 0, clientHeight: 0, style: {},
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
      append() {}, replaceChildren() {}, remove() {}, setAttribute() {},
    };
    elements.set(id, node);
    return node;
  }
  let available = true;
  let renewalError = null;
  let renewals = 0;
  const current = {
    genesisTxid, address: 'tark1localfixture', playerAsset,
    playerActive: true, playerStateOutpoint: `${'33'.repeat(32)}:0`,
    playerStateExpiresInSeconds: 30, playerRolloverMarginSeconds: 300,
    playerXp: 0, playerLogs: 0, playerStone: 0, playerIronOre: 0,
    playerAxe: 'none', playerLevel: 1, mapWidth: 425, mapHeight: 425,
    trees: [], fundingRequiredSats: 0, walletSats: 350,
    dustSats: 330, walletVtxos: [{ amountSats: 20, assets: [] }],
    emulatorVersion: 'fixture', emulatorSigner: '44'.repeat(32),
  };
  const app = {
    exportKey: () => secretKey,
    exportProfile: () => JSON.stringify({ genesisTxid, playerAsset }),
    setTreeViewport() {},
    refresh: async () => ({ ...current }),
    refreshWorld: async () => ({ ...current }),
    renewPlayer: async () => {
      renewals += 1;
      if (renewalError) throw new Error(renewalError);
      return { ...current };
    },
    address: () => current.address,
  };
  class HostURL extends URL {
    static createObjectURL(blob) { blobs.push(blob); return 'blob:local-backup'; }
    static revokeObjectURL() {}
  }
  const context = vm.createContext({
    console, URL: HostURL, URLSearchParams, Blob,
    location: { origin: 'http://127.0.0.1:8090' },
    window: { addEventListener() {} },
    document: {
      getElementById: element,
      querySelector: () => ({ content: 'self' }),
      createElement: (tag) => element(Symbol(tag)),
      body: { append() {} },
    },
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value),
      removeItem: (key) => storage.delete(key),
    },
    setTimeout() {}, clearTimeout() {},
    setInterval: (callback, delay) => intervals.set(delay, callback),
    fetch: async (url) => {
      assert.equal(new URL(url).pathname, '/v1/leaderboard');
      return { ok: true, json: async () => ({
        delegationAvailable: available, delegatedPlayerAssets: [playerAsset], players: [],
      }) };
    },
  });
  const module = new vm.SourceTextModule(`${source}\n
    export function installFixture(wallet, snapshot, manifest) {
      app = wallet;
      state = snapshot;
      worldManifest = manifest;
      busy = false;
      serverRegistered = true;
      serverRegistrationOutpoint = snapshot.playerStateOutpoint;
    }
    export { refreshLeaderboard };
  `, {
    context,
    identifier: sourceUrl.href,
    initializeImportMeta: (meta) => { meta.url = sourceUrl.href; },
  });
  const wasm = new vm.SyntheticModule(['default', 'WoodlandApp'], function () {
    // Leave automatic boot waiting: these cases supply only the wallet boundary,
    // not a synthetic game. All scheduling, delegation and backup code is real.
    this.setExport('default', () => new Promise(() => {}));
    this.setExport('WoodlandApp', {});
  }, { context });
  await module.link((specifier) => {
    assert.equal(specifier, './pkg/woodland.js');
    return wasm;
  });
  await module.evaluate();
  const install = (snapshot) => module.namespace.installFixture(app, snapshot, {
    gameId: 'woodland.sh', protocolVersion: 3, network: 'regtest', genesisTxid,
    mapWidth: 425, mapHeight: 425,
  });
  install({ ...current });
  return {
    current, install, element, blobs,
    renewals: () => renewals,
    failRenewal: (message) => { renewalError = message; },
    setDelegationAvailable: async (value) => {
      available = value;
      await module.namespace.refreshLeaderboard();
    },
    poll: () => intervals.get(10_000)(),
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
console.log('browser recovery: delegation fallback, visible renewal failure, persistent PLAYER_ID backup passed');
