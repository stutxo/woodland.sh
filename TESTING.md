# woodland.sh Testing

## Verification Layers

woodland.sh uses four layers:

1. native tests for packet encodings, host mirrors, builders, signature checks,
   and malformed state;
2. executable acceptance and rejection vectors in the pinned stock Arkade
   interpreter, including complete player-template authentication;
3. production WASM and GitHub Pages artifact compilation for browser transport,
   storage, manifest, CSP, optional game-server origin, and static-route paths;
4. destructive regtest profiles against stock arkd and the stock emulator.

Package 4.0.0 tests target protocol 4, signed schema 4, and
`woodland.sh/forest/v4`. Recreate test worlds from a fresh genesis; never reuse
v3 assets, deployment outpoints, or manifests when validating v4.

## Test Placement

Unit tests live at the end of their corresponding protocol module. They retain
access to private encoding and script helpers without expanding the public API.
Targeted local HTTP fixtures exercise lost submission responses, interrupted
finalization, saved-journal recovery, and historical settlement after an output
is spent. Protocol acceptance is verified against stock arkd and the stock
emulator; HTTP fixtures do not replace those checks. Destructive mutation hooks
compile only with `regtest-e2e`; the production WASM build excludes them.

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
for file in scripts/*.sh; do bash -n "$file"; done
node --check web/app.js
for file in scripts/*.mjs; do node --check "$file"; done
node scripts/test-assemble-web.mjs
node scripts/test-browser-recovery.mjs
```

The artifact regression accepts manifest recipe keys in any object order while
rejecting changed values, missing or extra recipe fields, and reordered tiers.

The browser-state regression runs the real application module against isolated
DOM and wallet boundaries. It covers clicked-tree selection, obsolete movement
cancellation, stationary presence heartbeats, expired or unavailable delegation,
stable social DOM during movement, visible renewal errors, and preservation of
activation journals and backup identity after stale or failed refreshes.
It also exercises two tabs sharing storage, lock takeover, mainnet reset
protection, owner renewal near expiry, and restore recovery after storage errors.
Durable pending journals block key replacement even before the UI has received
a new snapshot.
Imported activation journals are checked against the complete canonical wallet
transaction before custody changes. Rust regressions reject damaged checkpoint
bytes, owner signatures, source values, control blocks, and recovery metadata.
Server tests exercise delayed verification after renewal, registration during
consent changes, and delegation revoked while a renewal waits in the queue.

Native maintenance regressions check resumed shards by outpoint rather than
response order and keep healthy tree lineages renewable while neighboring
successors remain unindexed.

## Execute the Covenant Interpreter

With Go 1.26.5 or newer available as `go` (or set `WOODLAND_GO` to its binary),
run:

```bash
./scripts/test-covenants.sh
```

The command exports Rust-built `template.json` and `chop.json` transaction
vectors and evaluates them with the pinned, unmodified stock Arkade interpreter.
It fails if either exporter produces no fixtures or acceptance differs from a
vector's expectation. This exercises the actual script instructions, including
the full six-leaf player template and its 32-byte owner plus `0x02`/`0x03`
compressed-output prefix witness. It complements the native host checks and
the live arkd/emulator profiles; it does not replace either.

## Stock arkd Boundary

The regtest wrapper builds unmodified arkd commit
`c7c3184f5cd416e231023f717489a5b0550960cc`, including the upstream atomic
offchain-spend fix. Protocol v4 uses ordinary Asset V1 validity. TREE, LOG, XP,
STONE, IRON ORE, and each PLAYER_ID have no control asset; a fresh issuance
creates a different AssetId rather than reissuing an existing one. No custom
arkd or emulator patch or policy proxy is part of the protocol.

## Functional Profiles

Use Node.js 22 or newer and Firefox/Geckodriver with WebDriver BiDi support.
The full profile injects a submission failure before the reloaded page starts.

```bash
./scripts/test-regtest.sh smoke
./scripts/test-regtest.sh full
./scripts/test-regtest.sh progression
./scripts/test-regtest.sh regrowth
```

All four profiles clean wrapper-owned containers and volumes, start
Bitcoin/indexers/stock arkd/emulator, deploy a fresh signed schema 4 world,
build the web bundle, and serve the bundle plus `/v1/*` API from one native
Axum origin. Browser and renewal stages then run before teardown.

Smoke proves the complete path quickly, including one covenant-enforced Wooden
Axe craft. Full exercises player-bound luck, bounded reward streaks, material
drops, harvesting, axe crafting, lifecycle renewal, LOG withdrawal, recovery,
adversarial mutations, and four-player concurrency. Its later-swing recovery
check preserves the exact signed journal through a failed reload, then settles
the pending tree outside the viewport without duplicating rewards. The
`progression` profile uses one fixed-key Firefox player to find both materials,
exercise the exact level gates, and craft Wooden, Stone, and Iron Axes on the
stock emulator. It replays an old craft request after the next tier becomes
affordable and requires unchanged player state, balances, and asset supplies.
The dedicated `regrowth` profile exercises complete one-batch stump regrowth.
Both mobile and desktop checks require the player overlay to remain exactly
centered across every sampled frame while only the Canvas camera changes. Tests
also require viewport-only tile rendering, zero per-tile DOM nodes, real canvas
coordinate input, and movement rejection before activation.
The primary browser also renders 500 synthetic nearby players as one Canvas
cluster. Multiplayer profiles verify viewport-bounded presence after players
move into different spatial regions.

The same `dist/` layout can alternatively deploy to GitHub Pages. The artifact
contains a manifest-specific CSP, `.nojekyll`, and an explicit 404. Gameplay
still calls Arkade and the stock emulator directly.

CI allows 120 two-second emulator readiness attempts. A timeout prints the
container state and the final 200 log lines before teardown, so startup failures
remain diagnosable.

Pushes and pull requests gate on both smoke and progression. Scheduled runs
use full plus progression; manual dispatch runs the selected profile plus
progression, without duplicating a selected progression run. Each matrix leg
uploads its own report, and Pages waits for every functional leg to pass.

## Bootstrap Assertions

A clean deployment verifies:

- signed schema 4, protocol 4, and ruleset `woodland.sh/forest/v4`;
- canonical BIP340 manifest authentication under the declared deployer;
- one genesis txid with TREE/LOG/XP/STONE/IRON ORE groups 0/1/2/3/4;
- supplies 420 and 21,000,000 for each of the four inventory assets;
- signed rates: 25 XP per LOG, 20–30% level rate, 38% absolute axe rate,
  10% STONE, 2% IRON ORE from level 10, and exact three-tier axe recipes;
- metadata `game=woodland.sh`, `protocol=4`, exact ruleset and label, and exact
  deployer and rollover signers;
- exact `treeScript`, `treeChopArkadeScript`, `treeRegrowthArkadeScript`, and
  `treeMaintenanceArkadeScript` commitments;
- fresh v4 genesis and rejection of legacy v3 manifests;
- complete player-template authentication by the tree, with player covenants
  binding the immutable TREE AssetId rather than tree P2TR;
- no retired-tree or vault fields;
- no control asset;
- exactly 420 tree VTXOs with one TREE, 50,000 units each of LOG, XP, STONE,
  and IRON ORE, health ten, and 330 sats;
- total world funding 138,600 sats;
- no shared vault, player reserve, or allocator signer.

## Browser Coverage

The browser stage verifies:

- direct CORS calls to Arkade and the stock emulator, with no Woodland proxy;
- one exact 330-sat deposit issues a unique PLAYER_ID and activates player state
  with canonical roll, initial luck credit, and axe tier `None` entirely
  client-side;
- the profile persists the exact transaction-derived AssetId;
- every live tree exposes the same next LOG/material outcome for one player,
  while different players retain independent outcomes;
- every swing satisfies
  `next credit + 10,000*drop = previous credit + rate`, keeps credit within
  0–20,000, and respects the ten-miss bound and rate-dependent two/three-success bound;
- every material delta matches the independent roll bucket, occurs only with a
  LOG, and moves at most one of STONE or IRON ORE;
- zero-XP and nonzero-XP owner renewals preserve PLAYER_ID, all four inventory
  assets, and axe while extending expiry without a player API;
- fee-policy tests require a clean asset-free funding VTXO, maximum
  current/scheduled fee selection, exact same-contract change, and unchanged
  recursive state;
- player state holds harvested LOG and earned soulbound XP/material units;
- user-facing Woodcutting XP is exactly `25 ×` the held XP asset balance;
- levels 2 and 3 appear after four and seven successful LOGs;
- per-asset tree/player accounting and indexed issued supplies remain exact
  after every transition;
- `seasonXpRemaining` reports on-tree Woodcutting XP (525,000,000 at genesis);
- a Wooden Axe craft requires at least one earned XP unit (25 Woodcutting XP),
  burns exactly one LOG, preserves progression and the tree,
  raises LOG chance by 200 basis points, and rejects the level-locked Stone Axe;
- an owner LOG withdrawal through the `withdrawLog` API moves LOG out while XP,
  materials, axe, PLAYER_ID, sats, roll, and luck credit remain, and an
  over-balance withdrawal is rejected client-side;
- click-to-walk/chop updates rendered map, bag, tool, recipe, stats, and effects;
- state recovery after reload preserves the player key, PLAYER_ID, and lineage.

Set `WOODLAND_E2E_REQUIRE_RENEWAL_FEE=1` with a nonzero
`ARK_OFFCHAIN_INPUT_FEE` to make the browser and regrowth profiles require an
observed reduction in the clean fee VTXO. Without that flag, the same scenarios
also support intentionally fee-free local overrides.

## Deployed Axe Progression

```bash
./scripts/test-regtest.sh progression
```

The progression profile submits every swing through the real browser/WASM,
stock arkd, and stock emulator. It rejects Stone and Iron crafting one XP asset
unit before their exact level thresholds, then continues through real material
drops until each recipe is funded. Successful Wooden, Stone, and Iron crafts
must burn exactly 8 LOG, 2 STONE, and 2 IRON ORE in total while preserving XP,
PLAYER_ID, luck, sats, and every unrelated inventory unit.

Final assertions require the Iron Axe rate, maximum-tier UI, exact tree/player
accounting, indexed circulating supplies of 20,999,992 LOG, 21,000,000 XP,
20,999,998 STONE, and 20,999,998 IRON ORE, owner renewal, and browser-reload
recovery. The structured report is
`regtest/_build/progression-report.json`. Material acquisition is unbounded in
the protocol, so the local harness uses configurable safety limits
`WOODLAND_PROGRESSION_MAX_SWINGS` and
`WOODLAND_PROGRESSION_MAX_SUCCESSES`.

## Adversarial Covenant Coverage

Executable covenant vectors require interpreter rejection of arbitrary player
covenants, replaced or extra leaves, noncanonical internal keys, and equipped
axes without sufficient earned XP, including a zero-XP Wooden Axe.

Live mutation probes require emulator rejection without indexed outpoint changes for:

- wrong player-roll successor or luck-credit successor;
- missing or extra XP/LOG delta or wrong STONE/IRON ORE delta;
- malformed health or luck-credit encoding;
- TREE inflation;
- swapped or foreign asset groups;
- PLAYER_ID or inventory transfer metadata, and control-asset references;
- extra output, wrong anchor, or funded extension;
- stale expected tree, player-state, or LOG preconditions.

Tree lifecycle tests require distinct leaves and signer sets: permissionless
funded-stump regrowth resets health to ten; rollover-authorized maintenance
preserves active or terminal health exactly. Both conserve local reserves and
use the zero-locktime transaction shape rebuilt by stock arkd.

Native tests additionally cover exact signature sets, axe packet mutation,
craft tier/recipe enforcement, previous-transaction binding, checkpoint
mapping, mandatory-marker renewal, decoy-marker rejection, malformed graph
chunks, nonce/signature ordering, and forfeit combination.
The Wooden minimum is one XP asset unit; Stone and Iron retain their existing
level thresholds (16 and 97 units). Pre-entry luck and credit selection remains
a documented permissionless-identity boundary, not evidence of a free axe path.

## One-Batch Regrowth

The dedicated profile uses the canonical world without reserve overrides:

```bash
./scripts/test-regtest.sh regrowth
```

One Firefox player harvests exactly ten successful drops from a pristine tree,
reaching 250 Woodcutting XP and level 3 while producing health zero, 49,990
local LOG, 49,990 local XP asset units, and deterministic material deltas. A
second Firefox browser remains inactive and carries only a clean ordinary-wallet
VTXO for arkd intent fees. It clicks the stump and completes one permissionless
renewal batch, proving that active player state and player-key covenant
authorization are unnecessary. The harness mines no delay blocks.

Immutable state, TREE, 330 sats, coordinate, script, player-bound entropy, and
all post-harvest local reserves remain unchanged by regrowth. Indexer assertions
require the old stump spent, the new health-ten tree live, and fixed issued
TREE/LOG/XP/STONE/IRON ORE supplies of
420/21,000,000/21,000,000/21,000,000/21,000,000. The report records the
observed batch latency at `regtest/_build/regrowth-report.json`.

For a prepared remote-compatible world, the funding executable is called for
both the activation VTXO and the inactive regrower's fee VTXO, as
`<address> <sats>`:

```bash
WOODLAND_E2E_PROFILE=regrowth \
WOODLAND_E2E_WEB_URL=https://test.example \
WOODLAND_REGROWTH_FUND_COMMAND=/path/to/test-wallet-funder \
node scripts/e2e-regrowth-regtest.mjs
```

The regtest-e2e build checks each swing against its exposed next-drop forecast.
A production remote build exposes no forecast: the same profile uses the
ordinary `chop` method and accepts naturally observed misses or drops. Its
per-operation timeout defaults to two minutes, not ten.

Player renewal still runs inside the main browser profile at XP zero and after
earned XP. It uses the owner leaf, validates batch event order and graph shape,
contributes MuSig2 signatures, and requires a later indexed expiry. Native
tests cover the optional watchtower leaf's signer closure and exact intent
construction. Batch tests simulate an arkd duplicate-input response and require
the client to delete every queued intent overlapping the renewal outpoint by
signed ownership proof before retrying once. Cleanup stops after forfeits reach
the emulator; that state must reconcile through the committed lineage instead
of being discarded.

## Multiplayer

Smoke starts two browser wallets; full starts four. Each receives 330 sats,
issues a distinct PLAYER_ID, activates, automatically registers, rejects forged
registration and location, exchanges authenticated presence and chat, and
appears with independently verified XP/LOG/STONE/IRON ORE and axe state.
Regtest also forces one signed delegated renewal and verifies revocation.
Players then chop disjoint trees concurrently; a same-tree race must produce
one winning state transition. There is no shared activation reserve or finite
ticket supply.

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
tree state, no pending chops, exact manifest-declared per-asset accounting
across all players and trees, and exact indexed TREE/LOG/XP/STONE/IRON ORE
issued supplies. The runner determines the winner from reconciled outpoints
rather than a possibly ambiguous submission response. Convergence refreshes
exercise the same exact-transaction pending recovery as the browser polling
loop; this matters when an Ark batch disappears after exposing a transient
indexed successor.
Independent browser refreshes can
also straddle legitimate active-tree maintenance.
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

### Emulator Outage Chaos

The chaos profile runs the same one-tree contention contract through a
loopback-only fault proxy in front of the stock emulator:

```bash
./scripts/test-regtest.sh chaos
```

One round holds every `POST /v1/tx` at HTTP 500 through both the initial submit
and the browser's immediate exact-transaction resume. The harness requires zero
state transitions during the outage and one retained pending journal per
player. It then restores the emulator, resumes those exact transactions under
contention, and requires one winner, cleared journals, converged browsers, and
conserved supplies. A later round forwards one successful submission but masks
its response as HTTP 500, proving reconciliation of a committed transaction
whose response was lost. Proxy counters in `chaosEvents` prove that both fault
classes occurred; the proxy log is
`regtest/_build/ci-artifacts/emulator-chaos-proxy.log`.

Defaults are 12 players, 12 rounds, persistent outage at round 3, and masked
success at round 7. Override
`WOODLAND_SOAK_CHAOS_FAIL_BEFORE_ROUND` and
`WOODLAND_SOAK_CHAOS_FAIL_AFTER_SUCCESS_ROUND` for shorter checks. Chaos rounds
must be distinct and use one target tree.

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

## Overnight v4 Release Soak

The overnight runner repeatedly creates a fresh world and rotates the full,
same-tree soak, 24-player burst, persistent emulator-outage chaos, multi-tree
fanout, browser-reload, renewal, and one-batch regrowth profiles. Every cycle
exercises the `renew-world` startup gate before launching the watcher. It stops
on the first failure and preserves that cycle's manifest, cycle/server/watcher
logs, the last 2,000 arkd and emulator log lines, screenshots, JUnit, and
structured scenario report.

```bash
./scripts/test-overnight.sh
```

Defaults are eight hours, plan
`full,soak,burst,chaos,fanout,reload,renewal,regrowth`, 12 baseline soak players,
and 50 contention rounds per baseline soak cycle. A successful cycle
additionally requires an exact signed schema-4/protocol-4 regtest manifest,
ruleset `woodland.sh/forest/v4`, the 420-tree world, canonical rates and
reserves, and its profile's structured report. Before every
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
`full`, `soak`, `burst`, `chaos`, `fanout`, `reload`, `renewal`, and `regrowth`.

## Four-Hour Aggressive Matrix

The aggressive launcher uses fresh worlds and rotates seven materially
different profiles instead of repeating one load shape:

```bash
./scripts/test-aggressive.sh
```

| Profile | Load |
| --- | --- |
| `full` | Existing browser, adversarial covenant, renewal, recovery, and multiplayer suite |
| `burst` | 24 players, 100 zero-delay rounds, all 24 race one tree, six rotating browser reloads every 25 rounds |
| `chaos` | 24 players contend on one tree while a persistent emulator outage rejects both submission attempts, then one committed response is masked |
| `fanout` | 24 players, four tree groups per round, 320 accepted and 1,600 conflicting submissions per cycle |
| `reload` | 12 players, two tree groups, all 12 browsers reload together every 20 rounds |
| `renewal` | Sustained same-tree pressure with forced pre-chop and post-chop exact-self-send rollovers, natural watcher rollovers, and rotating browser reloads |
| `regrowth` | Inactive fee payer permissionlessly regrows a funded stump in one fresh batch, without delay mining |

Grouped rounds require exactly one player/tree transaction per target tree.
Additional player outpoint transitions are accepted only when the PLAYER_ID,
XP, LOG, STONE, IRON ORE, axe, luck credit, and level are unchanged; the report
counts these as independent expiry renewals. Each convergence probe refreshes
the full fixed viewport, while pending journals exercise exact transaction
recovery. Browser reloads must restore the same PLAYER_ID from storage, recover
any pending state, re-register with the server, and converge with browsers that
stayed online. The harness reapplies its fixed soak viewport after every
WebDriver reload so pending-tree reconciliation observes the same target set.
Every profile still verifies global TREE/LOG/XP/STONE/IRON ORE issued supplies.

The default duration is four hours and the default plan is
`full,burst,chaos,fanout,reload,renewal,regrowth`. Duration and plan remain
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
