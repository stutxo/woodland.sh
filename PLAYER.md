# woodland.sh Player Protocol v2

## Purpose

A player is one owner-specific recursive VTXO. It holds 330 sats, exactly one
self-issued PLAYER_ID, immutable identity and position packets, a recursive
player roll, bounded luck credit, numeric XP, harvested LOG, and earned XP.
There is no carrier, PLAYER_TICKET, allocator, protocol registry, or player cap.
When configured, the game server indexes public state, presence, and chat, but
has no role in gameplay authorization.

```text
player state: D sats + 1 PLAYER_ID + identity + position + player roll
              + luck credit + numeric XP + optional LOG + optional XP asset
D = 330 sats
```

## Permissionless Activation

A browser derives an ordinary Arkade address from its player key and receives one
exact `D`-sat VTXO. Activation is an owner-signed offchain transaction that both
issues a marker and creates the personalized player state. It attaches:

- one uncontrolled, one-unit `PLAYER_ID` at fresh asset group zero;
- metadata `game=woodland.sh`, `protocol=2`, `asset=PLAYER_ID`, and the owner;
- identity derived from `SHA256("woodland.sh/PlayerIdentity/v1" || owner || genesis_txid)`;
- spawn position `(3,17)`;
- roll `SHA256("woodland.sh/player-roll/v1" || identity_packet)`;
- luck credit 8,000 and canonical numeric XP zero.

The PLAYER_ID AssetId is `(activation_txid, 0)`. The browser persists that exact
ID before submission, then discovers only state containing that one-unit asset.
A failed submission can be rebuilt deterministically; a submitted-but-unknown
transaction can be reconciled by the same marker. Anyone may fund more players
or mint a decoy marker, but cannot reproduce an existing transaction-derived ID.
For the reference client's direct issuance-to-state shape, every player
covenant leaf recognizes the first recursive spend because the PLAYER_ID
AssetId txid equals the state input's outpoint txid. On that spend it requires
the identity-derived roll, AssetId group index zero, and 8,000 credit. A
malformed direct activation therefore cannot be laundered through renewal or
withdrawal before chopping.

## XP Backing

`PlayerXp` is a checked `u64` encoded as eight little-endian value bytes plus a
zero sign byte. Alternate numeric encodings, including negative zero, fail.

XP must equal the amount of fixed-supply XP held by player state:

```text
numeric XP X  <=>  player state owns X XP
```

A forged activation with a high XP packet but no XP cannot chop: both
player and tree covenants compare the packet to the state asset balance.
XP has no control asset and cannot be reissued.

Level is derived, never stored. The reachable canonical curve is
`woodland-xp-v1`: level 2 at 83 XP, with chance boundaries at levels 10, 20,
30, 40, and 50 (1,154 / 4,470 / 13,363 / 37,224 / 101,333 XP). Base LOG
chance is 20% and rises two percentage points per boundary to a 30% cap while
aggregate XP remains fixed and asset-backed.

## Player-Bound Luck

The next reward belongs to the player lineage. A swing advances
`Rnext = SHA256(Rprevious)` and maps the successor to a little-endian bucket
modulo 10,000. Selecting, renewing, or regrowing another tree cannot change that bucket.

For input-XP rate `p`, input credit `C`, `Q = C+p`, and reward bit `G`:

```text
G = 0                    if Q < 10,000
G = 1                    if Q > 20,000
G = (roll_bucket < p)    otherwise
Cnext = Q - 10,000*G
```

Credit is canonical, fixed-width, and constrained to 0–20,000. The identity
`Cnext + 10,000*G = C+p` keeps cumulative rewards within two units of expected
value. At 20%, every eleven-swing window contains a reward, so no miss run
exceeds ten; no rate permits more than two consecutive successes. The hash
chain is public and predictable. This prevents tree-target grinding and bounds
variance; it does not prevent PLAYER_ID Sybils or provide hidden randomness.

## Soulbound XP, Liquid LOG

XP is soulbound by covenant: no leaf moves it out of player state. The chop
covenants pin XP deltas to the deterministic drop bit, both renewal leaves
preserve the XP balance and packet exactly, and the LOG withdrawal leaf
requires the XP balance and packet to survive unchanged. There is no transfer
path at all — not to other players, not to ordinary outputs — so a level can
never be bought, sold, or pooled.

LOG is the liquid token. The fourth player tapleaf, authorized by the owner
plus the Arkade operator and tweaked emulator, withdraws an arbitrary amount
of LOG to any destination:

```text
inputs:  player state, owner wallet dust input
outputs: player state minus the withdrawn LOG, LOG destination, extension,
         anchor
groups:  PLAYER_ID, LOG, XP
```

The covenant preserves the player P2TR, 330 sats, PLAYER_ID, identity,
position, XP packet, and XP balance exactly; the destination output is funded
entirely by the wallet dust input, never by player sats.

## Atomic Chop

A canonical swing has:

```text
input 0:  player state, 1 PLAYER_ID, X XP, M LOG
input 1:  tree, one TREE, F XP, N LOG, health H > 0

output 0: same player state, 1 PLAYER_ID, X+G XP, M+G LOG
output 1: same tree, F-G XP, N-G LOG, health H-G
output 2: zero-value merged Ark extension
output 3: canonical zero-value anchor
```

`G` is the deterministic reward bit derived from player roll, luck credit, and
input XP. The four Asset V1 groups are ordered `PLAYER_ID`, `TREE`, `LOG`, `XP`.
Group zero is exactly one metadata-free, uncontrolled unit assigned from input
zero to output zero. Zero LOG/XP assignments are omitted while those world
groups remain present because the tree has positive inventory before a swing.

The player covenant requires:

- the current input is player-state input zero;
- exactly two inputs and four outputs;
- recursive preservation of player script and `D` sats;
- the exact shared tree script and fixed tree value;
- preserved identity and position;
- canonical initial luck on the first recursive spend;
- exact roll hash successor and bounded luck-credit transition;
- player input/output assets contain exactly one PLAYER_ID and only optional LOG
  and XP besides it;
- input and output XP packets equal their corresponding XP balances;
- canonical extension and anchor.

The reciprocal tree covenant enforces the actual inventory and XP deltas.

## Authorization

The player contract has four tapleaves:

```text
chop:                player owner + Arkade operator + script-tweaked emulator
owner renewal:       player owner + Arkade operator + script-tweaked emulator
watchtower renewal:  rollover key + Arkade operator + script-tweaked emulator
LOG withdrawal:      player owner + Arkade operator + script-tweaked emulator
```

The owner key remains a Bitcoin Taproot signer; owner authorization is not
merely an emulator policy.

## Direct Owner Renewal

Player state has one exact-self-send covenant per authority:

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

Every renewal preserves byte-for-byte:

- player P2TR and 330 sats;
- identity, position, roll, luck-credit, and XP packets;
- the one-unit PLAYER_ID;
- LOG and XP balances.

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

The player count is unlimited, but season resources are not: the world
contains exactly 21,000,000 LOG and 21,000,000 XP, all issued into 420
tree-local reserves of 50,000 each. Harvested LOG leaves player state through
the owner-authorized withdrawal leaf; XP never leaves.
PLAYER_ID and player identity are world-specific. A new deployment has a fresh
genesis and fresh TREE/LOG/XP AssetIds; an owner may reuse a key, but the
genesis-bound identity and marker change.

A covenant sees the current transaction, not arbitrary ancestry from before a
marker entered player state. An owner can issue a marker to an unconstrained
intermediate output and later choose any valid starting roll and credit. This
can bias reward timing inside the corridor, but every recursive transition still
enforces player binding, the exact rate budget, the credit corridor, and streak
limits. The reference client always uses direct canonical activation.
