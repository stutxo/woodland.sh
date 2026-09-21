# woodland.sh Player Protocol v4

Protocol v4 requires a fresh genesis and schema-4 manifest. Never reuse v3
assets, player state, tree deployment outpoints, or manifests with this version.

## Purpose

A player is one owner-specific recursive VTXO. It holds 330 sats, exactly one
self-issued PLAYER_ID, a recursive player roll, bounded luck credit, permanent
axe tier, harvested LOG, and soulbound XP, STONE, and IRON ORE. There is no
carrier, PLAYER_TICKET, allocator, protocol registry, or player cap. When
configured, the game server indexes public state, presence, and chat, but has
no role in gameplay authorization.

```text
player state: D sats + 1 PLAYER_ID + player roll + luck credit + axe tier
              + optional LOG + optional XP + optional STONE + optional IRON ORE
D = 330 sats
```

## Permissionless Activation

A browser derives an ordinary Arkade address from its player key and receives one
exact `D`-sat VTXO. Activation is an owner-signed offchain transaction that both
issues a marker and creates the personalized player state. It attaches:

- one uncontrolled, one-unit `PLAYER_ID` at fresh asset group zero;
- metadata `game=woodland.sh`, `protocol=4`, `asset=PLAYER_ID`, and the owner;
- roll `SHA256("woodland.sh/player-roll/v2" || p2tr_witness_program)`;
- luck credit 8,000.
- axe tier packet `None`.

The PLAYER_ID AssetId is `(activation_txid, 0)`. The browser persists that exact
ID and the original signed Ark PSBT plus checkpoints before submission, then
discovers only state containing that one-unit asset. Reload and retry resume the
same journal, including checkpoint finalization after an accepted submission.
Historical settlement still proves activation if a later action has spent its
output. Anyone may fund more players or mint a decoy marker, but cannot reproduce
an existing transaction-derived ID.
For the reference client's direct issuance-to-state shape, every player
covenant leaf recognizes the first recursive spend because the PLAYER_ID
AssetId txid equals the state input's outpoint txid. On that spend it requires
the script-derived roll, AssetId group index zero, 8,000 credit, and axe tier
`None`. A malformed direct activation therefore cannot be laundered through
renewal, withdrawal, or crafting before chopping.

## XP Backing

XP has one protocol representation: the fixed-supply XP asset balance held by
the recursive player state. There is no numeric XP packet, duplicate counter,
or alternate encoding to forge:

```text
Woodcutting XP  =  25 × player XP asset balance
```

XP has no control asset and cannot be reissued. Every successful chop moves one
XP asset unit from the selected tree to player state and therefore awards 25
Woodcutting XP; every miss moves none.

Level is derived, never stored. The reachable canonical curve is
`woodland-xp-v1`: level 2 at 83 XP, reached by the fourth successful LOG. Chance
boundaries remain levels 10, 20, 30, 40, and 50
(1,154 / 4,470 / 13,363 / 37,224 / 101,333 Woodcutting XP), reached at XP
asset balances 47 / 179 / 535 / 1,489 / 4,054. Base LOG chance is 20% and rises
two percentage points per boundary to a 30% level cap. The equipped Wooden,
Stone, or Iron Axe adds 2%, 5%, or 8%, for an absolute 38% cap, while aggregate
XP asset supply remains fixed.

## Player-Bound Luck

The next reward belongs to the player lineage. A swing advances
`Rnext = SHA256(Rprevious)` and maps the successor to a little-endian bucket
modulo 10,000. Selecting, renewing, or regrowing another tree cannot change that bucket.

For input-XP-and-axe rate `p`, input credit `C`, `Q = C+p`, and reward bit `G`:

```text
G = 0                    if Q < 10,000
G = 1                    if Q > 20,000
G = (roll_bucket < p)    otherwise
Cnext = Q - 10,000*G
```

Credit is canonical, fixed-width, and constrained to 0–20,000. The identity
`Cnext + 10,000*G = C+p` keeps cumulative rewards within two units of expected
value. At 20%, every eleven-swing window contains a reward, so no miss run
exceeds ten. Success runs are bounded at two for rates up to one third and at
three for higher axe-enhanced rates, including the 38% cap. The hash chain is
public and predictable. This prevents tree-target grinding and bounds variance;
it does not prevent PLAYER_ID Sybils or provide hidden randomness.

## Successful-Swing Materials

A separate bucket commits material selection to the next player roll:

```text
B = SHA256("woodland.sh/material-roll/v1" || Rnext) mod 10,000
```

Before level 10, `B < 1,000` selects one STONE. From level 10 onward,
`B < 200` selects one IRON ORE and `200 <= B < 1,200` selects one STONE. The
tree covenant gates the selected material by `G`, so misses move no material
and the disjoint ranges can never move both.

## Soulbound Progression, Liquid LOG

XP, STONE, IRON ORE, and axe tier are soulbound by covenant. Chop pins inventory
deltas to the deterministic reward and material buckets. Both renewal leaves
preserve every inventory balance and the axe exactly. Withdrawal preserves XP,
STONE, IRON ORE, and axe, while crafting may only burn its exact declared LOG
and material recipe. There is no progression transfer path to another player
or ordinary output.

LOG is the liquid token. The fourth player tapleaf, authorized by the owner
plus the Arkade operator and tweaked emulator, withdraws an arbitrary amount
of LOG to any destination:

```text
inputs:  player state, owner wallet dust input
outputs: player state minus the withdrawn LOG, LOG destination, extension,
         anchor
groups:  PLAYER_ID, LOG, XP, STONE, IRON ORE
```

The covenant preserves the player P2TR, 330 sats, PLAYER_ID, roll, luck credit,
axe tier, and XP/STONE/IRON ORE balances exactly; the destination output is
funded entirely by the wallet dust input, never by player sats.

## Atomic Chop

A canonical swing has:

```text
input 0:  player, PLAYER_ID, XP X, LOG M, STONE S, IRON ORE I, axe A
input 1:  tree, TREE, XP F, LOG N, STONE T, IRON ORE O, health H > 0

output 0: player, PLAYER_ID, XP X+G, LOG M+G, STONE S+K,
          IRON ORE I+J, axe A
output 1: tree, TREE, XP F-G, LOG N-G, STONE T-K,
          IRON ORE O-J, health H-G
output 2: zero-value merged Ark extension
output 3: canonical zero-value anchor
```

`G` is the deterministic LOG reward bit; `K` and `J` are the gated STONE and
IRON ORE material bits. A success adds 25 user-facing Woodcutting XP. The six
Asset V1 groups are ordered `PLAYER_ID`, `TREE`, `LOG`, `XP`, `STONE`,
`IRON ORE`. Group zero is exactly one metadata-free, uncontrolled unit assigned
from input zero to output zero. Zero inventory assignments are omitted while
all five world groups remain present because the tree has positive inventory
before a swing.

The player covenant requires:

- the current input is player-state input zero;
- exactly two inputs and four outputs;
- recursive preservation of player script and `D` sats;
- the immutable world TREE AssetId and fixed tree value;
- canonical initial luck on the first recursive spend;
- exact roll hash successor and bounded luck-credit transition;
- player input/output assets contain exactly one PLAYER_ID and only optional
  LOG, XP, STONE, and IRON ORE besides it;
- equipped axe preservation;
- canonical extension and anchor.

The reciprocal tree covenant enforces the actual inventory and XP deltas and
authenticates the complete player Taproot template. Its witness supplies the
32-byte owner key and one compressed-output prefix byte (`0x02` or `0x03`).
The tree reconstructs all six canonical leaves and the NUMS internal key,
rejecting alternate scripts or additional escape paths. Pinning TREE rather
than tree P2TR in the player covenant breaks the otherwise circular dependency.

## Authorization

The player contract has five covenant tapleaves plus the Arkade CSV exit:

```text
chop:                player owner + Arkade operator + script-tweaked emulator
owner renewal:       player owner + Arkade operator + script-tweaked emulator
watchtower renewal:  rollover key + Arkade operator + script-tweaked emulator
LOG withdrawal:      player owner + Arkade operator + script-tweaked emulator
axe crafting:        player owner + Arkade operator + script-tweaked emulator
```

The owner key remains a Bitcoin Taproot signer; owner authorization is not
merely an emulator policy.

## Covenant-Enforced Axe Crafting

Crafting is a one-input/three-output recursive self-send: player state,
then merged extension and canonical anchor. It advances exactly one tier and
burns exactly the covenant-selected recipe:

| Next tier | Required level | Burn | LOG chance bonus |
| --- | ---: | --- | ---: |
| Wooden | 1 | 1 LOG | 2% |
| Stone | 5 | 2 LOG + 2 STONE | 5% |
| Iron | 15 | 5 LOG + 2 IRON ORE | 8% |

Wooden also requires one earned XP asset unit (25 Woodcutting XP, one
successful chop). The minimum held XP balances are 1, 16, and 97 units for
Wooden, Stone, and Iron. XP is never burned, and every equipped tier must remain
backed by its minimum XP balance. Stone and Iron keep their existing levels.

The craft leaf preserves state sats, P2TR, PLAYER_ID, XP, roll, luck, and every
unspent inventory asset. The next tier is permanent and automatically equipped;
the transaction cannot downgrade, skip a tier, unequip, or substitute a
different recipe.

## Direct Owner Renewal

Player state has one exact-state self-send covenant per authority:

- owner renewal;
- optional-watchtower renewal.

Every player has an Asset V1 packet, even at zero XP, because PLAYER_ID is
mandatory. The browser uses the owner leaf directly. It creates the version-2
intent, signs with the player key, obtains emulator approval, joins the Arkade
batch, contributes the ephemeral MuSig2 cosigner, verifies the generated trees
and forfeits, and confirms that expiry increased.

The optional watchtower leaf uses the dedicated rollover key and never receives
the player secret. The game server enables it only after a separate BIP340-signed
delegation and accepts a newer signed revocation. It can rotate only the exact
state into a new batch, but that outpoint change can race gameplay; unattended
operation is therefore near-expiry policy, not a requirement for active players.

If arkd's current or scheduled intent policy charges a fee, either path adds
one clean asset-free wallet VTXO and returns exact same-contract change. The
player's 330 sats and assets never fund the fee.

Every renewal preserves byte-for-byte:

- player P2TR and 330 sats;
- roll, luck-credit, and axe packets;
- the one-unit PLAYER_ID;
- LOG, XP, STONE, and IRON ORE balances.

## Browser Persistence

The reference frontend uses:

```text
woodland.sh:web:v2:key:<site-origin>
woodland.sh:web:v2:profile:<site-origin>:<genesis-txid>
woodland.sh:web:v2:position:<site-origin>
woodland.sh:web:v2:pending:<arkade-url>:<genesis-txid>
```

The profile pins the current genesis transaction and exact PLAYER_ID AssetId.
Unknown chop outcomes retain the exact prepared PSBT and reconcile indexed
state before another swing.

The signing key alone is insufficient for deterministic discovery when public
decoy states exist; production backups must preserve the profile's PLAYER_ID as
well as the key.

The browser's **Player backup & restore** control downloads one plaintext JSON
file containing both values plus the world identity. Keep it offline and secret:
anyone with the file controls the player and wallet. Restore validates the key
against its saved wallet address and reinstates the exact PLAYER_ID profile.

## Limits

The player count is unlimited, but season resources are not: the world contains
exactly 21,000,000 units each of LOG, XP, STONE, and IRON ORE, all issued into
420 tree-local reserves of 50,000 each. Those XP units represent 525,000,000
Woodcutting XP. Harvested LOG leaves player state through withdrawal or exact
craft burns. Materials leave only through exact craft burns; XP never leaves.
PLAYER_ID is world-specific. A new deployment has a fresh genesis and fresh
TREE/LOG/XP/STONE/IRON ORE AssetIds; an owner may reuse a key, but the
transaction-derived marker changes.

A covenant sees the current transaction, not arbitrary ancestry from before a
marker entered player state. An owner can issue a marker to an unconstrained
intermediate output and later choose any valid starting roll and credit. This
can bias reward timing inside the corridor, but cannot equip an axe without
the corresponding earned XP balance. Every recursive transition still
enforces player binding, the exact rate budget, the credit corridor, and streak
limits. The reference client always uses direct canonical activation.
