# woodland.sh Tree Protocol v2

## Status

Protocol v2 uses manifest schema 2, stock arkd, and the stock Arkade Script
emulator. Genesis metadata commits `game=woodland.sh`, `protocol=2`, and one of
`TREE`, `LOG`, or `XP`. Every manifest field is mandatory; incomplete or
unknown manifests fail closed.

## Genesis

One transaction creates three fixed-supply Asset V1 groups:

```text
group 0: 420 TREE
group 1: 21,000,000 LOG
group 2: 21,000,000 XP
```

All groups are uncontrolled. Deterministic deployment places one TREE, 50,000
LOG, 50,000 XP, and 330 sats into each of exactly 420 shared tree contracts.
That allocates the complete LOG and XP supplies locally; no supply vault,
restock transaction, or player reserve exists. World bootstrap requires
138,600 sats for the tree outputs.

## Tree State

Every tree carries:

- immutable `TreeState { tree_id, x, y }`;
- numeric health from zero through ten;
- one TREE marker;
- its remaining local LOG and XP inventory;
- fixed value 330 sats.

Reward entropy is deliberately absent from tree state. It belongs to the
recursive player lineage, so scanning, renewing, or regrowing trees cannot
change a player's next outcome.

## Swing

A swing spends player state at input zero and tree state at input one:

```text
inputs:  player, tree
outputs: player, tree, merged extension, anchor
groups:  PLAYER_ID, TREE, LOG, XP
```

For input player XP `X`, the LOG threshold is 2,000 basis points plus 200 at
each canonical level boundary 10, 20, 30, 40, and 50 of the
`woodland-xp-v1` curve (1,154 / 4,470 / 13,363 / 37,224 / 101,333 XP),
capped at 3,000. The player roll and bounded luck credit produce reward bit `G`.

The tree covenant enforces atomically:

```text
player LOG:       M -> M+G
player XP asset:  X -> X+G
player XP packet: X -> X+G
tree LOG:         N -> N-G
tree XP:          F -> F-G
tree health:      H -> H-G
```

It also requires the canonical two-input/four-output shape, ordered uncontrolled
asset groups, one conserved TREE marker, recursive player and tree scripts,
fixed sat values, preserved identities, canonical player luck, and exact
extension and anchor outputs. The player covenant pins this exact tree script,
so both halves authorize the same transaction.

A successful final swing simply leaves output health zero. No stump-height
packet, block witness, or timer participates.

## One-Batch Regrowth

A mature tree has health ten. Ten successful drops produce a funded stump while
removing exactly ten LOG and ten XP from its local reserve. In one fresh Ark
batch, the renewal covenant permits:

```text
funded input health = 0
output health = 10
```

TREE, LOG, XP, identity, script, and 330 sats remain byte-for-byte conserved.
Any caller may submit the path. No player state, player key, reward entropy,
height witness, or project-held lifecycle key participates.

After 5,000 complete health cycles, a tree's 50,000 local LOG and XP are
exhausted. Its health-zero state remains terminal: renewal preserves zero
health, and no protocol path can draw reserves from another tree or mint
replacements.

## Renewal

Swing transactions do not extend Arkade batch lifetime. The same permissionless
exact-self-send leaf enters a tree into a fresh batch while preserving:

- P2TR and 330 sats;
- TREE, LOG, and XP amounts;
- identity;
- active health.

A funded stump takes the one-batch regrowth branch above. An unnecessary
renewal can race gameplay by rotating the outpoint, so the operator `watch`
command selects funded stumps immediately and renews every other tree lineage
only near the observed expiry margin. The public client `regrow` API uses the
same transaction and needs no player key.

## Trust

Every usable tree leaf closes over the Arkade operator and covenant-tweaked
emulator, not a woodland-held lifecycle key. Bitcoin Taproot enforces those
signer requirements. The pinned stock emulator is trusted to execute Arkade
Script correctly. The NUMS-keyed CSV leaf satisfies Arkade expiry accounting
but provides no unilateral bypass of the recursive covenant.
