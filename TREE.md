# woodland.sh Tree Protocol v1

## Status

Protocol v1 uses manifest schema 1 and stock arkd. Genesis metadata commits
`game=woodland.sh`, `protocol=1`, and one of `TREE`, `LOG`, or `XP_FUEL`.
Every manifest field is mandatory; incomplete or unknown manifests fail closed.

## Genesis

One transaction creates three fixed-supply Asset V1 groups:

```text
group 0:  10 TREE
group 1: 100 LOG
group 2: 100 XP_FUEL
```

All groups are uncontrolled. Ten deployment transactions place one TREE, ten
LOG, ten XP_FUEL, and 1,980 sats into each shared tree contract. World bootstrap
therefore needs 19,800 sats; no player reserve exists.

## Tree State

Every tree carries:

- immutable `TreeState { tree_id, x, y }`;
- a 32-byte public roll value;
- numeric health from zero through five;
- one TREE marker;
- remaining LOG and XP_FUEL inventory;
- fixed value `6 * 330 = 1,980 sats`.

The roll begins at a domain-separated hash of immutable tree identity. Every
swing sets `Rnext = SHA256(Rprevious)`. The bucket is the first eight digest
bytes modulo 10,000.

## Swing

A swing spends player state at input zero and tree state at input one:

```text
inputs:  player, tree
outputs: player, tree, merged extension, anchor
groups:  PLAYER_ID, TREE, LOG, XP_FUEL
```

For input player XP `X`, the LOG threshold is 1,000 basis points plus 100 at
each canonical level boundary 10, 20, 30, 40, and 50, capped at 1,500. Let `G`
be the resulting public drop bit.

The tree covenant enforces atomically:

```text
player LOG:     M -> M+G
player XP_FUEL: X -> X+G
player XP:      X -> X+G
tree LOG:       N -> N-G
tree XP_FUEL:   F -> F-G
tree health:    H -> H-G
```

It also requires:

- exactly two inputs and four outputs;
- group zero is one uncontrolled, metadata-free PLAYER_ID transferred from
  player input zero to player output zero;
- the world asset IDs occupy TREE group one, LOG group two, and XP_FUEL group
  three;
- no transfer metadata or control-asset references;
- one TREE on the tree lineage;
- unchanged player and tree scripts and sat values;
- preserved tree identity and player identity/position;
- a recursive Arkade script on player input zero;
- canonical extension and anchor.

The player covenant pins this exact tree script, so both halves authorize the
same transaction.

## Regrowth
A new tree has health five and reserve ten. Five successful swings leave a
renewable stump with health zero and five LOG/fuel remaining. The covenant
allows only a zero-to-five health reset that preserves roll, assets, script, and
sats. The maintenance host waits for the deterministic 20-40 second deadline
before signing; wall-clock delay is signer policy, not an Arkade Script
condition.

After the second health window, LOG and XP_FUEL are zero. The tree remains a
permanent stump; regrowth is impossible because the covenant requires at least
five of both assets.

## Renewal

Swing and regrowth transactions do not extend Arkade batch lifetime. A separate
rollover-authorized exact-self-send leaf enters the tree into a fresh batch while
preserving:

- P2TR and 1,980 sats;
- TREE, LOG, and XP_FUEL amounts;
- identity, roll, and health packets.

A low-authority rollover signer plus operator and tweaked emulator authorize the
leaf. It cannot change tree state, assets, script, or value, but an unnecessary
renewal can race gameplay by rotating the outpoint. The maintenance watcher
therefore uses it only near the observed expiry margin.

## Trust

The operator and emulator cooperate on every swing. Maintenance signs only
regrowth; rollover signs only exact tree/player self-sends. The NUMS-keyed CSV
leaf satisfies Arkade expiry accounting but provides no unilateral bypass of the
recursive covenant.
