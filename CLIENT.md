# woodland.sh Protocol v3 Client Guide

This guide describes interoperability with signed schema 3 worlds. The
reference browser is executable documentation; its animation and storage
choices are not protocol requirements.

## Validate the World

Load the canonical manifest and require:

```text
schemaVersion = 3
protocolVersion = 3
gameId = woodland.sh
rulesetId = woodland.sh/forest/v3
dustSats = 330
mapWidth = 425
mapHeight = 425
activeLogsPerTree = 10
logReservePerTree = 50000
xpPerTree = 50000
stoneReservePerTree = 50000
ironOreReservePerTree = 50000
woodcuttingXpPerLog = 25
playerLevelCurve = woodland-xp-v1
maxPlayerLevel = 99
baseLogDropBasisPoints = 2000
levelLogDropBonusBasisPoints = 200
levelLogDropXpThresholds = [1154, 4470, 13363, 37224, 101333]
maxLevelLogDropBasisPoints = 3000
maxLogDropBasisPoints = 3800
stoneDropBasisPoints = 1000
ironOreDropBasisPoints = 200
ironOreUnlockLevel = 10
axeRecipes = [
  {axe: wooden, requiredLevel: 1,  logCost: 1, stoneCost: 0, ironOreCost: 0},
  {axe: stone,  requiredLevel: 5,  logCost: 2, stoneCost: 2, ironOreCost: 0},
  {axe: iron,   requiredLevel: 15, logCost: 5, stoneCost: 0, ironOreCost: 2},
]
luckWindowBasisPoints = 10000
initialLuckCredit = 8000

The manifest pins direct `arkadeServiceUrl` and stock `emulatorUrl` values plus
the deployer, operator, emulator, and rollover keys. Before using any URL,
verify the BIP340 `manifestSignature` under `deployerSigner` over
`SHA256("woodland.sh/world-manifest/v3\0" || canonical_json)`, where
`canonical_json` sorts every object key and encodes `manifestSignature` as the
empty string.

Fetch both `/v1/info` resources directly and verify network, signer, exit delay,
version policy, 330-sat support, and extension support. Compile both current and
scheduled arkd intent fee programs. A renewal charged by either policy needs one
clean asset-free wallet VTXO and exact same-contract change.

Recompute the five asset IDs from the genesis txid:

```text
0 TREE
1 LOG
2 XP
3 STONE
4 IRON ORE
```

Fetch indexed metadata and require `game=woodland.sh`, `protocol=3`,
`ruleset=woodland.sh/forest/v3`, the exact asset label, deployer signer, and
rollover signer; also require no control asset and supply exactly 420 for TREE
and 21,000,000 for each inventory asset. Recompute the tree contract and compare
every committed script in the manifest.

The manifest must contain the reconstructed `treeScript`,
`treeChopArkadeScript`, `treeRegrowthArkadeScript`, and
`treeMaintenanceArkadeScript` exactly. Missing and unknown fields fail closed.

## Discover Trees

Start from every manifest-pinned deployment outpoint and fetch exact indexed
records. Follow direct Ark transaction successors or settlement-batch leaves
until each lineage reaches one unspent VTXO. Bind a batch leaf back to its tree
with the creating transaction's immutable `TreeState` packet; never infer state
from result order. For each declared tree, require exactly one lineage carrying:

- one TREE;
- zero through 50,000 LOG;
- the same amount of XP as LOG;
- zero through 50,000 STONE and IRON ORE;
- fixed 330 sats;
- canonical immutable-state and health packets.

Active trees have health one through ten. A health-zero tree with local LOG is
a funded stump; one regrowth batch resets it to ten. A zero-reserve stump is
terminal and can only use exact-state maintenance. There is no vault lineage.

## Activate Without a Woodland Service

Derive an ordinary Arkade address from the player key. Select one clean exact
330-sat VTXO and build a normal offchain transaction to the owner-specific
player contract. Add one fresh, uncontrolled issuance group with amount one
assigned to player output zero and metadata:

```text
game=woodland.sh
protocol=3
asset=PLAYER_ID
owner=<owner_xonly>
```

Attach initial roll
`SHA256("woodland.sh/player-roll/v2" || p2tr_witness_program)`, luck credit
8,000, and axe packet type 9 with value `None`. Compute the final unsigned txid,
define `PLAYER_ID = (txid, 0)`, and persist it before direct submission. On
synchronization, query the player script but accept only state carrying that
exact one-unit marker. No PLAYER_TICKET, allocator signature, invitation, or
protocol registry exists.

For that direct issuance-to-state shape, all recursive player leaves detect the
first spend by comparing the PLAYER_ID AssetId txid with the player input
outpoint txid. They require canonical AssetId group zero, initial roll, initial
credit, and axe tier `None` before allowing chop, renewal, withdrawal, or craft.

A canonical player state has exactly 330 sats, exactly one profile-selected
PLAYER_ID, player roll, bounded luck-credit, and axe packets, plus optional LOG,
XP, STONE, and IRON ORE balances. The XP asset balance is the sole level backing;
clients display `25 × balance` Woodcutting XP and derive level from that value.

Protocol v3 has exactly five gameplay state packet types:

```text
2  TREE_STATE
5  PLAYER_ROLL
7  TREE_HEALTH
8  PLAYER_LUCK_CREDIT
9  PLAYER_AXE
```

Types 3, 4, and 6 are retired. PLAYER_ID is an AssetId, not an identity packet;
XP and materials are world asset balances, not numeric state packets. Position
remains presentation and signed social-server state.
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
covenant, PLAYER_ID issuance, creating transaction, recursive lineage, and XP
asset balance.

Location, chat, and delegation use the same owner and PLAYER_ID with a second
signed message:

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
input XP and equipped axe; the selected tree is not part of this calculation.
Derive the independent material bucket from the next roll.

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
XP:        player X + tree F -> player X+G + tree F-G
STONE:     player S + tree T -> player S+K + tree T-K
IRON ORE:  player I + tree O -> player I+J + tree O-J
```

`G` is the LOG bit. `K` and `J` are the successful swing's disjoint STONE and
IRON ORE bits; both are zero on a miss. Attach preserved tree state and axe,
the next player roll and luck credit, next health, and both Arkade Script
entries. Sign player state and its checkpoint with the owner key. Submit
directly to the stock emulator, verify byte-identical unsigned transactions and
the exact signature matrix, then finalize through Arkade.

Automation should supply explicit expected tree outpoint, player-state outpoint,
LOG bit, and material outcome. Reject locally if any changed after refresh.

## Build an Axe Craft

Refresh player state and derive the sole recipe from its current axe tier.
Require the recipe's level and balances locally, but treat the covenant as
authoritative. Build one input and three outputs:

```text
input/output 0: player state
output 1:       merged extension
output 2:       anchor
```

Preserve PLAYER_ID, XP, roll, luck, state sats, and all inventory except the
exact recipe burn. Replace packet type 9 with the next tier, keep canonical
asset group order, and include no destination output. Execute with the craft
Arkade Script, owner signature, and checkpoint before finalizing through Arkade.
After settlement, verify the exact tier and balance deltas; an emulator success
response alone is not completion.

## Unknown Outcomes

Persist the exact Ark PSBT, checkpoint PSBTs, selected state/tree outpoints,
expected txid, LOG bit, and material outcome before emulator submission. A
transport failure, malformed success response, or HTTP 5xx after submission is
unknown, not rejected. Query the expected outputs first; when they are absent
and both inputs remain current, retry only the exact persisted PSBT. The
reference client does this after a bounded reconcile poll that gives the
original submission time to land; resubmitting against a still in-flight
original can trip the service's concurrent-spend protection.

The reference key is `woodland.sh:web:v2:pending:<arkade-url>:<genesis-txid>`.

## Direct Owner Renewal

Prepare an exact-state self-send version-2 intent. Group zero always preserves
the one-unit PLAYER_ID; append LOG, XP, STONE, and IRON ORE groups when present,
and preserve roll, luck, and axe packets byte-for-byte. Evaluate arkd's current
and scheduled fee programs. If either charges, add one clean asset-free wallet
VTXO and return its value minus the maximum required fee to the same wallet
contract. Never reduce player-state value or assets.

Sign with the player key, submit the intent to the emulator, and register it
directly with Arkade. Subscribe to `/v1/batch/events`, confirm registration,
validate VTXO and connector graphs, contribute MuSig2 nonces and partial
signatures, validate the commitment, obtain emulator forfeit signatures, and
submit final forfeits. The renewed output must preserve state exactly and have
a later indexed expiry. The optional rollover leaf permits a separate
watchtower to perform the same self-send without receiving the player secret.

## Permissionless Tree Regrowth

Refresh the selected tree. For a funded health-zero tree, build one version-2
regrowth intent: preserve TREE, LOG, XP, STONE, IRON ORE, immutable state,
script, and sats exactly, and set health to ten. No player state, player-key
authorization, timer, block-height witness, or woodland lifecycle key
participates. Active trees and terminal stumps instead require
rollover-authorized maintenance, which preserves health exactly.

## Headless Rust Client

Agents and tooling can drive the same protocol without a browser through
`woodland::client::WoodlandClient` (native, `woodland-app` feature):

```rust
let manifest = WorldManifest::from_json(&json)?;
let mut client = WoodlandClient::connect(&manifest, keys, player_asset).await?;
client.wallet_address()?;            // fund one exact 330-sat VTXO
let asset = client.activate().await?; // persist with the player key
let player = client.sync_player().await?;
let trees = client.trees(&[]).await?; // lineage heads and reserves decoded
let outcome = client.chop(tree_id).await?;
let craft = client.craft_axe().await?; // once the sole next recipe is ready
client.withdraw_log(500, None).await?; // LOG only; progression is soulbound
client.regrow(tree_id).await?;         // permissionless one-batch regrowth
let outpoint = client.renew_player().await?;
```

`connect` authenticates and validates the manifest against the live services
and pins the batch flow to its forfeit identity. `craft_axe` derives the only
next recipe, builds and signs its covenant self-send, and verifies the settled
tier and exact inventory burn. `withdraw_log` moves LOG to any Arkade address
(the player's own plain wallet by default), funded by a wallet dust input; the
covenant rejects anything that touches XP, materials, or axe. `regrow` recreates
an eligible funded stump with health ten while preserving its exact local
reserve; any caller may invoke it. `chop` builds, signs, emulator-executes, and
verifies the exact accepted LOG and material transition; a lost submission
response is reconciled against the settled lineage before any error is returned,
so callers resync rather than blind-resubmit. `trees` resolves declared trees to
current heads by immutable `TreeState`, never by indexer result order. The crate
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
regrow(treeId)
withdrawLog(amount)
craftAxe()
```

Snapshots expose `playerStone`, `playerIronOre`, `playerAxe`, `nextAxeRecipe`,
`craftAxeReady`, both material AssetIds, and each tree's material reserves.
Chop results include `material` (`none`, `stone`, or `ironOre`); clients must
render the settled result rather than predict it.

`chopExpected` and the mutation probes exist only in the `regtest-e2e` test
build; production snapshots expose observed state, never predicted outcomes.

## Limits

Movement and adjacency are frontend policy. Randomness is public and predictable.
Player count is unlimited, while LOG, XP, STONE, and IRON ORE season issuance
is fixed at 21,000,000 each. XP and materials are soulbound; exact crafting
recipes are their only burn path. LOG is liquid through the owner-authorized
withdraw leaf and also pays craft recipes. A covenant cannot authenticate
arbitrary PLAYER_ID ancestry before the marker entered recursive state;
intermediate-output activation can choose only a bounded starting luck phase.
The reference browser stores its key and PLAYER_ID profile in localStorage and
is not production custody.
