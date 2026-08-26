#!/usr/bin/env node
// Renewal E2E: proves a world tree re-enters a fresh batch through its
// renewal leaf with a new expiry, identical contract, assets, and state, and
// that the renewed lineage keeps working for a second renewal.
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import fs from 'node:fs';
import path from 'node:path';
import { E2E_PROFILE, FULL_E2E, waitForHttp } from './e2e-runtime.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const MANIFEST = path.join(ROOT, 'regtest/_build/woodland-world.json');
const ARKD = 'http://127.0.0.1:7070';
const EMULATOR = 'http://127.0.0.1:7073';
const ROLLOVER_SECRET =
  '4444444444444444444444444444444444444444444444444444444444444444';
const TREE_ID = 417;

function renewArgs(args) {
  return [
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
    MANIFEST,
    ...args,
  ];
}

function renew(...args) {
  const output = execFileSync(
    'cargo',
    renewArgs(args),
    {
      cwd: ROOT,
      env: {
        ...process.env,
        WOODLAND_ROLLOVER_SECRET: ROLLOVER_SECRET,
        WOODLAND_FORCE_ROLLOVER: '1',
      },
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'inherit'],
    },
  );
  return JSON.parse(output.trim().split('\n').at(-1));
}

function renewAsync(...args) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      'cargo',
      renewArgs(args),
      {
        cwd: ROOT,
        env: {
          ...process.env,
          WOODLAND_ROLLOVER_SECRET: ROLLOVER_SECRET,
          WOODLAND_FORCE_ROLLOVER: '1',
        },
        stdio: ['ignore', 'pipe', 'inherit'],
      },
    );
    let output = '';
    child.stdout.on('data', (chunk) => { output += chunk.toString(); });
    child.on('error', reject);
    child.on('close', (code) => {
      if (code !== 0) {
        reject(new Error(`renewal process exited with status ${code}`));
        return;
      }
      try {
        resolve(JSON.parse(output.trim().split('\n').at(-1)));
      } catch (error) {
        reject(error);
      }
    });
  });
}

async function indexerVtxos(params) {
  const query = new URLSearchParams(params);
  const response = await fetch(`${ARKD}/v1/indexer/vtxos?${query}`);
  if (!response.ok) throw new Error(`indexer query failed: ${await response.text()}`);
  const payload = await response.json();
  return payload.vtxos || [];
}

async function virtualTxBytes(txid) {
  const response = await fetch(`${ARKD}/v1/indexer/virtualTx/${txid}`);
  if (!response.ok) throw new Error(`virtual tx fetch failed: ${await response.text()}`);
  const payload = await response.json();
  assert.equal(payload.txs?.length, 1, `expected one virtual tx for ${txid}`);
  const encoded = payload.txs[0];
  return /^[0-9a-f]+$/i.test(encoded) && encoded.length % 2 === 0
    ? Buffer.from(encoded, 'hex')
    : Buffer.from(encoded, 'base64');
}

function treeStatePacketBytes(state) {
  const encoded = Buffer.alloc(11);
  encoded.set([0x54, 0x52, 0x01]); // "TR" + version
  encoded.writeUInt32LE(state.treeId, 3);
  encoded.writeUInt16LE(state.x, 7);
  encoded.writeUInt16LE(state.y, 9);
  return encoded;
}

function outpoint(record) {
  return `${record.outpoint.txid}:${record.outpoint.vout}`;
}

function worldAssetTotals(records, treeAsset, logAsset, xpAsset) {
  return records.reduce(
    (sum, record) => {
      for (const asset of record.assets || []) {
        if (asset.assetId === treeAsset) sum.trees += Number(asset.amount);
        if (asset.assetId === logAsset) sum.logs += Number(asset.amount);
        if (asset.assetId === xpAsset) sum.xp += Number(asset.amount);
      }
      return sum;
    },
    { trees: 0, logs: 0, xp: 0 },
  );
}

async function main() {
  await Promise.all([
    waitForHttp(`${ARKD}/v1/info`),
    waitForHttp(`${EMULATOR}/v1/info`),
  ]);
  if (!fs.existsSync(MANIFEST)) {
    throw new Error('world manifest is missing; run ./scripts/regtest.sh start-tree first');
  }
  const blockedMaintenance = spawnSync(
    path.join(ROOT, 'scripts/regtest.sh'),
    ['maintain'],
    { cwd: ROOT, encoding: 'utf8' },
  );
  assert.notEqual(blockedMaintenance.status, 0, 'interactive maintenance unexpectedly ran');
  assert.match(blockedMaintenance.stderr, /maintain is pre-game only/);
  const manifest = JSON.parse(fs.readFileSync(MANIFEST, 'utf8'));
  assert.equal(manifest.schemaVersion, 1, 'world manifest must be schema 1');
  assert.equal(manifest.protocolVersion, 1, 'world manifest must declare protocol v1');
  assert.equal(manifest.gameId, 'woodland.sh');
  assert.equal(manifest.playerLevelCurve, 'woodland-xp-v1');
  assert.equal(manifest.maxPlayerLevel, 99);
  assert.equal(manifest.baseLogDropBasisPoints, 1_000);
  assert.equal(manifest.levelLogDropBonusBasisPoints, 100);
  assert.deepEqual(manifest.levelLogDropXpThresholds, [1_154, 4_470, 13_363, 37_224, 101_333]);
  assert.equal(manifest.maxLevelLogDropBasisPoints, 1_500);
  assert.equal(manifest.activeLogsPerTree, 5);
  assert.equal(manifest.logReservePerTree, 10);
  assert.equal(manifest.xpPerTree, 10);
  assert.equal(manifest.respawnMinSeconds, 20);
  assert.equal(manifest.respawnMaxSeconds, 40);
  assert.ok(manifest.maintenanceSigner, 'world manifest must pin a maintenance signer');
  assert.ok(manifest.rolloverSigner, 'world manifest must pin a rollover signer');
  const unauthorizedRollover = spawnSync(
    'cargo',
    renewArgs(['tree', String(TREE_ID)]),
    {
      cwd: ROOT,
      env: {
        ...process.env,
        WOODLAND_ROLLOVER_SECRET:
          '5555555555555555555555555555555555555555555555555555555555555555',
        WOODLAND_FORCE_ROLLOVER: '1',
      },
      encoding: 'utf8',
    },
  );
  assert.notEqual(unauthorizedRollover.status, 0, 'wrong rollover signer was accepted');
  assert.match(unauthorizedRollover.stderr, /rollover signer/i);
  assert.ok(manifest.treeRenewalArkadeScript, 'world manifest must pin the tree renewal leaf');
  const treeState = manifest.trees.find((tree) => tree.state.treeId === TREE_ID)?.state;
  assert.ok(treeState, `manifest omits tree ${TREE_ID}`);
  const treeScript = manifest.treeScript;
  const treeAsset = manifest.treeAsset;
  const logAsset = manifest.logAsset;
  const xpAsset = manifest.xpAsset;
  const assetInfo = async (assetId, label) => {
    const response = await fetch(`${ARKD}/v1/indexer/asset/${assetId}`);
    if (!response.ok) {
      throw new Error(`${label} asset query failed: ${await response.text()}`);
    }
    const info = await response.json();
    const metadata = Buffer.from(info.metadata, 'hex');
    for (const token of ['game', 'woodland.sh', 'protocol', '1', 'asset', label]) {
      assert.ok(metadata.includes(Buffer.from(token)), `${label} metadata omits ${token}`);
    }
    return { info, metadata };
  };
  await assetInfo(treeAsset, 'TREE');
  const { info: logAssetInfo } = await assetInfo(logAsset, 'LOG');
  const { info: xpAssetInfo } = await assetInfo(xpAsset, 'XP');
  assert.equal(logAssetInfo.controlAsset || '', '');
  assert.equal(xpAssetInfo.controlAsset || '', '');
  const worldBefore = await indexerVtxos({ scripts: treeScript, spendableOnly: 'true' });
  assert.equal(worldBefore.length, 10, 'all ten trees must be live before renewal');
  const totalsBefore = worldAssetTotals(worldBefore, treeAsset, logAsset, xpAsset);
  assert.equal(totalsBefore.trees, 10);
  assert.equal(totalsBefore.logs, totalsBefore.xp);
  assert.ok(totalsBefore.logs > 0 && totalsBefore.logs <= 100);

  // Join two renewals to the same batch. Each topic-filtered client receives
  // its own path while parent chunks retain omitted sibling references.
  const [first, peer] = await Promise.all([
    renewAsync('tree', String(TREE_ID)),
    renewAsync('tree', String(TREE_ID + 1)),
  ]);
  assert.equal(first.kind, 'tree');
  assert.equal(first.treeId, TREE_ID);
  assert.equal(first.commitmentTxid, peer.commitmentTxid, 'peer renewals must share one batch');
  assert.notEqual(first.newOutpoint, first.oldOutpoint, 'renewal must create a new outpoint');
  assert.ok(
    first.newExpiresAt > first.oldExpiresAt,
    `expiry must increase: ${first.oldExpiresAt} -> ${first.newExpiresAt}`,
  );
  assert.equal(peer.treeId, TREE_ID + 1);
  assert.notEqual(peer.newOutpoint, peer.oldOutpoint);
  assert.ok(peer.newExpiresAt > peer.oldExpiresAt);

  const [firstNewTxid] = first.newOutpoint.split(':');
  const leafBytes = await virtualTxBytes(firstNewTxid);
  assert.ok(
    leafBytes.includes(treeStatePacketBytes(treeState)),
    'renewed batch leaf must preserve the exact tree state packet',
  );
  assert.match(first.treeRoll, /^[0-9a-f]{64}$/);
  assert.ok(
    leafBytes.includes(Buffer.from(first.treeRoll, 'hex')),
    'renewed batch leaf must preserve the exact tree roll packet',
  );

  // The old VTXO is spent; the renewed one is the sole live tree 417.
  const [newTxid, newVout] = first.newOutpoint.split(':');
  const live = await indexerVtxos({ scripts: treeScript, spendableOnly: 'true' });
  const renewed = live.filter(
    (record) => record.outpoint.txid === newTxid && record.outpoint.vout === Number(newVout),
  );
  assert.equal(renewed.length, 1, 'renewed tree must be indexed exactly once');
  const previous = worldBefore.find((record) => outpoint(record) === first.oldOutpoint);
  assert.ok(previous, 'renewal input was not in the pre-renewal world');
  const previousHoldings = new Map(
    previous.assets.map((asset) => [asset.assetId, Number(asset.amount)]),
  );
  const holdings = new Map(renewed[0].assets.map((asset) => [asset.assetId, Number(asset.amount)]));
  assert.equal(holdings.get(treeAsset), previousHoldings.get(treeAsset));
  assert.equal(holdings.get(logAsset), previousHoldings.get(logAsset));
  assert.equal(holdings.get(xpAsset), previousHoldings.get(xpAsset));
  assert.equal(Number(renewed[0].amount), Number(previous.amount));
  assert.equal(renewed[0].script, treeScript, 'renewed tree keeps the world contract');

  const old = await indexerVtxos({ outpoints: first.oldOutpoint });
  assert.equal(old.length, 1, 'old tree outpoint must be indexed');
  assert.equal(old[0].isSpent, true, 'old tree VTXO must be spent');

  if (FULL_E2E) {
    // The full profile proves a batch-leaf output remains renewable.
    const second = renew('tree', String(TREE_ID));
    assert.notEqual(second.newOutpoint, first.newOutpoint);
    assert.ok(
      second.newExpiresAt > first.newExpiresAt,
      `second expiry must increase: ${first.newExpiresAt} -> ${second.newExpiresAt}`,
    );
  }

  // World-wide conservation is untouched by both renewals.
  const world = await indexerVtxos({ scripts: treeScript, spendableOnly: 'true' });
  assert.equal(world.length, 10, 'all ten trees remain live');
  const totals = worldAssetTotals(world, treeAsset, logAsset, xpAsset);
  assert.deepEqual(
    totals,
    totalsBefore,
    'world asset conservation holds',
  );

  console.log(
    `renewal E2E (${E2E_PROFILE}) passed: concurrent renewal preserved tree and roll state`
      + (FULL_E2E ? ' and remained renewable' : ''),
  );
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
