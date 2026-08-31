# Under the Canopy: How woodland.sh Works

Click a tree and the browser walks beside it, then submits swings until a LOG
falls. A successful swing also moves one XP into player state, making the
numeric XP counter independently verifiable.

## The Frontend Is Not the Game

The map is presentation. The boundary is the schema 2 manifest, indexed Arkade
state, fixed Asset V1 supply, Bitcoin Taproot signer closures, Arkade Script,
and the Bitcoin-height policy enforced by the emulator gate. The browser calls
the manifest-pinned Arkade service and emulator gate directly.

## Three World Assets, One Marker per Player

Protocol v2 creates:

```text
420 TREE
21,000,000 LOG
21,000,000 XP
```

Each initial tree owns one TREE, 50,000 LOG, 50,000 XP, health ten, stump
height zero, and 330 sats. No shared vault or shared reserve exists. There is
no player-ticket reserve and no allocator service. PLAYER_ID is not a world
reserve: each activation issues its own one-unit, uncontrolled marker.

## Deposit and Start

A fresh browser shows an ordinary Arkade address. The player sends exactly 330
sats from any Arkade wallet. One activation transaction issues a PLAYER_ID and
places it with those sats in the personalized recursive contract at XP zero,
with the identity-derived roll and 8,000 luck credit. Because fresh AssetIds are
`(transaction ID, group index)`, this marker names one lineage without a central
allocator. For this direct issuance-to-state shape, every player leaf detects
the first recursive spend from that AssetId and rejects a noncanonical initial
roll or credit. The browser saves the exact ID before submission and ignores
public lookalikes. Creation remains unlimited and permissionless.

## One Swing, Two Inputs

```text
before:
  player: 1 PLAYER_ID, XP X, X XP, M LOG, 330 sats
  tree:   health H, F XP, N LOG, one TREE, 330 sats

after:
  player: 1 PLAYER_ID, XP X+G, X+G XP, M+G LOG, 330 sats
  tree:   health H-G, F-G XP, N-G LOG, one TREE, 330 sats
```

`G` is zero or one. The transaction has exactly two inputs and four outputs:
player, tree, merged extension, and canonical anchor. Its four asset groups are
ordered PLAYER_ID, TREE, LOG, XP. PLAYER_ID group zero must be one
metadata-free, uncontrolled unit moving from player input zero to output zero.

## XP Cannot Be Invented

A high XP packet alone is useless. Both covenants require player numeric XP to
equal player-held XP. XP is fixed supply with no control asset. Every
successful swing moves one unit from tree to player; misses move none. Thus:

```text
sum(tree XP) + sum(player XP) = 21,000,000
```

LOG obeys the same conservation equation.

XP is also soulbound: chop pins its movement to the drop bit, renewal
preserves it exactly, and no leaf moves it out of player state, so a level
can never change hands. LOG is the liquid asset — an owner-authorized leaf
withdraws arbitrary LOG to any destination, funded by a wallet dust input.

## Public Player Luck

Reward entropy belongs to the player, not a tree. Activation sets
`R0 = SHA256("woodland.sh/player-roll/v1" || player_identity_packet)`. Every
swing publishes `Rnext = SHA256(Rprevious)` and interprets the 32-byte successor
as a little-endian integer modulo 10,000.

Input XP selects `p`: 2,000 basis points at level 1, plus 200 at levels 10, 20,
30, 40, and 50, capped at 3,000. Let `C` be luck credit, initially 8,000, and
`Q = C + p`. The canonical reward bit is:

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
more than ten consecutive misses), and no rate permits more than two consecutive
successes. Selecting another tree cannot change `Rnext`, `C`, or `G`. The roll
is public and predictable: the mechanism bounds variance; it is not a VRF.

## Reciprocal Covenants

Player state is personalized and requires the owner signature. Tree state is
shared. Both inspect the same transaction.

The player half preserves its P2TR, 330 sats, identity, position, PLAYER_ID, and
XP backing while advancing the player roll and luck credit canonically. It pins
the exact tree P2TR and fixed 330-sat value. The tree half requires a live
Arkade covenant at player input zero, pins all three world Asset IDs to groups
one through three, and enforces the same player-luck, reward, health, LOG, and XP
deltas. Both require the same extension and anchor shape.

The player tapleaf closes over owner, Arkade operator, and script-tweaked
emulator. The tree tapleaf closes over operator and tweaked emulator. The
transaction succeeds only when both covenant programs and both signer closures
agree on the same bytes.

## Stumps and Two-Tip Regrowth

Ten successful drops reduce a mature tree from health ten to zero. The final
successful chop records the emulator-attested Bitcoin height `H`. Any caller
may regrow that funded stump to health ten once the attested height is at least
`H + 2`. Regrowth preserves its TREE marker, coordinate, script, 330 sats,
and remaining local LOG and XP exactly, then clears the stump height.

The canonical witness is serialized in the transaction's introspector extension,
so transaction signatures commit it. Before the stock emulator signs, the
woodland emulator gate checks that height against Bitcoin Core. This trusted
gate emulates the two-block relative delay that Arkade Script cannot observe
directly. A stump with no local LOG has no source
of replacement supply and remains terminal.

## Renewal

Normal offchain transitions do not extend batch lifetime. Players directly join
a fresh Arkade batch through an owner-authorized exact-self-send leaf. Group zero
always carries the PLAYER_ID, including at zero XP, followed by any LOG and
XP inventory. A separate rollover leaf allows an optional watchtower to do
the same exact self-send unattended; its CLI input includes the selected
PLAYER_ID so decoy states are ignored. A renewal cannot alter state, but its
outpoint rotation can race gameplay, so watchtowers act only near expiry. Tree
renewal is a permissionless covenant path outside the player web path. The
operator watcher renews active trees near expiry and regrows eligible funded
stumps after the two-tip gate; it is a convenience, not a trust requirement.

## Honest Boundary

Protocol v2 proves fixed world supply, XP backing, soulbound XP, selected
player lineage, canonical direct-activation and recursive player-luck
transitions, packet canonicality, atomic reward movement, local-reserve
preservation, and the covenant side of two-tip regrowth. PLAYER_ID means “this
recursive state,” not “one human”: Sybil creation and identity grinding remain
permissionless. A covenant cannot inspect ancestry from before a marker entered
player state, so an owner can use an intermediate output to choose any starting
roll and credit within the corridor. This can bias reward timing inside the
corridor; the recursive rate budget and streak bounds still apply after entry.
Movement and adjacency are frontend policy. Randomness is public and predictable.
Browser key custody and service availability are not solved. Arkade Script is
enforced by the stock emulator; the woodland gate is additionally trusted to
report Bitcoin Core height honestly.
