#!/usr/bin/env node
// Renewal E2E: proves a world tree re-enters a fresh batch through its
// rollover-authorized maintenance leaf with a new expiry, identical contract,
// assets, and state, and that the renewed lineage works a second time.
import { execFileSync, spawnSync } from 'node:child_process';
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
const EMULATOR = 'http://127.0.0.1:7073';
const TREE_ID = 417;

function treeRenewalEnv() {
  const environment = {
    ...process.env,
    WOODLAND_FORCE_ROLLOVER: '1',
  };
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

function renewalAddress() {
  const output = execFileSync(
    'cargo',
    [
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
      'renewal-address',
      MANIFEST,
    ],
    {
      cwd: ROOT,
      env: treeRenewalEnv(),
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'inherit'],
    },
  );
  return output.trim().split('\n').at(-1);
}

function fundRenewals() {
  execFileSync(
    path.join(ROOT, 'scripts/regtest.sh'),
    ['fund', renewalAddress(), '2000'],
    {
      cwd: ROOT,
      env: process.env,
      stdio: 'inherit',
    },
  );
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

function worldAssetTotals(records, treeAsset, logAsset, xpAsset, stoneAsset, ironOreAsset) {
  return records.reduce(
    (sum, record) => {
      for (const asset of record.assets || []) {
        if (asset.assetId === treeAsset) sum.trees += Number(asset.amount);
        if (asset.assetId === logAsset) sum.logs += Number(asset.amount);
        if (asset.assetId === xpAsset) sum.xp += Number(asset.amount);
        if (asset.assetId === stoneAsset) sum.stone += Number(asset.amount);
        if (asset.assetId === ironOreAsset) sum.ironOre += Number(asset.amount);
      }
      return sum;
    },
    { trees: 0, logs: 0, xp: 0, stone: 0, ironOre: 0 },
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
  assert.equal(manifest.schemaVersion, 3, 'world manifest must be schema 3');
  assert.equal(manifest.protocolVersion, 3, 'world manifest must declare protocol v3');
  assert.equal(manifest.gameId, 'woodland.sh');
  assert.equal(manifest.rulesetId, 'woodland.sh/forest/v3');
  assert.match(manifest.deployerSigner, /^[0-9a-f]{64}$/);
  assert.match(manifest.manifestSignature, /^[0-9a-f]{128}$/);
  assert.equal(manifest.playerLevelCurve, 'woodland-xp-v1');
  assert.equal(manifest.woodcuttingXpPerLog, 25);
  assert.equal(manifest.maxPlayerLevel, 99);
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
  assert.equal(manifest.activeLogsPerTree, 10);
  assert.equal(manifest.logReservePerTree, 50_000);
  assert.equal(manifest.xpPerTree, 50_000);
  assert.equal(manifest.stoneReservePerTree, 50_000);
  assert.equal(manifest.ironOreReservePerTree, 50_000);
  assert.ok(manifest.rolloverSigner, 'world manifest must pin a rollover signer');
  for (const removed of [
    'treeRetireArkadeScript',
    'vaultScript',
    'vaultRestockArkadeScript',
    'vaultRenewalArkadeScript',
  ]) {
    assert.equal(removed in manifest, false, `manifest retained obsolete ${removed}`);
  }
  assert.ok(
    manifest.treeMaintenanceArkadeScript,
    'world manifest must pin the tree maintenance leaf',
  );
  assert.ok(
    manifest.treeRegrowthArkadeScript,
    'world manifest must pin the tree regrowth leaf',
  );
  const treeState = manifest.trees.find((tree) => tree.state.treeId === TREE_ID)?.state;
  assert.ok(treeState, `manifest omits tree ${TREE_ID}`);
  const treeScript = manifest.treeScript;
  const treeAsset = manifest.treeAsset;
  const logAsset = manifest.logAsset;
  const xpAsset = manifest.xpAsset;
  const stoneAsset = manifest.stoneAsset;
  const ironOreAsset = manifest.ironOreAsset;
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
        ['ruleset', manifest.rulesetId],
        ['asset', label],
        ['deployer', manifest.deployerSigner],
        ['rollover', manifest.rolloverSigner],
      ],
      `${label} metadata changed`,
    );
    return info;
  };
  const treeAssetInfo = await assetInfo(treeAsset, 'TREE');
  const logAssetInfo = await assetInfo(logAsset, 'LOG');
  const xpAssetInfo = await assetInfo(xpAsset, 'XP');
  const stoneAssetInfo = await assetInfo(stoneAsset, 'STONE');
  const ironOreAssetInfo = await assetInfo(ironOreAsset, 'IRON ORE');
  assert.equal(treeAssetInfo.supply, '420', 'indexed TREE supply changed');
  // The preceding browser stage always crafts one Wooden Axe and its covenant
  // burns the exact first recipe. Issuance remains fixed; indexed circulating
  // supply records the burn.
  assert.equal(
    logAssetInfo.supply,
    String(21_000_000 - manifest.axeRecipes[0].logCost),
    'indexed LOG supply does not reflect the Wooden Axe recipe',
  );
  assert.equal(xpAssetInfo.supply, '21000000', 'indexed XP supply changed');
  assert.equal(stoneAssetInfo.supply, '21000000', 'indexed STONE supply changed');
  assert.equal(ironOreAssetInfo.supply, '21000000', 'indexed IRON ORE supply changed');
  assert.equal(logAssetInfo.controlAsset || '', '');
  assert.equal(xpAssetInfo.controlAsset || '', '');
  assert.equal(stoneAssetInfo.controlAsset || '', '');
  assert.equal(ironOreAssetInfo.controlAsset || '', '');
  fundRenewals();
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
  const totalsBefore = worldAssetTotals(
    worldBefore,
    treeAsset,
    logAsset,
    xpAsset,
    stoneAsset,
    ironOreAsset,
  );
  assert.equal(totalsBefore.trees, manifest.trees.length);
  assert.equal(totalsBefore.logs, totalsBefore.xp);
  assert.ok(
    totalsBefore.logs > 0
      && totalsBefore.logs <= manifest.logReservePerTree * manifest.trees.length,
  );
  assert.ok(
    totalsBefore.stone > 0
      && totalsBefore.stone <= manifest.stoneReservePerTree * manifest.trees.length,
  );
  assert.ok(
    totalsBefore.ironOre > 0
      && totalsBefore.ironOre <= manifest.ironOreReservePerTree * manifest.trees.length,
  );

  // Serialize fee-funded maintenance so the exact ordinary-wallet change from
  // one round funds the next without a conflicting double spend.
  const peer = renew('tree', String(TREE_ID + 1));
  const sibling = renew('tree', String(TREE_ID + 2));
  assert.equal(peer.kind, 'tree');
  assert.equal(peer.treeId, TREE_ID + 1);
  assert.equal(sibling.kind, 'tree');
  assert.equal(sibling.treeId, TREE_ID + 2);
  assert.notEqual(peer.newOutpoint, peer.oldOutpoint);
  assert.notEqual(sibling.newOutpoint, sibling.oldOutpoint);
  assert.ok(peer.newExpiresAt > peer.oldExpiresAt);
  assert.ok(sibling.newExpiresAt > sibling.oldExpiresAt);

  // Renew the target independently, then inspect its actual batch-leaf bytes.
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
  assert.equal(holdings.get(stoneAsset), previousHoldings.get(stoneAsset));
  assert.equal(holdings.get(ironOreAsset), previousHoldings.get(ironOreAsset));
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
  const totals = worldAssetTotals(
    world,
    treeAsset,
    logAsset,
    xpAsset,
    stoneAsset,
    ironOreAsset,
  );
  assert.deepEqual(
    totals,
    totalsBefore,
    'world asset conservation holds',
  );

  console.log(
    `renewal E2E (${E2E_PROFILE}) passed: recurring renewal preserved tree state`
      + (FULL_E2E ? ' and remained renewable' : ''),
  );
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
