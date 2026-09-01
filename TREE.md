# woodland.sh Tree Protocol v3

## Status

Protocol v3 uses signed manifest schema 3, stock arkd, and the stock Arkade
Script emulator. Genesis metadata commits `game=woodland.sh`, `protocol=3`,
`ruleset=woodland.sh/forest/v3`, one of `TREE`, `LOG`, `XP`, `STONE`, or
`IRON ORE`, and the exact deployer and rollover signers. The deployer signs
every manifest field with BIP340; invalid, incomplete, or unknown manifests
fail closed.

## Genesis

One transaction creates five fixed-supply Asset V1 groups:

```text
group 0: 420 TREE
group 1: 21,000,000 LOG
group 2: 21,000,000 XP asset units
group 3: 21,000,000 STONE
group 4: 21,000,000 IRON ORE
```

All groups are uncontrolled. Deterministic deployment places one TREE, 50,000
units each of LOG, XP, STONE, and IRON ORE, and 330 sats into each of exactly
420 shared tree contracts. Each XP unit represents 25 Woodcutting XP, so one
tree backs 1,250,000 Woodcutting XP. The complete supplies are local; no supply
vault, restock transaction, or player reserve exists. World bootstrap requires
138,600 sats for the tree outputs.

## Tree State

Every tree carries:

- immutable `TreeState { tree_id, x, y }`;
- numeric health from zero through ten;
- one TREE marker;
- its remaining local LOG, XP, STONE, and IRON ORE inventory;
- fixed value 330 sats.

Reward entropy is deliberately absent from tree state. It belongs to the
recursive player lineage, so scanning, renewing, or regrowing trees cannot
change a player's next outcome.

## Swing

A swing spends player state at input zero and tree state at input one:

```text
inputs:  player, tree
outputs: player, tree, merged extension, anchor
groups:  PLAYER_ID, TREE, LOG, XP, STONE, IRON ORE
```

For input player XP asset balance `X`, the level component of the LOG threshold
is 2,000 basis points plus 200 at canonical Woodcutting levels 10, 20, 30, 40,
and 50 of the `woodland-xp-v1` curve
(1,154 / 4,470 / 13,363 / 37,224 / 101,333 Woodcutting XP). Because one asset
unit represents 25 XP, the covenant compares `X` against
47 / 179 / 535 / 1,489 / 4,054, capped at 3,000 basis points. The player's
Wooden, Stone, or Iron Axe adds 200, 500, or 800 basis points, capped at 3,800
total. The player roll and bounded luck credit produce reward bit `G`.

The tree covenant enforces atomically:

```text
player LOG:          M -> M+G
player XP asset:     X -> X+G
player STONE:        S -> S+K
player IRON ORE:     I -> I+J
Woodcutting XP:    25X -> 25(X+G)
tree LOG:            N -> N-G
tree XP asset:       F -> F-G
tree STONE:          T -> T-K
tree IRON ORE:       O -> O-J
tree health:         H -> H-G
```

`K` and `J` are disjoint material bits derived from
`SHA256("woodland.sh/material-roll/v1" || next_player_roll) mod 10,000`, and
both are gated by `G`. STONE is selected at 10%; from level 10, IRON ORE is
selected at 2% before the STONE range. A miss moves neither.

It also requires the canonical two-input/four-output shape, ordered uncontrolled
asset groups, one conserved TREE marker, recursive player and tree scripts,
fixed sat values, preserved tree state, canonical player luck, and exact
extension and anchor outputs. The player covenant pins this exact tree script,
so both halves authorize the same transaction.

A successful final swing simply leaves output health zero. No stump-height
packet, block witness, or timer participates.

## One-Batch Regrowth

A mature tree has health ten. Ten successful drops produce a funded stump while
removing exactly ten LOG and ten XP asset units — 250 Woodcutting XP — from its
local reserve. In one fresh Ark batch, the renewal covenant permits:

```text
funded input health = 0
output health = 10
```

TREE, LOG, XP, STONE, IRON ORE, immutable tree state, script, and 330 sats
remain byte-for-byte conserved. Any caller may submit the path. No player state,
player key, reward entropy, height witness, or project-held lifecycle key
participates.

After 5,000 complete health cycles, a tree's 50,000 local LOG and XP are
exhausted. Its health-zero state remains terminal even if STONE or IRON ORE
remains: the regrowth leaf requires a positive LOG reserve, maintenance
preserves zero health, and no protocol path can draw reserves from another tree
or mint replacements.

## Renewal

Swing transactions do not extend Arkade batch lifetime. Tree lifecycle uses two
separate exact-state leaves:

- **regrowth** requires input health zero and positive LOG reserve, changes only
  output health to ten, and closes over operator plus tweaked emulator;
- **maintenance** preserves health exactly, handles active trees and terminal
  stumps, and additionally requires the dedicated rollover signer.

Both preserve P2TR, 330 sats, TREE, LOG, XP, STONE, IRON ORE, and immutable
tree state. If arkd's current or scheduled intent fee is nonzero, the intent
adds one clean, asset-free wallet VTXO and returns exact same-contract change;
tree state never funds fees.

An unnecessary maintenance renewal can race gameplay by rotating the outpoint,
so the operator `watch` command selects funded stumps immediately and maintains
every other tree lineage only near the observed expiry margin. The public
client `regrow` API submits only eligible funded-stump regrowth.

## Trust

Every usable tree leaf closes over the Arkade operator and covenant-tweaked
emulator. Maintenance additionally closes over the dedicated rollover signer;
regrowth does not. Bitcoin Taproot enforces those signer requirements. The
pinned stock emulator is trusted to execute Arkade Script correctly. The
NUMS-keyed CSV leaf satisfies Arkade expiry accounting but provides no
unilateral bypass of the recursive covenant.
