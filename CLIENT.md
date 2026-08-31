# woodland.sh Protocol v1 Client Guide

This guide describes interoperability with schema 1 worlds. The reference
browser is executable documentation; its animation and storage choices are not
protocol requirements.

## Validate the World

Load the canonical manifest and require:

```text
schemaVersion = 1
protocolVersion = 1
gameId = woodland.sh
dustSats = 330
mapWidth = 425
mapHeight = 425
activeLogsPerTree = 5
logReservePerTree = 1000
xpPerTree = 1000
playerLevelCurve = woodland-xp-v1
maxPlayerLevel = 99
baseLogDropBasisPoints = 2000
levelLogDropBonusBasisPoints = 200
levelLogDropXpThresholds = [1154, 4470, 13363, 37224, 101333]
maxLevelLogDropBasisPoints = 3000
luckWindowBasisPoints = 10000
initialLuckCredit = 8000
```

The manifest pins direct `arkadeServiceUrl` and `emulatorUrl` values plus the
operator, emulator, and rollover keys. Fetch both `/v1/info` resources directly
and verify network, signer, exit delay, version policy, 330-sat support,
extension support, and zero current/scheduled offchain fees.

Recompute the three asset IDs from the genesis txid:

```text
0 TREE
1 LOG
2 XP
```

Fetch indexed metadata and require `game=woodland.sh`, `protocol=1`, the exact
asset label, no control asset, and supply exactly 2,100, 21,000,000, and
21,000,000. Recompute the tree contract and compare every committed script in
the manifest.

The manifest must contain the reconstructed `treeScript`,
`treeChopArkadeScript`, `treeRenewalArkadeScript`,
`treeRetireArkadeScript`, `vaultScript`, `vaultRestockArkadeScript`, and
`vaultRenewalArkadeScript` exactly. Missing and unknown fields fail closed.

## Discover Trees

Start from every manifest-pinned deployment outpoint and fetch exact indexed
records. Follow direct Ark transaction successors or settlement-batch leaves
until each lineage reaches one unspent VTXO. Bind a batch leaf back to its tree
with the creating transaction's immutable identity packet; never infer identity
from result order. For each declared tree, require exactly one lineage carrying:

- one TREE;
- zero through 1,000 LOG;
- the same amount of XP as LOG;
- fixed 330 sats;
- valid identity and health packets.

A tree with zero LOG is depleted and awaits restock. Follow the supply vault
lineage from its manifest-pinned deployment outpoint the same way: the live
vault VTXO carries the undistributed LOG and XP, and every restock moves
exactly one tree reserve (1,000 of each) out of it.

## Activate Without a Woodland Service

Derive an ordinary Arkade address from the player key. Select one clean exact
330-sat VTXO and build a normal offchain transaction to the owner-specific
player contract. Add one fresh, uncontrolled issuance group with amount one
assigned to player output zero and metadata:

```text
game=woodland.sh
protocol=1
asset=PLAYER_ID
owner=<owner_xonly>
```

Attach identity, spawn position `(3,17)`, XP zero, initial roll
`SHA256("woodland.sh/player-roll/v1" || identity_packet)`, and luck credit 8,000.
Compute the final unsigned txid, define `PLAYER_ID = (txid, 0)`, and persist it
before direct submission. On synchronization, query the player script but
accept only state carrying that exact one-unit marker. No PLAYER_TICKET,
allocator signature, invitation, or protocol registry exists.

For that direct issuance-to-state shape, all recursive player leaves detect the
first spend by comparing the PLAYER_ID AssetId txid with the player input
outpoint txid. They require canonical AssetId group zero, initial roll, and
initial credit before allowing chop, renewal, or withdrawal.

A canonical player state has exactly 330 sats, exactly one profile-selected
PLAYER_ID, player roll and bounded luck-credit packets, and optional LOG and XP
asset balances. Its numeric XP packet must equal the XP asset balance.
Identity is:

```text
SHA256("woodland.sh/PlayerIdentity/v1" || owner_xonly || genesis_txid)
```

## Optional Game Server

An alternate client joins a configured server by BIP340-signing the SHA256
digest of this exact UTF-8 message, including its final newline:

```text
woodland.sh/ServerRegistration/v1
world=<genesisTxid>
owner=<owner x-only public key>
playerAsset=<PLAYER_ID>
server=<canonical server origin>
```

POST `{owner, playerAsset, signature}` to `/v1/players`. The server binds the
proof to its configured public origin and independently verifies the live
covenant, PLAYER_ID issuance, creating transaction, identity, and XP backing.

Location, chat, and delegation use the same identity with a second signed
message:

```text
woodland.sh/ServerAction/v1
world=<genesisTxid>
owner=<owner x-only public key>
playerAsset=<PLAYER_ID>
server=<canonical server origin>
action=<location|chat|delegation>
timestampMs=<unix milliseconds>
payloadHash=<SHA256 of endpoint-specific UTF-8 payload>
```

Location payload is `x=<x>\ny=<y>\n`; chat payload is the exact message;
delegation payload is `enabled=<true|false>\n`. Requests include those payload
fields plus `timestampMs` and `signature`. The server accepts only active
registered players, a five-minute clock window, and increasing action
timestamps. Delegated renewal uses the rollover leaf and preserves player state
exactly; it never gives the server the player key.

Read APIs are deliberately split by update rate:

```text
GET /v1/presence?minX=&minY=&maxX=&maxY=
GET /v1/chat
GET /v1/leaderboard?limit=&offset=
```

Presence bounds must fit within the declared map and span at most 128 tiles per
axis. Responses contain at most 2,000 nearby players and report truncation.
Leaderboard limits are capped at 200.

## Build a Swing

Refresh state immediately before construction. Require player and tree inputs
outside the expiry safety margin. Advance the player roll and luck credit using
input XP; the selected tree is not part of this calculation.

```text
input/output 0: player state
input/output 1: tree
output 2:       merged extension
output 3:       anchor
```

Build groups in order:

```text
PLAYER_ID: player 1 -> player 1
TREE:      tree 1 -> tree 1
LOG:       player M + tree N -> player M+G + tree N-G
XP:   player X + tree F -> player X+G + tree F-G
```

Attach preserved player/tree packets, the next player roll and luck credit,
next health, next XP, and both Arkade Script entries. Sign player state and its
checkpoint with the owner key. Submit to the emulator, verify byte-identical
unsigned transactions and the exact signature matrix, then finalize through Arkade.

Automation should supply explicit expected tree outpoint, player-state outpoint,
and drop bit. Reject locally if any changed after refresh.

## Unknown Outcomes

Persist the exact Ark PSBT, checkpoint PSBTs, selected state/tree outpoints,
expected txid, and drop bit before emulator submission. A transport failure,
malformed success response, or HTTP 5xx after submission is unknown, not
rejected. Query the expected outputs first; when they are absent and both inputs
remain current, retry only the exact persisted PSBT. The reference client does
this after a bounded reconcile poll that gives the original submission time to land, and then on a background cadence; resubmitting against a still in-flight original can trip the service's concurrent-spend protection.

The reference key is `woodland.sh:web:v1:pending:<arkade-url>:<genesis-txid>`.

## Direct Owner Renewal

Prepare an exact-self-send version-2 intent. Group zero always preserves the
one-unit PLAYER_ID; append LOG and XP groups when present. Sign with the
player key, submit the intent to the emulator, and register it directly with
Arkade.

Subscribe to `/v1/batch/events`, confirm registration, validate VTXO and
connector graphs, contribute MuSig2 nonces and partial signatures, validate the
commitment, obtain emulator forfeit signatures, and submit final forfeits. The
renewed output must preserve state exactly and have a later indexed expiry.

The optional rollover leaf permits a separate watchtower to perform the same
self-send without receiving the player secret.

## Headless Rust Client

Agents and tooling can drive the same protocol without a browser through
`woodland::client::WoodlandClient` (native, `woodland-app` feature):

```rust
let manifest = WorldManifest::from_json(&json)?;
let mut client = WoodlandClient::connect(&manifest, keys, player_asset).await?;
client.wallet_address()?;            // fund one exact 330-sat VTXO
let asset = client.activate().await?; // persist with the player key
let player = client.sync_player().await?;
let trees = client.trees(&[]).await?; // every lineage head, logs/health decoded
let outcome = client.chop(tree_id).await?;
client.withdraw_log(500, None).await?; // LOG only; XP is soulbound
client.restock(tree_id).await?;        // replace a depleted tree from the vault
let outpoint = client.renew_player().await?;
```

`connect` validates the manifest against the live services and pins the batch
flow to its forfeit identity. `withdraw_log` moves LOG to any Arkade address
(the player's own plain wallet by default), funded by a wallet dust input; the
covenant rejects anything that touches XP. `restock` atomically retires one
depleted tree against the supply vault and recreates it at the same
coordinate; any caller may invoke it. `chop` builds, signs, emulator-executes,
and verifies the exact accepted transition; a lost submission response is
reconciled against the settled lineage before any error is returned, so
callers resync rather than blind-resubmit. `trees` resolves declared trees to
current heads by identity packet, never by indexer result order. The crate
re-exports the concrete transport (`arkade`), transaction (`txbuild`), world
(`world`), chop (`chop`), renewal (`renewal`), and batch (`batch`) modules for
custom clients. `examples/woodland-agent.rs` is a complete season-playing bot:

```bash
cargo run --example woodland-agent --features woodland-app -- \
  <world-manifest.json> [player-key-hex] [player-asset-id]
```

## Reference WASM API

`WoodlandApp` exposes:

```text
WoodlandApp.init(arkadeUrl, emulatorUrl, manifestJson, secret?, profile?)
exportKey()
exportProfile()  # genesisTxid plus exact playerAsset after activation
exportPendingChop()
activate()
refresh()
chop(treeId)
resumePendingChop()
renewPlayer()
withdrawLog(amount)
```

`chopExpected` and the mutation probes exist only in the `regtest-e2e` test
build; production snapshots expose observed state, never predicted outcomes.

## Limits

Movement and adjacency are frontend policy. Randomness is public and predictable.
Player count is unlimited, while LOG and XP season supply is fixed at
21,000,000 each. XP is soulbound — no covenant path transfers it; LOG is the
liquid asset through the owner-authorized withdraw leaf. A covenant cannot
authenticate arbitrary PLAYER_ID ancestry before the marker entered recursive
state; intermediate-output activation can choose only a bounded starting luck
phase. The reference browser stores its key and PLAYER_ID profile in
localStorage and is not production custody.
