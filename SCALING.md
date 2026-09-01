# Scaling Decision: Protocol v3 Permissionless Player State

## Status

Protocol v3 uses signed schema 3, stock arkd and the stock Arkade Script
emulator, self-issued PLAYER_ID markers, unlimited permissionless activation,
owner renewal, permissionless funded-stump regrowth, rollover-authorized tree
maintenance, and fixed world LOG/XP supply.

## State Partitioning

The world contains:

- one recursive VTXO per player, holding 330 sats, one PLAYER_ID, player luck,
  LOG, and XP;
- one recursive VTXO per tree, holding TREE, remaining local LOG/XP, immutable
  state, and health;
- no shared supply vault, global player reserve, PLAYER_TICKET, allocator, or
  protocol registry.

A swing spends one player and one tree. Different players on different trees
share no inputs. Different players targeting the same tree contend only on that
tree. One player's sequential swings contend only on that player's state.

## Runtime Multiplayer State

woodland.sh has no server simulation tick. Arkade VTXOs are the authoritative
game state, and a chop is an atomic event rather than an input integrated into a
continuous physics simulation.

State ownership stays explicit:

```text
Arkade:         player/tree lineage, inventory, XP, health, player luck
browser:        camera, walking animation, focused tree
server durable: signed registration and renewal delegation consent
server live:    signed presence, bounded chat, replay clocks
derived:        leaderboard projected from registered live player VTXOs
```

The server's entire live multiplayer state is one `MultiplayerState`: a location
index, a 200-message chat deque, per-action replay timestamps, and the next chat
ID. Location is last-write-wins, expires after 60 seconds, and is queried by
viewport. It is intentionally ephemeral; a server restart can clear presence
and chat without changing gameplay.

This differs from server-authoritative room engines such as
[Colyseus](https://docs.colyseus.io/state) or
[Nakama](https://heroiclabs.com/docs/nakama/concepts/multiplayer/authoritative/),
which apply client input to an in-memory room state on a fixed tick and replicate
snapshots or patches. A second authoritative room state here would duplicate
Arkade, create a split-brain recovery problem, and add no useful simulation.
One-second signed HTTP presence is sufficient for the slow tile map; WebSockets,
prediction, interpolation, and rollback remain unnecessary.

## Capacity

Player count is not encoded in world asset supply. Activation consumes one
user-owned 330-sat VTXO, issues one unique uncontrolled marker, and creates one
owner-specific state. Capacity is bounded by Arkade throughput and
client/indexer resources, not a protocol ticket count.

Season rewards remain fixed:

```text
21,000,000 LOG
21,000,000 XP
```

Exactly 420 trees hold 50,000 of each asset at genesis, allocating both complete
21,000,000-unit supplies. Each tree can fund 5,000 ten-LOG health cycles; no
tree can draw from another tree. XP moves into player state rather than
disappearing, and it is soulbound — supply conservation also authenticates XP
because no covenant path transfers it. Unlimited players do not imply
unlimited rewards.

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
uses a separate exact-state leaf and can be horizontally sharded by owner,
PLAYER_ID, or outpoint.

Tree lifecycle still contends only per tree. Funded stumps use the
permissionless regrowth leaf; active trees and terminal stumps use exact-state
maintenance with the rollover signer. Different lineages remain independent,
and the watcher processes them concurrently with a fixed bound.

Any nonzero current or scheduled arkd intent fee adds one independent clean
wallet VTXO and same-contract change output. This increases renewal intent,
connector, and forfeit width but does not create a shared gameplay input or let
state value fund fees.

Delegated player renewals are exact per-player self-sends. The server processes
due delegations sequentially, so they share batch/service capacity but no
gameplay input.

## Indexing

Player contracts are owner-specific P2TR scripts. A browser queries its own
script and selects only a state carrying its profile's exact PLAYER_ID, so
public lookalikes do not create ambiguous state. It does not scan all players.
Tree discovery starts from the 420 manifest-pinned deployment outpoints and
follows each exact indexed successor. Shared-script pagination is not on the
browser path. Creating transactions are fetched in bounded parallel chunks only
for changed lineages, while snapshots serialize dynamic state for the current
viewport and merge it with the static manifest layout.

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

- Direct Arkade and emulator calls remove Woodland game-server availability and
  bandwidth from the player path.
- Loss of either the browser key or its PLAYER_ID profile prevents deterministic
  recovery; reference localStorage is not production custody.
- A player can self-renew without Woodland infrastructure.
- Renewal-watcher failure delays rollover-authorized active-tree maintenance
  and automatic funded-stump regrowth. Any caller can still regrow an eligible
  stump; active-tree maintenance requires the independent rollover authority.
  Player authorization is unaffected.
- Game-server failure hides social state and rankings; online clients fall back
  to owner renewal and gameplay remains direct.
- Operator or emulator retirement still strands NUMS-exit recursive state; no
  signer or service rotation is encoded.

## Launch Risks

Production still needs hardened key custody, service pin rotation policy,
stock-emulator support commitments, monitoring for emulator and tree-renewal
health, and UX for batch renewal latency. Player-bound deterministic rolls
prevent tree-target grinding and bounded credit limits streaks; both remain
public game mechanics, not fair hidden randomness.
Permissionless PLAYER_ID creation means Sybil and identity grinding remain
possible, and an intermediate marker output can choose any starting roll and
credit within the corridor before the recursive covenant takes control.
