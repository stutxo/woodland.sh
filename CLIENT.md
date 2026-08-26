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
mapWidth = 45
mapHeight = 19
activeLogsPerTree = 5
logReservePerTree = 10
xpPerTree = 10
playerLevelCurve = woodland-xp-v1
maxPlayerLevel = 99
```

The manifest pins direct `arkadeServiceUrl` and `emulatorUrl` values plus
operator, emulator, maintenance, and rollover keys. Fetch both `/v1/info`
resources directly and verify network, signer, exit delay, version policy,
330-sat support, extension support, and zero current/scheduled offchain fees.

Recompute the three asset IDs from the genesis txid:

```text
0 TREE
1 LOG
2 XP
```

Fetch indexed metadata and require `game=woodland.sh`, `protocol=1`, the exact
asset label, no control asset, and supply no greater than 10, 100, and 100.
Recompute the tree contract and compare every committed script in the manifest.

The manifest must contain the reconstructed `treeScript`,
`treeChopArkadeScript`, `treeRegrowArkadeScript`, and
`treeRenewalArkadeScript` exactly. Missing and unknown fields fail closed.

## Discover Trees

Query spendable VTXOs for `treeScript`. For each declared tree packet, require
exactly one lineage carrying:

- one TREE;
- zero through ten LOG;
- the same amount of XP as LOG;
- fixed 1,980 sats;
- valid identity, roll, and health packets.

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

Attach identity, spawn position `(3,17)`, and XP zero. Compute the final unsigned
txid, define `PLAYER_ID = (txid, 0)`, and persist it before direct submission.
On synchronization, query the player script but accept only state carrying that
exact one-unit marker. No PLAYER_TICKET, allocator signature, invitation, or
protocol registry exists.

A canonical player state has exactly 330 sats, exactly one profile-selected
PLAYER_ID, and optional LOG and XP asset balances. Its numeric XP packet must
equal the XP asset balance. Identity is:

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

## Build a Swing

Refresh state immediately before construction. Require player and tree inputs
outside the expiry safety margin. Compute the next tree roll and drop bit using
input XP.

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

Attach preserved player/tree packets, next roll, next health, next XP, and both
Arkade Script entries. Sign player state and its checkpoint with the owner key.
Submit to the emulator, verify byte-identical unsigned transactions and the
exact signature matrix, then finalize through Arkade.

Automation should supply explicit expected tree outpoint, player-state outpoint,
and drop bit. Reject locally if any changed after refresh.

## Unknown Outcomes

Persist the exact Ark PSBT, checkpoint PSBTs, selected state/tree outpoints,
expected txid, and drop bit before emulator submission. A transport error after
submission is unknown, not rejected. Query the expected state and tree outputs;
resume the exact PSBT only when they are absent and both inputs remain current.

The reference key is `woodland.sh:web:v1:pending:<arkade-url>`.

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
chopExpected(treeId, treeOutpoint, playerStateOutpoint, drop)
resumePendingChop()
renewPlayer()
```

## Limits

Movement and adjacency are frontend policy. Randomness is public and predictable.
Player count is unlimited, while LOG and XP season supply is fixed. The
reference browser stores its key and PLAYER_ID profile in localStorage and is
not production custody.
