# Under the Canopy: How woodland.sh Works

Click a tree and the browser walks beside it, then submits swings until a LOG
falls. A successful swing also moves one XP into player state, making the
numeric XP counter independently verifiable.

## The Frontend Is Not the Game

The map is presentation. The boundary is the schema 1 manifest, indexed Arkade
state, fixed Asset V1 supply, Bitcoin Taproot signer closures, and Arkade Script.
The browser calls the manifest-pinned Arkade and emulator services directly.

## Three World Assets, One Marker per Player

Protocol v1 creates:

```text
2,100 TREE
21,000,000 LOG
21,000,000 XP
```

Each initial tree owns one TREE, 1,000 LOG, 1,000 XP, health five, and 330 sats.
The remaining 18,900,000 LOG and 18,900,000 XP sit in one supply vault covenant
until restocks draw them down. There is no player-ticket reserve and no allocator
service. PLAYER_ID is not a world reserve: each activation issues its own
one-unit, uncontrolled marker.

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
sum(tree XP) + sum(player XP) + vault XP = 21,000,000
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

## Stumps and Restock

A stump — zero health with reserve left — refills to full health on its next
batch renewal. The permissionless renewal leaf carries no clock or privileged
signer, so refill is paced by batch settlement. Once the last LOG leaves, only
the TREE marker and 330 sats remain. That depleted tree is
retired and restocked in one atomic transaction with the supply vault: the
fresh tree reappears at the same coordinate with the same script, a full
1,000-unit reserve and health five, while the vault keeps the change. Player
luck is untouched by stump renewal and restock. Anyone can submit a restock;
the operator CLI is a convenience. A coordinate stays empty only once the vault
itself is exhausted.

## Renewal

Normal offchain transitions do not extend batch lifetime. Players directly join
a fresh Arkade batch through an owner-authorized exact-self-send leaf. Group zero
always carries the PLAYER_ID, including at zero XP, followed by any LOG and
XP inventory. A separate rollover leaf allows an optional watchtower to do
the same exact self-send unattended; its CLI input includes the selected
PLAYER_ID so decoy states are ignored. A renewal cannot alter state, but its
outpoint rotation can race gameplay, so watchtowers act only near expiry. Tree
and vault renewal are permissionless covenant paths outside the player web
path; the operator watcher performs them on the expiry margin (and refills
stumps immediately) as a convenience, not a trust requirement.

## Honest Boundary

Protocol v1 proves fixed world supply, XP backing, soulbound XP, selected
player lineage, canonical direct-activation and recursive player-luck
transitions, packet canonicality, atomic reward movement, and
vault-conserving restock. PLAYER_ID means “this recursive state,” not “one
human”: Sybil creation and identity grinding remain permissionless. A covenant
cannot inspect ancestry from before a marker entered player state, so an owner
can use an intermediate output to choose any starting roll and credit within the
corridor. This can bias reward timing inside the corridor; the recursive rate
budget and streak bounds still apply after entry. Movement and adjacency
are frontend policy. Randomness is public and predictable. Browser key custody
and service availability are not solved. Arkade Script is enforced by the
configured emulator, so bypassing covenant evaluation requires trusting the
emulator and every other signer on that leaf.
