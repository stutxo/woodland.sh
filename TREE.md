# woodland.sh Tree Protocol v2

## Status

Protocol v2 uses manifest schema 2, stock arkd, and a woodland policy gate in
front of the stock Arkade Script emulator. Genesis metadata commits
`game=woodland.sh`, `protocol=2`, and one of `TREE`, `LOG`, or `XP`. Every
manifest field is mandatory; incomplete or unknown manifests fail closed.

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
- a numeric stump height, zero while active;
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

Every swing carries a witness `[height, "WOODLAND_BLOCK_V1"]`. The covenant
requires the minimally encoded height to be positive. The introspector extension
serializes that witness into the transaction, so transaction signatures commit
it without changing the zero-locktime Ark transaction shape rebuilt by stock
arkd. A successful final swing, where output health becomes zero, records that
height in the stump packet. Every non-final swing preserves stump height zero.

## Two-Tip Regrowth

A mature tree has health ten. Ten successful drops produce a funded stump while
removing exactly ten LOG and ten XP from its local reserve. If the final chop
records height `H`, the renewal covenant permits regrowth only when:

```text
attested height >= H + 2
output health = 10
output stump height = 0
```

TREE, LOG, XP, identity, script, and 330 sats remain byte-for-byte conserved.
Any caller may submit the path. No player state or reward entropy participates.

Arkade Script cannot query Bitcoin Core. The woodland emulator gate validates
the witnessed height against its current Core tip before forwarding the request
to the stock emulator. The gate-authenticated witness is committed in the
transaction extension, and the covenant checks the two-height delta. The gate
accepts only the current Core height; a tip race requires the client to refresh
and rebuild rather than shortening the delay.

After 5,000 complete health cycles, a tree's 50,000 local LOG and XP are
exhausted. Its health-zero state remains terminal: renewal preserves the zero
health and stump height, and no protocol path can draw reserves from another
tree or mint replacements.

## Renewal

Swing transactions do not extend Arkade batch lifetime. The same permissionless
exact-self-send leaf enters a tree into a fresh batch while preserving:

- P2TR and 330 sats;
- TREE, LOG, and XP amounts;
- identity;
- active health and stump height.

Only an eligible funded stump takes the regrowth branch above. An unnecessary
renewal can race gameplay by rotating the outpoint, so the operator `watch`
command renews active trees only near the observed expiry margin and selects
funded stumps only after the two-tip threshold. The public client `regrow` API
uses the same transaction and needs no player key.

## Trust

Every usable tree leaf closes over the Arkade operator and covenant-tweaked
emulator, not a woodland-held lifecycle key. Bitcoin Taproot enforces those
signer requirements. The woodland gate is additionally trusted to report Core
height honestly; its stock-emulator upstream must remain loopback-only.
Compromising or bypassing the gate can waive the delay but does not create a
covenant path that changes local reserves or redirects TREE, LOG, XP,
or sats. The NUMS-keyed CSV leaf satisfies Arkade expiry accounting but provides
no unilateral bypass of the recursive covenant.
