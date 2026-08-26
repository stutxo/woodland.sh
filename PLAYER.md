# woodland.sh Player Protocol v1

## Purpose

A player is one owner-specific recursive VTXO. It holds 330 sats, exactly one
self-issued PLAYER_ID, immutable identity and position packets, numeric XP,
harvested LOG, and earned XP_FUEL. There is no carrier, PLAYER_TICKET, allocator,
registry, or player cap.

```text
player state: D sats + 1 PLAYER_ID + identity + position + XP
              + optional LOG + optional XP_FUEL
D = 330 sats
```

## Permissionless Activation

A browser derives an ordinary Arkade address from its player key and receives one
exact `D`-sat VTXO. Activation is an owner-signed offchain transaction that both
issues a marker and creates the personalized player state. It attaches:

- one uncontrolled, one-unit `PLAYER_ID` at fresh asset group zero;
- metadata `game=woodland.sh`, `protocol=1`, `asset=PLAYER_ID`, and the owner;
- identity derived from `SHA256("woodland.sh/PlayerIdentity/v1" || owner || genesis_txid)`;
- spawn position `(3,17)`;
- canonical numeric XP zero.

The PLAYER_ID AssetId is `(activation_txid, 0)`. The browser persists that exact
ID before submission, then discovers only state containing that one-unit asset.
A failed submission can be rebuilt deterministically; a submitted-but-unknown
transaction can be reconciled by the same marker. Anyone may fund more players
or mint a decoy marker, but cannot reproduce an existing transaction-derived ID.

## XP Backing

`PlayerXp` is a checked `u64` encoded as eight little-endian value bytes plus a
zero sign byte. Alternate numeric encodings, including negative zero, fail.

XP must equal the amount of fixed-supply XP_FUEL held by player state:

```text
numeric XP X  <=>  player state owns X XP_FUEL
```

A forged activation with a high XP packet but no XP_FUEL cannot chop: both
player and tree covenants compare the packet to the state asset balance.
XP_FUEL has no control asset and cannot be reissued.

Level is derived, never stored. The canonical level thresholds remain
`woodland-xp-v1`; LOG chance rises from 10% to 15% at levels 10, 20, 30, 40,
and 50.
The deployed 100-fuel world caps aggregate player XP at 100, so its level-10
and higher chance bonuses are intentionally dormant. Reaching those tiers would
require a larger future world or a separate balance change.

## Atomic Chop

A canonical swing has:

```text
input 0:  player state, 1 PLAYER_ID, X XP_FUEL, M LOG
input 1:  tree, one TREE, F XP_FUEL, N LOG, health H > 0

output 0: same player state, 1 PLAYER_ID, X+G XP_FUEL, M+G LOG
output 1: same tree, F-G XP_FUEL, N-G LOG, health H-G
output 2: zero-value merged Ark extension
output 3: canonical zero-value anchor
```

`G` is the deterministic drop bit derived from the tree roll and input XP.
The four Asset V1 groups are ordered `PLAYER_ID`, `TREE`, `LOG`, `XP_FUEL`.
Group zero is exactly one metadata-free, uncontrolled unit assigned from input
zero to output zero. Zero LOG/XP_FUEL assignments are omitted while those world
groups remain present because the tree has positive inventory before a swing.

The player covenant requires:

- the current input is player-state input zero;
- exactly two inputs and four outputs;
- recursive preservation of player script and `D` sats;
- the exact shared tree script and fixed tree value;
- preserved identity and position;
- player input/output assets contain exactly one PLAYER_ID and only optional LOG
  and XP_FUEL besides it;
- input and output XP packets equal their corresponding XP_FUEL balances;
- canonical extension and anchor.

The reciprocal tree covenant enforces the actual inventory and XP deltas.

## Authorization

The chop tapleaf is fixed to:

```text
player owner + Arkade operator + script-tweaked emulator
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
the player secret. It can rotate only the exact state into a new batch, but that
outpoint change can race gameplay; unattended operation is therefore
near-expiry policy, not a requirement for active players.

Every renewal preserves byte-for-byte:

- player P2TR and 330 sats;
- identity, position, and XP packets;
- the one-unit PLAYER_ID;
- LOG and XP_FUEL balances.

## Browser Persistence

The reference frontend uses:

```text
woodland.sh:web:v1:key:<site-origin>
woodland.sh:web:v1:profile:<site-origin>
woodland.sh:web:v1:position:<site-origin>
woodland.sh:web:v1:pending:<arkade-url>
```

The profile pins the current genesis transaction and exact PLAYER_ID AssetId.
Unknown chop outcomes retain the exact prepared PSBT and reconcile indexed state
before another swing.

The signing key alone is insufficient for deterministic discovery when public
decoy states exist; production backups must preserve the profile's PLAYER_ID as
well as the key.

## Limits

The player count is unlimited, but season resources are not: the example world
contains 100 LOG and 100 XP_FUEL. Player state currently keeps harvested assets
inside the recursive contract; a separate owner-authorized withdrawal contract
would be required for external LOG transfer without weakening XP invariants.
