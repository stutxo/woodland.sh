# Under the Canopy: How woodland.sh Works

Click a tree and the browser walks beside it, then submits swings until a LOG
falls. A successful swing also moves one unit of the soulbound XP asset into
player state, awarding 25 user-facing Woodcutting XP, and may move one crafting
material selected by an independent public bucket.

## The Frontend Is Not the Game

The map is presentation. The boundary is the signed schema 4 manifest, indexed
Arkade state, fixed Asset V1 supply, Bitcoin Taproot signer closures, and Arkade
Script. The browser verifies the deployer signature before calling the
manifest-pinned Arkade service and stock emulator directly.

## Five World Assets, One Marker per Player

Protocol v4 creates:

```text
420 TREE
21,000,000 LOG
21,000,000 XP asset units
21,000,000 STONE
21,000,000 IRON ORE
```

This version requires a fresh genesis and signed schema-4 manifest. Never reuse
v3 assets or manifests: the changed player and tree covenants cannot upgrade
an existing v3 world in place.

Each initial tree owns one TREE and 50,000 units each of LOG, XP, STONE, and
IRON ORE. The XP reserve represents 1,250,000 Woodcutting XP. It also starts
with health ten and 330 sats. No shared vault or reserve exists. There is no
player-ticket reserve and no allocator service. PLAYER_ID is not a world
reserve: each activation issues its own one-unit, uncontrolled marker.

## Deposit and Start

A fresh browser shows an ordinary Arkade address. The player sends exactly 330
sats from any Arkade wallet. One activation transaction issues a PLAYER_ID and
places it with those sats in the personalized recursive contract, with a roll
derived from that contract's P2TR witness program, 8,000 luck credit, and axe
tier `None`.
Because fresh AssetIds are `(transaction ID, group index)`, this marker names one
lineage without a central allocator. For this direct issuance-to-state shape,
every player leaf detects the first recursive spend from that AssetId and
rejects a noncanonical initial roll or credit. The browser saves the exact ID
before submission and ignores public lookalikes. Creation remains unlimited and
permissionless.

## One Swing, Two Inputs

```text
before:
  player: 1 PLAYER_ID, XP X, LOG M, STONE S, IRON ORE I, axe A, 330 sats
  tree:   health H, XP F, LOG N, STONE T, IRON ORE O, one TREE, 330 sats

after:
  player: 1 PLAYER_ID, XP X+G, LOG M+G, STONE S+K, IRON ORE I+J, axe A, 330 sats
  tree:   health H-G, XP F-G, LOG N-G, STONE T-K, IRON ORE O-J, one TREE, 330 sats
```

`G`, `K`, and `J` are zero or one; `K+J` is at most one and both material bits
are zero when `G=0`. The transaction has exactly two inputs and four outputs:
player, tree, merged extension, and canonical anchor. Its six asset groups are
ordered PLAYER_ID, TREE, LOG, XP, STONE, IRON ORE. PLAYER_ID group zero must be
one metadata-free, uncontrolled unit moving from player input zero to output
zero.

## XP Cannot Be Invented

XP has no numeric packet or second counter: player-held XP assets are the sole
progression backing. XP is fixed supply with no control asset. Every successful
swing moves one unit from tree to player and awards 25 Woodcutting XP; misses
move none. Thus:

```text
sum(tree XP assets) + sum(player XP assets) = 21,000,000
Woodcutting XP = 25 × player XP asset balance
```

LOG, STONE, and IRON ORE are also fixed issuance. Chops conserve each asset;
crafting reduces circulating balances only by its covenant-selected recipe.

XP, crafting materials, and the axe tier are soulbound: chop pins material
movement to the public bucket, renewal preserves all four inventory assets and
the axe exactly, and only crafting may burn its declared ingredients. LOG is
the liquid asset — an owner-authorized leaf withdraws arbitrary LOG to any
destination, funded by a wallet dust input.

## Public Player Luck

Reward entropy belongs to the player, not a tree. Activation sets
`R0 = SHA256("woodland.sh/player-roll/v2" || p2tr_witness_program)`. Every swing
publishes `Rnext = SHA256(Rprevious)` and interprets the 32-byte successor as a
little-endian integer modulo 10,000.

The input XP asset balance, scaled by 25 Woodcutting XP per earned unit, selects
the level component of `p`: 2,000 basis points at level 1, plus 200 at levels
10, 20, 30, 40, and 50, capped at 3,000. The equipped Wooden, Stone, or Iron
Axe adds 200, 500, or 800 basis points, so `p` is at most 3,800. Let `C` be
luck credit, initially 8,000, and `Q = C + p`. The canonical reward bit is:

```text
G = 0                    when Q < 10,000
G = 1                    when Q > 20,000
G = (bucket < p)         otherwise
Cnext = Q - 10,000 * G
```

Initial credit makes the first base-rate swing land at `Q = 10,000`, so it uses
the public 20% candidate rather than a forced miss or reward.

Therefore `Cnext + 10,000*G = C+p` on every swing and `Cnext` stays within
0–20,000. At base rate, a reward occurs at least once per eleven swings (no
more than ten consecutive misses). Success runs are bounded at two for rates
up to one third and at three for higher axe-enhanced rates. Selecting another
tree cannot change `Rnext`, `C`, or `G`. The roll is public and predictable:
the mechanism bounds variance; it is not a VRF.

Material selection is independent of the LOG candidate:

```text
B = SHA256("woodland.sh/material-roll/v1" || Rnext) mod 10,000

before level 10:  K = G when B < 1,000
from level 10:    J = G when B < 200
                  K = G when 200 <= B < 1,200
```

Thus STONE is 10% per successful LOG, IRON ORE is 2% per successful LOG after
its level-10 unlock, and a swing never moves both.

## Reciprocal Covenants

Player state is personalized and requires the owner signature. Tree state is
shared. Both inspect the same transaction.

The player half preserves its P2TR, 330 sats, PLAYER_ID, equipped axe, and every
inventory asset while advancing the player roll and luck credit canonically. It
pins the immutable TREE AssetId and fixed 330-sat value. The tree half
authenticates the complete six-leaf player Taproot template and its NUMS
internal key at input zero. Its witness supplies the 32-byte owner key plus a
single compressed-output prefix byte (`0x02` or `0x03`). This rejects arbitrary
player scripts and extra escape leaves. Pinning TREE instead of tree P2TR on
the player side avoids a circular script dependency.

The tree also pins all five world AssetIds to groups one through five and
enforces the same player-luck, reward, health, LOG, XP, STONE, and IRON ORE
deltas. Both require the same extension and anchor shape.

The player tapleaf closes over owner, Arkade operator, and script-tweaked
emulator. The tree tapleaf closes over operator and tweaked emulator. The
transaction succeeds only when both covenant programs and both signer closures
agree on the same bytes.

## Covenant-Enforced Axe Crafting

The owner can advance only one tier at a time. Crafting spends one player state
and recreates it as output zero beside the merged extension and canonical
anchor. The player covenant preserves sats, PLAYER_ID, XP, roll, luck, and
unspent inventory while requiring the next axe packet and exact recipe burn:

```text
Wooden Axe: level 1 plus 1 earned XP unit (25 XP), 1 LOG
Stone Axe:  level 5,  2 LOG + 2 STONE
Iron Axe:   level 15, 5 LOG + 2 IRON ORE
```

The highest crafted tier is always equipped. There is no skip, downgrade, or
unequip transition. The craft leaf requires owner, operator, and
script-tweaked-emulator signatures.
The first Wooden Axe therefore requires one successful chop. XP is preserved,
not spent, and equipped tiers remain constrained by held XP on every action.
Stone and Iron keep their existing level thresholds.

## Stumps and One-Batch Regrowth

Ten successful drops reduce a mature tree from health ten to zero. Any caller
may renew that funded stump into one fresh Ark batch. The renewal covenant sets
health to ten while preserving its TREE marker, coordinate, script, 330 sats,
and remaining local LOG, XP, STONE, and IRON ORE exactly.

No timer, block height, player key, or project-held lifecycle key participates.
The stump's conserved local LOG reserve is the eligibility proof. A stump with
no local LOG has no source of replacement supply and remains terminal.

## Renewal

Normal offchain transitions do not extend batch lifetime. Players directly join
a fresh Arkade batch through an owner-authorized exact-state self-send. Group
zero always carries the PLAYER_ID, including at zero XP, followed by any LOG,
XP, STONE, and IRON ORE inventory. Roll, luck, and axe packets are byte-exact.
A separate rollover leaf allows an optional watchtower to do the same exact
self-send unattended; its CLI input includes the selected PLAYER_ID so decoy
states are ignored. If arkd charges a current or scheduled intent fee, the
renewal consumes one separate asset-free wallet VTXO and returns same-contract
change; recursive state value and assets remain exact.

Tree lifecycle has separate covenant leaves. Funded-stump regrowth is
permissionless and may only reset health from zero to ten. Active trees and
terminal stumps use exact-state maintenance, which additionally requires the
low-authority rollover signer and cannot alter health or inventory. The
operator watcher selects funded stumps immediately and maintains every other
tree lineage near expiry.

## Honest Boundary

Protocol v4 proves fixed world supply, sole-source and soulbound XP and
materials, selected player lineage, canonical direct activation, recursive
player-luck and axe transitions, packet canonicality, atomic reward movement,
local-reserve preservation, and one-batch funded-stump regrowth. The signed
manifest binds ruleset, endpoints, signers, assets, and scripts to the deployer
key; genesis metadata independently binds the deployer and rollover signers.

PLAYER_ID means “this recursive state,” not “one human”: Sybil creation and
identity grinding remain permissionless. A covenant cannot inspect ancestry
from before a marker entered player state, so an owner can use an intermediate
output to choose any starting roll and credit within the corridor. This can bias
reward timing inside the corridor; it cannot provide an axe without the required
earned XP. The recursive rate budget and streak bounds still apply after entry.
Movement and adjacency are frontend policy. Randomness
is public and predictable. Browser key custody and service availability are not
solved. Arkade Script is enforced by the pinned stock emulator.
