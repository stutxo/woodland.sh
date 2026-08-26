# Under the Canopy: How woodland.sh Works

Click a tree and the browser walks beside it, then submits swings until a LOG
falls. A successful swing also moves one XP_FUEL into player state, making the
numeric XP counter independently verifiable.

## The Frontend Is Not the Game

The map is presentation. The boundary is the schema 1 manifest, indexed Arkade
state, fixed Asset V1 supply, Bitcoin Taproot signer closures, and Arkade Script.
The browser calls the manifest-pinned Arkade and emulator services directly.

## Three World Assets, One Marker per Player

Protocol v1 creates:

```text
10 TREE
100 LOG
100 XP_FUEL
```

Each initial tree owns one TREE, ten LOG, ten XP_FUEL, health five, a public roll,
and 1,980 sats. There is no player-ticket reserve and no allocator service.
PLAYER_ID is not a world reserve: each activation issues its own one-unit,
uncontrolled marker.

## Deposit and Start

A fresh browser shows an ordinary Arkade address. The player sends exactly 330
sats from any Arkade wallet. One activation transaction issues a PLAYER_ID and
places it with those sats in the personalized recursive contract at XP zero.
Because fresh AssetIds are `(transaction ID, group index)`, this marker names one
lineage without a central allocator. The browser saves the exact ID before
submission and ignores public lookalike states carrying any other marker.
Player creation remains unlimited and permissionless.

## One Swing, Two Inputs

```text
before:
  player: 1 PLAYER_ID, XP X, X XP_FUEL, M LOG, 330 sats
  tree:   health H, F XP_FUEL, N LOG, one TREE, 1,980 sats

after:
  player: 1 PLAYER_ID, XP X+G, X+G XP_FUEL, M+G LOG, 330 sats
  tree:   health H-G, F-G XP_FUEL, N-G LOG, one TREE, 1,980 sats
```

`G` is zero or one. The transaction has exactly two inputs and four outputs:
player, tree, merged extension, and canonical anchor. Its four asset groups are
ordered PLAYER_ID, TREE, LOG, XP_FUEL. PLAYER_ID group zero must be one
metadata-free, uncontrolled unit moving from player input zero to output zero.

## XP Cannot Be Invented

A high XP packet alone is useless. Both covenants require player numeric XP to
equal player-held XP_FUEL. XP_FUEL is fixed supply with no control asset. Every
successful swing moves one unit from tree to player; misses move none. Thus:

```text
sum(tree XP_FUEL) + sum(player XP_FUEL) = 100
```

LOG obeys the same conservation equation.

## Public Roll

Each tree has a 32-byte roll. Every swing publishes its SHA-256 successor. The
first eight bytes modulo 10,000 form a bucket. Input XP selects a 1,000-1,500
basis-point threshold. This is deterministic public randomness, not hidden luck.

## Reciprocal Covenants

Player state is personalized and requires the owner signature. Tree state is
shared. Both inspect the same transaction.

The player half preserves its P2TR, 330 sats, identity, position, PLAYER_ID, and
XP backing. It pins the exact tree P2TR and fixed 1,980-sat value. The tree half
requires a live Arkade covenant at player input zero, pins all three world Asset
IDs to groups one through three, and computes the roll, reward, health, LOG, and
fuel deltas. Both require the same extension and anchor shape.

The player tapleaf closes over owner, Arkade operator, and script-tweaked
emulator. The tree tapleaf closes over operator and tweaked emulator. The
transaction succeeds only when both covenant programs and both signer closures
agree on the same bytes.

## Regrowth

The first five hits consume half the reserve and leave a renewable stump.
Maintenance waits for a deterministic 20-40 second deadline, then signs the
health reset. The covenant preserves assets and roll but does not inspect wall
clock time. The second five hits exhaust LOG and fuel; that stump is permanent.

## Renewal

Normal offchain transitions do not extend batch lifetime. Players directly join
a fresh Arkade batch through an owner-authorized exact-self-send leaf. Group zero
always carries the PLAYER_ID, including at zero XP, followed by any LOG and
XP_FUEL inventory. A separate rollover leaf allows an optional watchtower to do
the same exact self-send unattended; its CLI input includes the selected
PLAYER_ID so decoy states are ignored. A renewal cannot alter state, but its
outpoint rotation can race gameplay, so watchtowers act only near expiry. Tree
renewal and regrowth remain shared maintenance duties, not part of the player
web path.

## Honest Boundary

Protocol v1 proves fixed world supply, XP backing, selected player lineage,
packet canonicality, roll progression, and atomic reward movement. PLAYER_ID
means “this recursive state,” not “one human”: Sybil creation remains
permissionless. Movement and adjacency are frontend policy. Randomness is public
and predictable. Browser key custody and service availability are not solved.
Arkade Script is enforced by the configured emulator, so bypassing covenant
evaluation requires trusting the emulator and every other signer on that leaf.
