# woodland.sh Testing

## Verification Layers

woodland.sh uses three layers:

1. native tests for packet encodings, host mirrors, builders, signature checks,
   and malformed state;
2. production WASM and GitHub Pages artifact compilation for browser transport,
   storage, manifest, CSP, optional game-server origin, and static-route paths;
3. destructive regtest profiles against stock arkd and the real emulator.

## Test Placement

Unit tests live at the end of their corresponding protocol module. They retain
access to private encoding and script helpers without expanding the public API.
Cross-component behavior uses stock arkd and the real emulator rather than mock
transport traits. Destructive mutation hooks compile only with
`regtest-e2e`; the production WASM build excludes them.

## Static Checks

```bash
cargo fmt --check
cargo test --locked --features woodland-app
cargo clippy --locked --all-targets --features woodland-app -- -D warnings
cargo audit
cargo test --locked --features keygen --bin woodland-keygen
cargo clippy --locked --features keygen --bin woodland-keygen -- -D warnings
cargo test --locked --all-targets --features server
cargo clippy --locked --all-targets --features server -- -D warnings
CC_wasm32_unknown_unknown=<clang> \
  cargo check --locked --lib --target wasm32-unknown-unknown --features woodland-app
node --check web/app.js
node --check scripts/*.mjs
node scripts/test-assemble-web.mjs
```

## Stock arkd Boundary

The regtest wrapper builds unmodified arkd commit
`c7c3184f5cd416e231023f717489a5b0550960cc`, including the upstream atomic
offchain-spend fix. Protocol v1 uses ordinary Asset V1 validity. TREE, LOG, XP,
and each PLAYER_ID have no control asset; a fresh issuance creates a different
AssetId rather than reissuing an existing one. No custom arkd patch is part of
the protocol.

## Functional Profiles

```bash
./scripts/test-regtest.sh smoke
./scripts/test-regtest.sh full
```

Both clean wrapper-owned containers and volumes, start Bitcoin/indexers/stock
arkd/emulator, deploy a fresh schema 1 world, build the web bundle, and serve the
bundle plus `/v1/*` API from one native Axum origin. Browser and renewal stages
then run before teardown.

Smoke proves the complete path quickly. Full exercises player-bound luck,
bounded reward streaks, harvesting, stump renewal, LOG withdrawal, recovery,
adversarial mutations, and four-player concurrency.
Both mobile and desktop checks require the player overlay to remain exactly
centered across every sampled frame while only the Canvas camera changes. Tests
also require viewport-only tile rendering, zero per-tile DOM nodes, real canvas
coordinate input, and movement rejection before activation.
The primary browser also renders 500 synthetic nearby players as one Canvas
cluster. Multiplayer profiles verify viewport-bounded presence after players
move into different spatial regions.

The same `dist/` layout can alternatively deploy to GitHub Pages. The artifact
contains a manifest-specific CSP, `.nojekyll`, and an explicit 404. Gameplay
still calls Arkade and the emulator directly.

CI allows 120 two-second emulator readiness attempts. A timeout prints the
container state and the final 200 log lines before teardown, so startup failures
remain diagnosable.

## Bootstrap Assertions

A clean deployment verifies:

- schema 1 and protocol 1;
- one genesis txid with TREE group 0, LOG group 1, and XP group 2;
- supplies 2,100, 21,000,000, and 21,000,000;
- metadata `game=woodland.sh`, `protocol=1`, and the exact label;
- exact `treeScript`, `treeChopArkadeScript`, `treeRenewalArkadeScript`,
  `treeRetireArkadeScript`, `vaultScript`, `vaultRestockArkadeScript`, and
  `vaultRenewalArkadeScript` commitments;
- no control asset;
- 2,100 tree VTXOs with one TREE, 1,000 LOG, 1,000 XP, health five, and
  330 sats;
- one supply-vault VTXO with 18,900,000 LOG, 18,900,000 XP, and 330 sats;
- total world funding 693,330 sats;
- no player reserve or allocator signer.

## Browser Coverage

The browser stage verifies:

- direct CORS calls to Arkade and emulator, with no Woodland proxy;
- one exact 330-sat deposit issues a unique PLAYER_ID and activates player state
  with canonical roll and initial luck credit entirely client-side;
- the profile persists the exact transaction-derived AssetId;
- every live tree exposes the same next outcome for one player, while different
  players retain independent outcomes;
- every swing satisfies
  `next credit + 10,000*drop = previous credit + rate`, keeps credit within
  0–20,000, and respects the ten-miss/two-success protection bounds;
- zero-XP and nonzero-XP owner renewals preserve PLAYER_ID and extend expiry
  without a player API;
- player state holds harvested LOG and earned XP;
- `numeric XP == player-held XP` after every transition;
- LOG and XP remain conserved across trees, players, and the vault after every
  transition;
- `seasonXpRemaining` reports on-tree XP (2,100,000 at genesis);
- an owner LOG withdrawal through the `withdrawLog` API moves LOG out while
  XP, PLAYER_ID, sats, and packets remain, and an over-balance withdrawal is
  rejected client-side;
- click-to-walk/chop updates rendered map, bag, stats, and effects;
- state recovery after reload preserves the player key, PLAYER_ID, and lineage.

## Adversarial Covenant Coverage

Mutation probes require emulator rejection without indexed outpoint changes for:

- wrong player-roll successor or luck-credit successor;
- missing or extra XP/LOG asset delta;
- negative-zero XP or health;
- TREE inflation;
- swapped or foreign asset groups;
- PLAYER_ID or inventory transfer metadata, and control-asset references;
- changed position;
- extra output, wrong anchor, or funded extension;
- stale expected tree, player-state, or drop preconditions.

Restock mutations require the same rejection discipline: wrong reserve
amounts, a moved coordinate, a redirected TREE marker, and vault change that
leaks supply must all fail without indexed outpoint changes.

Native tests additionally cover exact signature sets, previous-transaction
binding, checkpoint mapping, mandatory-marker renewal, decoy-marker rejection,
malformed graph chunks, nonce/signature ordering, and forfeit combination.

## Stump Renewal and Restock

Full coverage harvests a tree to a stump twice and requires each renewal to
refill health while preserving LOG, XP, identity, and sats. Player-bound reward
entropy remains unchanged because tree renewal has no player input. The
first-party game intentionally exposes no withdrawal control; the browser
invokes the public `withdrawLog` API directly to preserve marketplace and
third-party integration coverage. LOG leaves to a destination while XP stays
soulbound in player state. Concurrent tree renewals preserve every asset and
packet.

The opt-in restock scenario, `scripts/e2e-restock-regtest.mjs`, is wired into
the aggressive matrix as the `restock` profile: a swarm chops one tree to zero
LOG, then `woodland-operator restock <manifest> tree <id>` replaces it
atomically from the supply vault. Assertions cover the fresh tree at the same
coordinate with a full 1,000-unit reserve and health five, unchanged
player-bound entropy, exact vault change, and conserved TREE/LOG/XP supplies.

Player renewal runs inside the browser at XP zero and after earned XP. It uses
the owner leaf, validates batch event order and graph shape, contributes MuSig2
signatures, and requires a later indexed expiry. Native tests cover the optional
watchtower leaf's signer closure and exact intent construction.
Batch tests simulate an arkd duplicate-input response and require the client to
delete every queued intent overlapping the renewal outpoint by signed ownership
proof before retrying once. Cleanup stops after forfeits reach the emulator;
that state must reconcile through the committed lineage instead of being
discarded.

## Vault Restock Swarm

The canonical 1,000-LOG reserve takes thousands of serial Ark batches to
deplete. The dedicated local profile therefore compiles the existing
`regtest-e2e` hooks and deploys every initial tree with five LOG and five XP,
while the manifest and covenant still require a canonical 1,000-unit restock:

```bash
./scripts/test-regtest.sh restock
```

Two browser players race the same chosen tree through the exact depletion
boundary. A shared tree outpoint permits only one committed chop per Ark batch,
so extra browser processes consume memory without increasing depletion
throughput. Set `WOODLAND_RESTOCK_PLAYERS` to raise the race fan-out explicitly,
or set `WOODLAND_E2E_INITIAL_TREE_RESERVE=1000` for the exhaustive canonical
genesis run.

The stage requires the depleted tree to carry only its TREE marker, runs
`woodland-operator restock <manifest> tree <id>`, and verifies the fresh tree
at the same coordinate (one TREE, 1,000 LOG, 1,000 XP, health five, and no
tree-local reward state), unchanged player-bound next outcome, the spent old
lineage, and a vault drawdown of exactly 1,000 LOG and 1,000 XP with vault sats
and indexed TREE/LOG/XP supplies unchanged. The JSON report lands under
`regtest/_build/restock-report.json`.

To target an already prepared remote-compatible world, run the Node scenario
directly. The funding executable receives `<address> <sats>`:

```bash
WOODLAND_E2E_PROFILE=restock \
WOODLAND_E2E_WEB_URL=https://test.example \
WOODLAND_RESTOCK_FUND_COMMAND=/path/to/test-wallet-funder \
WOODLAND_RESTOCK_PLAYERS=8 \
WOODLAND_RESTOCK_RACE_CONCURRENCY=4 \
WOODLAND_RESTOCK_ROUND_DELAY_MS=2000 \
node scripts/e2e-restock-regtest.mjs
```

## Multiplayer

Smoke starts two browser wallets; full starts four. Each receives 330 sats,
issues a distinct PLAYER_ID, activates, automatically registers, rejects forged
registration and location, exchanges authenticated presence and chat, and
appears with independently verified XP/LOG state. Regtest also forces one signed
delegated renewal and verifies revocation. Players then chop disjoint trees
concurrently; a same-tree race must produce one winning state transition. There
is no shared activation reserve or finite ticket supply.

The multiplayer stage injects one emulator submission failure after journaling,
then requires exact-PSBT recovery, rotated state/tree outpoints, and no remaining
pending chop.
Continuous-chop tests require fresh-state interactive swings, a one-second
animation cadence, and no surfaced stale-precondition error.

## Long Contention Soak

The opt-in soak profile creates 12 independent Firefox players by default and
races every player against the same tree outpoint for 30 rounds:

```bash
./scripts/test-regtest.sh soak
```

Every round requires exactly one committed player/tree transition, converged
tree state, no pending chops, conserved manifest-declared LOG/XP across all
players and trees, and exact indexed TREE/LOG/XP supplies. The runner determines
the winner from reconciled outpoints rather than a possibly ambiguous submission
response. Convergence refreshes exercise the same exact-transaction pending
recovery as the browser polling loop; this matters when an Ark batch disappears
after exposing a transient indexed successor. Independent browser refreshes can
also straddle a legitimate stump-renewal transition.
The JSON report records
client-reported acceptances, recovered unknown outcomes, convergence retries,
indexed supplies, and p50/p95 round latency under
`regtest/_build/soak-report.json`.

Load and remote-facing pressure are explicit:

```bash
WOODLAND_SOAK_PLAYERS=32 \
WOODLAND_SOAK_ROUNDS=200 \
WOODLAND_SOAK_ACTIVATION_CONCURRENCY=4 \
WOODLAND_SOAK_RACE_CONCURRENCY=4 \
WOODLAND_SOAK_TREES_PER_ROUND=4 \
WOODLAND_SOAK_RELOAD_EVERY=25 \
WOODLAND_SOAK_RELOAD_COUNT=8 \
WOODLAND_SOAK_ROUND_DELAY_MS=1000 \
./scripts/test-regtest.sh soak
```

To target an already prepared remote-compatible world, run the Node scenario
directly. The funding executable receives `<address> <sats>`:

```bash
WOODLAND_E2E_PROFILE=soak \
WOODLAND_E2E_WEB_URL=https://test.example \
WOODLAND_SOAK_FUND_COMMAND=/path/to/test-wallet-funder \
WOODLAND_SOAK_PLAYERS=8 \
WOODLAND_SOAK_RACE_CONCURRENCY=2 \
WOODLAND_SOAK_ROUND_DELAY_MS=2000 \
node scripts/e2e-soak-regtest.mjs
```

Do not point the soak profile at a shared remote service without operator
permission. Concurrency and round delay exist to bound remote load.

## Overnight v1 Release Soak

The overnight runner repeatedly creates a fresh world and rotates the full,
same-tree soak, 24-player burst, multi-tree fanout, browser-reload,
stump-renewal, and vault-restock profiles. Every cycle exercises the renamed
`renew-world` startup gate before launching the watcher. It stops on the first
failure and preserves that cycle's manifest, cycle/server/watcher logs, the last
2,000 arkd and emulator log lines, screenshots, JUnit, and structured scenario
report.

```bash
./scripts/test-overnight.sh
```

Defaults are eight hours, plan
`full,soak,burst,fanout,reload,renewal,restock`, 12 baseline soak players, and
50 contention rounds per baseline soak cycle. A successful cycle additionally
requires an exact schema-v1/protocol-v1 regtest manifest, the 2,100-tree world,
canonical rates and reserves, and its profile's structured report. Before every
cycle the runner refuses to continue below 5 GiB of free disk. The summary
records the active cycle before it starts and is rewritten atomically after
every completed cycle under
`regtest/_build/overnight/<timestamp>/overnight-summary.json`.
The overnight, aggressive, and direct `test-regtest.sh` launchers share an
advisory lock, so a second run cannot tear down the active world or reuse its
WebDriver ports.
Terminating a cycle signals the complete browser process group before the
regtest stack stops.

A larger local run:

```bash
WOODLAND_OVERNIGHT_HOURS=10 \
WOODLAND_OVERNIGHT_PLAYERS=16 \
WOODLAND_OVERNIGHT_ROUNDS=100 \
WOODLAND_OVERNIGHT_ACTIVATION_CONCURRENCY=3 \
WOODLAND_OVERNIGHT_RACE_CONCURRENCY=4 \
WOODLAND_OVERNIGHT_ROUND_DELAY_MS=1000 \
./scripts/test-overnight.sh
```

To leave it running after the terminal closes:

```bash
systemd-run --user --unit=woodland-overnight --collect \
  --property=WorkingDirectory="$PWD" \
  --setenv=WOODLAND_OVERNIGHT_HOURS=8 \
  "$PWD/scripts/test-overnight.sh"

journalctl --user -fu woodland-overnight
```

Stop it cleanly with `systemctl --user stop woodland-overnight`; the active
regtest wrapper receives a termination signal and tears down its containers.
Use `WOODLAND_OVERNIGHT_CYCLES=1` for orchestration checks. The plan accepts
`full`, `soak`, `burst`, `fanout`, `reload`, `renewal`, and `restock`.

## Four-Hour Aggressive Matrix

The aggressive launcher uses fresh worlds and rotates six materially different
profiles instead of repeating one load shape:

```bash
./scripts/test-aggressive.sh
```

| Profile | Load |
| --- | --- |
| `full` | Existing browser, adversarial covenant, renewal, recovery, and multiplayer suite |
| `burst` | 24 players, 100 zero-delay rounds, all 24 race one tree, six rotating browser reloads every 25 rounds |
| `fanout` | 24 players, four tree groups per round, 320 accepted and 1,600 conflicting submissions per cycle |
| `reload` | 12 players, two tree groups, all 12 browsers reload together every 20 rounds |
| `renewal` | Sustained same-tree pressure with forced pre-chop and post-chop exact-self-send rollovers, natural watcher rollovers, and rotating browser reloads |
| `restock` | Swarm chops one tree to zero LOG, then a permissionless operator restock replaces it from the vault mid-traffic |

Grouped rounds require exactly one player/tree transaction per target tree.
Additional player outpoint transitions are accepted only when the PLAYER_ID,
XP, LOG, luck credit, and level are unchanged; the report counts these as
independent expiry renewals. Each convergence probe refreshes the full fixed
viewport, while pending journals exercise exact transaction recovery. Browser
reloads must restore the same PLAYER_ID from storage, recover any pending state,
re-register with the server, and converge with browsers that stayed online. The
harness reapplies its fixed soak viewport after every WebDriver reload so
pending-tree reconciliation observes the same target set. Every profile still
verifies global TREE/LOG/XP supplies.

The default duration is four hours and the default plan is
`full,burst,fanout,reload,renewal,restock`. Duration and plan remain
configurable:

```bash
WOODLAND_OVERNIGHT_HOURS=3 \
WOODLAND_OVERNIGHT_PLAN=burst,fanout,reload \
./scripts/test-aggressive.sh
```

It uses the overnight runner's first-failure stop, disk floor, atomic summary,
per-cycle artifacts, and signal-safe cleanup.

## Known Gaps

Tests do not prove public service availability, denial-of-service resistance,
chat moderation, hidden randomness, geography, unique humans, pre-covenant
PLAYER_ID ancestry, hardened browser custody, or future
operator/emulator/rollover signer retention.
