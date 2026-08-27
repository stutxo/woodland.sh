# Scaling Decision: Protocol v1 Permissionless Player State

## Status

Protocol v1 uses schema 1, stock arkd, self-issued PLAYER_ID markers, unlimited
permissionless activation, owner renewal, and fixed world LOG/XP supply.

## State Partitioning

The world contains:

- one recursive VTXO per player, holding 330 sats, one PLAYER_ID, state packets,
  LOG, and XP;
- one recursive VTXO per tree, holding TREE, remaining LOG/XP, health, and
  roll;
- no global player reserve, PLAYER_TICKET, allocator, or protocol registry.

A swing spends one player and one tree. Different players on different trees
share no inputs. Different players targeting the same tree contend only on that
tree. One player's sequential swings contend only on that player's state.

## Capacity

Player count is not encoded in world asset supply. Activation consumes one
user-owned 330-sat VTXO, issues one unique uncontrolled marker, and creates one
owner-specific state. Capacity is bounded by Arkade throughput and
client/indexer resources, not a protocol ticket count.

Season rewards remain fixed:

```text
100 LOG
100 XP
```

XP moves into player state rather than disappearing, so supply conservation
also authenticates XP. Unlimited players do not imply unlimited rewards.

## Transaction Cost

A swing has two inputs and four outputs:

```text
inputs:  player, tree
outputs: player, tree, extension, anchor
groups:  PLAYER_ID, TREE, LOG, XP
```

A miss has the same shape and signature cost as a hit. Player state has exactly
one marker and at most two inventory holdings, so per-player parsing remains
bounded.

## Activation

Activation has one input and three outputs after extension insertion:

```text
input:   clean 330-sat ordinary VTXO
outputs: recursive player state, extension, anchor
issuance: one PLAYER_ID assigned to player output zero
```

Issuance and state creation are one transaction. No shared activation input means
concurrent activations do not race. There is no server-side lock or allocator
reserve to serialize.

## Renewal

Owner renewal is per-player and therefore independent across players. The
browser participates directly in Arkade batch signing. The optional watchtower
uses a separate exact-self-send leaf and can be horizontally sharded by owner,
PLAYER_ID, or outpoint.

Tree renewal and regrowth contend per tree. The maintenance watcher can process
independent trees concurrently with a fixed concurrency bound.
Delegated player renewals are exact per-player self-sends. The server processes
due delegations sequentially, so they share batch/service capacity but no
gameplay input.

## Indexing

Player contracts are owner-specific P2TR scripts. A browser queries its own
script and selects only a state carrying its profile's exact PLAYER_ID, so
public lookalikes do not create ambiguous state. It does not scan all players.
Tree discovery remains one shared-script query followed by packet-based lineage
selection for ten declared trees.

The optional game server is deliberately outside this state machine. Its
registry is capped at 10,000 automatically registered players. Verification is
staggered in 256-player batches; delegated players and signed actions are
verified on demand instead of waiting for their batch.

Presence uses a 32×32 spatial chunk index. Clients query only their Canvas
viewport plus a small margin, with a 2,000-player response cap. Canvas rendering
touches visible tiles and nearby clusters rather than allocating one DOM node
per world tile or player. Chat is bounded to 200 in-memory messages, and the
leaderboard is paginated to at most 200 rows per request.

BIP340 signatures prevent third parties from registering another owner or
forging their location, chat, or delegation payload. They do not rate-limit
arbitrary HTTP clients or make owners unique humans; the public deployment still
needs reverse-proxy request limits.

## Failure Domains

- Direct Arkade/emulator calls remove Woodland proxy availability and bandwidth
  from the player path.
- Loss of either the browser key or its PLAYER_ID profile prevents deterministic
  recovery; reference localStorage is not production custody.
- A player can self-renew without Woodland infrastructure.
- Tree maintenance failure affects regrowth/expiry but not player authorization.
- Game-server failure hides social state and rankings; online clients fall back
  to owner renewal and gameplay remains direct.
- Operator or emulator retirement still strands NUMS-exit recursive state; no
  signer rotation is encoded.

## Launch Risks

Production still needs hardened key custody, service pin rotation policy,
emulator support commitments, monitoring for tree maintenance, and UX for batch
renewal latency. Public deterministic rolls are game mechanics, not fair hidden
randomness.
