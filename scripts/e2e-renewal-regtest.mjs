#!/usr/bin/env node
// Renewal E2E: proves a world tree re-enters a fresh batch through its
// renewal leaf with a new expiry, identical contract, assets, and state, and
// that the renewed lineage keeps working for a second renewal.
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import fs from 'node:fs';
import path from 'node:path';
import {
  decodeAssetMetadata,
  E2E_PROFILE,
  FULL_E2E,
  waitForHttp,
} from './e2e-runtime.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const MANIFEST = path.join(ROOT, 'regtest/_build/woodland-world.json');
const ARKD = 'http://127.0.0.1:7070';
const EMULATOR = 'http://127.0.0.1:7074';
const TREE_ID = 417;

function treeRenewalEnv() {
  const environment = {
    ...process.env,
    WOODLAND_FORCE_ROLLOVER: '1',
  };
  delete environment.WOODLAND_ROLLOVER_SECRET;
  return environment;
}

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
      env: treeRenewalEnv(),
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
        env: treeRenewalEnv(),
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
  const blockedRenewal = spawnSync(
    path.join(ROOT, 'scripts/regtest.sh'),
    ['renew-world'],
    { cwd: ROOT, encoding: 'utf8' },
  );
  assert.notEqual(blockedRenewal.status, 0, 'interactive renewal unexpectedly ran');
  assert.match(blockedRenewal.stderr, /renew-world is pre-game only/);
  const manifest = JSON.parse(fs.readFileSync(MANIFEST, 'utf8'));
  assert.equal(manifest.schemaVersion, 2, 'world manifest must be schema 2');
  assert.equal(manifest.protocolVersion, 2, 'world manifest must declare protocol v2');
  assert.equal(manifest.gameId, 'woodland.sh');
  assert.equal(manifest.playerLevelCurve, 'woodland-xp-v1');
  assert.equal(manifest.maxPlayerLevel, 99);
  assert.equal(manifest.baseLogDropBasisPoints, 2_000);
  assert.equal(manifest.levelLogDropBonusBasisPoints, 200);
  assert.deepEqual(
    manifest.levelLogDropXpThresholds,
    [1_154, 4_470, 13_363, 37_224, 101_333],
  );
  assert.equal(manifest.maxLevelLogDropBasisPoints, 3_000);
  assert.equal(manifest.luckWindowBasisPoints, 10_000);
  assert.equal(manifest.initialLuckCredit, 8_000);
  assert.equal(manifest.activeLogsPerTree, 10);
  assert.equal(manifest.logReservePerTree, 50_000);
  assert.equal(manifest.xpPerTree, 50_000);
  assert.ok(manifest.rolloverSigner, 'world manifest must pin a rollover signer');
  for (const removed of [
    'treeRetireArkadeScript',
    'vaultScript',
    'vaultRestockArkadeScript',
    'vaultRenewalArkadeScript',
  ]) {
    assert.equal(removed in manifest, false, `manifest retained obsolete ${removed}`);
  }
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
    assert.deepEqual(
      [...decodeAssetMetadata(info.metadata)],
      [
        ['game', 'woodland.sh'],
        ['protocol', String(manifest.protocolVersion)],
        ['asset', label],
      ],
      `${label} metadata changed`,
    );
    return info;
  };
  const treeAssetInfo = await assetInfo(treeAsset, 'TREE');
  const logAssetInfo = await assetInfo(logAsset, 'LOG');
  const xpAssetInfo = await assetInfo(xpAsset, 'XP');
  assert.equal(treeAssetInfo.supply, '420', 'indexed TREE supply changed');
  assert.equal(logAssetInfo.supply, '21000000', 'indexed LOG supply changed');
  assert.equal(xpAssetInfo.supply, '21000000', 'indexed XP supply changed');
  assert.equal(logAssetInfo.controlAsset || '', '');
  assert.equal(xpAssetInfo.controlAsset || '', '');
  const worldBefore = await indexerVtxos({ scripts: treeScript, spendableOnly: 'true' });
  assert.equal(
    worldBefore.length,
    manifest.trees.length,
    'all declared trees must be live before renewal',
  );
  for (const record of worldBefore) {
    assert.equal(Number(record.amount), 330, 'every tree holds exactly the dust value');
    const markers = (record.assets || []).filter((asset) => asset.assetId === treeAsset);
    assert.equal(markers.length, 1, 'every tree carries exactly one TREE marker');
    assert.equal(Number(markers[0].amount), 1, 'every tree carries exactly one TREE marker');
  }
  const totalsBefore = worldAssetTotals(worldBefore, treeAsset, logAsset, xpAsset);
  assert.equal(totalsBefore.trees, manifest.trees.length);
  assert.equal(totalsBefore.logs, totalsBefore.xp);
  assert.ok(
    totalsBefore.logs > 0
      && totalsBefore.logs <= manifest.logReservePerTree * manifest.trees.length,
  );

  // Join two untouched, equal-depth lineages to the same batch. Each
  // topic-filtered client receives its own path while parent chunks retain
  // omitted sibling references.
  const [peer, sibling] = await Promise.all([
    renewAsync('tree', String(TREE_ID + 1)),
    renewAsync('tree', String(TREE_ID + 2)),
  ]);
  assert.equal(peer.kind, 'tree');
  assert.equal(peer.treeId, TREE_ID + 1);
  assert.equal(sibling.treeId, TREE_ID + 2);
  assert.equal(peer.commitmentTxid, sibling.commitmentTxid, 'peer renewals must share one batch');
  assert.notEqual(peer.newOutpoint, peer.oldOutpoint);
  assert.notEqual(sibling.newOutpoint, sibling.oldOutpoint);
  assert.ok(peer.newExpiresAt > peer.oldExpiresAt);
  assert.ok(sibling.newExpiresAt > sibling.oldExpiresAt);

  // Independently renew the mutated target so lookup depth cannot decide
  // whether the peer intents reach the same Ark batch.
  const first = renew('tree', String(TREE_ID));
  assert.equal(first.kind, 'tree');
  assert.equal(first.treeId, TREE_ID);
  assert.notEqual(first.newOutpoint, first.oldOutpoint, 'renewal must create a new outpoint');
  assert.ok(
    first.newExpiresAt > first.oldExpiresAt,
    `expiry must increase: ${first.oldExpiresAt} -> ${first.newExpiresAt}`,
  );

  const [firstNewTxid] = first.newOutpoint.split(':');
  const leafBytes = await virtualTxBytes(firstNewTxid);
  assert.ok(
    leafBytes.includes(treeStatePacketBytes(treeState)),
    'renewed batch leaf must preserve the exact tree state packet',
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

  // World-wide conservation is untouched by every renewal.
  const world = await indexerVtxos({ scripts: treeScript, spendableOnly: 'true' });
  assert.equal(world.length, manifest.trees.length, 'all declared trees remain live');
  const totals = worldAssetTotals(world, treeAsset, logAsset, xpAsset);
  assert.deepEqual(
    totals,
    totalsBefore,
    'world asset conservation holds',
  );

  console.log(
    `renewal E2E (${E2E_PROFILE}) passed: concurrent renewal preserved tree state`
      + (FULL_E2E ? ' and remained renewable' : ''),
  );
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
