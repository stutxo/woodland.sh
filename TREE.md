# woodland.sh Tree Protocol v1

## Status

Protocol v1 uses manifest schema 1 and stock arkd. Genesis metadata commits
`game=woodland.sh`, `protocol=1`, and one of `TREE`, `LOG`, or `XP`.
Every manifest field is mandatory; incomplete or unknown manifests fail closed.

## Genesis

One transaction creates three fixed-supply Asset V1 groups:

```text
group 0: 2,100 TREE
group 1: 21,000,000 LOG
group 2: 21,000,000 XP
```

All groups are uncontrolled. A deterministic deployment places one TREE, 1,000
LOG, 1,000 XP, and 330 sats into each shared tree contract, and the remaining
18,900,000 LOG, 18,900,000 XP, and 330 sats into one supply vault covenant.
World bootstrap therefore needs 693,330 sats, all of it recovered in
transit as trees are retired and restocked; no player reserve exists.

## Tree State

Every tree carries:

- immutable `TreeState { tree_id, x, y }`;
- numeric health from zero through five;
- one TREE marker;
- remaining LOG and XP inventory;
- fixed value 330 sats.

Reward entropy is deliberately absent from tree state. It belongs to the
recursive player lineage, so scanning, renewing, or restocking trees cannot
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

It also requires:

- exactly two inputs and four outputs;
- group zero is one uncontrolled, metadata-free PLAYER_ID transferred from
  player input zero to player output zero;
- the world asset IDs occupy TREE group one, LOG group two, and XP group
  three;
- no transfer metadata or control-asset references;
- one TREE on the tree lineage;
- unchanged player and tree scripts and sat values;
- preserved tree identity and player identity/position;
- canonical player roll successor and luck-credit transition;
- a recursive Arkade script on player input zero;
- canonical extension and anchor.

The player covenant pins this exact tree script, so both halves authorize the
same transaction.

## Stump Refill

A new tree has health five and a 1,000-unit reserve. Successful swings
decrement health, LOG, and XP together, so a health-zero tree is a stump with
995 LOG/XP remaining. The renewal covenant resets zero health back to five
while preserving identity, assets, script, and sats; a healthy tree preserves
health exactly. No signer clock exists: refill rides whichever batch renewal
settles the stump, and the leaf is permissionless like every other tree leaf.
Player roll and credit are not part of this transaction.

## Retire and Restock

Once the last LOG leaves a tree, only its TREE marker and 330 sats remain and
refill can no longer help. Such a depleted tree is replaced atomically against
the supply vault:

```text
inputs:  depleted tree, supply vault
outputs: fresh tree, vault change, merged extension, canonical anchor
groups:  TREE, LOG, XP
```

The tree retire leaf and the vault restock leaf are reciprocal covenants over
this one transaction. Together they pin:

- the dead tree input carries exactly one TREE, zero LOG, and zero XP;
- the fresh tree output keeps the same identity (tree id and coordinate),
  script, and 330 sats, and receives one TREE, 1,000 LOG, 1,000 XP, and health
  five;
- the TREE marker enters only from the dead tree input; LOG and XP enter only
  from the vault input;
- the vault change output preserves the vault script, sats, and remaining
  supply exactly;
- no player state or reward entropy participates.

Both leaves close over the Arkade operator and tweaked emulator and nothing
else, so any caller can restock. `woodland-operator restock <manifest> tree
<tree-id>` (or `due` for every depleted tree) is a convenience wrapper.
Restocks continue until the vault reserve is exhausted; only then does a
depleted coordinate stay empty.

## Renewal

Swing transactions do not extend Arkade batch lifetime. A permissionless
exact-self-send leaf enters the tree into a fresh batch while preserving:

- P2TR and 330 sats;
- TREE, LOG, and XP amounts;
- the identity packet.

Health is the one exception: a stump refills to five (see Stump Refill). The
leaf closes over the operator and tweaked emulator only, so any caller can
renew. An unnecessary renewal can still race gameplay by rotating the
outpoint, so the operator `watch` command renews healthy trees only near the
observed expiry margin, refills stumps as soon as they appear, renews the
vault on the same margin, and reports any live tree missing an indexed expiry.

## Trust

The operator and emulator cooperate on every swing. Every other tree leaf —
renewal with stump refill and retire-and-restock — closes over the same
operator and tweaked emulator pair and nothing else, so tree liveness never
depends on a project-held key; the watcher is a convenience, not a trust
requirement. The vault covenant holds the undistributed supply: its only moves
are the atomic restock above and an exact-self-send renewal, so nobody can
redirect the reserve elsewhere. The NUMS-keyed CSV leaf satisfies Arkade
expiry accounting but provides no unilateral bypass of the recursive covenant.
