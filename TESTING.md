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
`8b34e352859595cc03ba22ffa35088ab88b87fd9`. Protocol v1 uses ordinary Asset V1
validity. TREE, LOG, XP, and each PLAYER_ID have no control asset; a fresh
issuance creates a different AssetId rather than reissuing an existing one. No
custom arkd patch is part of the protocol.

## Functional Profiles

```bash
./scripts/test-regtest.sh smoke
./scripts/test-regtest.sh full
```

Both clean wrapper-owned containers and volumes, start Bitcoin/indexers/stock
arkd/emulator, deploy a fresh schema 1 world, build the web bundle, and serve the
bundle plus `/v1/*` API from one native Axum origin. Browser and renewal stages
then run before teardown.

Smoke proves the complete path quickly. Full exercises deterministic depletion,
regrowth, recovery, adversarial mutations, and four-player concurrency.
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
- supplies 10, 100, and 100;
- metadata `game=woodland.sh`, `protocol=1`, and the exact label;
- exact `treeScript`, `treeChopArkadeScript`, `treeRegrowArkadeScript`, and
  `treeRenewalArkadeScript` commitments;
- no control asset;
- ten tree VTXOs with one TREE, ten LOG, ten XP, health five, and
  1,980 sats;
- total world funding 19,800 sats;
- no player reserve or allocator signer.

## Browser Coverage

The browser stage verifies:

- direct CORS calls to Arkade and emulator, with no Woodland proxy;
- one exact 330-sat deposit issues a unique PLAYER_ID and activates player state
  entirely client-side in one transaction;
- the profile persists the exact transaction-derived AssetId;
- zero-XP and nonzero-XP owner renewals preserve PLAYER_ID and extend expiry
  without a player API;
- player state holds harvested LOG and earned XP;
- `player XP == player XP` after every transition;
- `sum(tree LOG) + player LOG = 100`;
- `sum(tree XP) + player XP = 100`;
- click-to-walk/chop updates rendered map, bag, stats, and effects;
- state recovery after reload preserves the player key, PLAYER_ID, and lineage.

## Adversarial Covenant Coverage

Mutation probes require emulator rejection without indexed outpoint changes for:

- wrong roll successor;
- missing or extra XP/LOG asset delta;
- negative-zero XP or health;
- TREE inflation;
- swapped or foreign asset groups;
- PLAYER_ID or inventory transfer metadata, and control-asset references;
- changed position;
- extra output, wrong anchor, or funded extension;
- stale expected tree, player-state, or drop preconditions.

Native tests additionally cover exact signature sets, previous-transaction
binding, checkpoint mapping, mandatory-marker renewal, decoy-marker rejection,
malformed graph chunks, nonce/signature ordering, and forfeit combination.

## Regrowth and Renewal

Full coverage harvests a tree through both health windows. The first stump
regrows only after its deterministic deadline and without inventory issuance;
the second is permanently exhausted. Concurrent tree renewals preserve every
asset and packet.

Player renewal runs inside the browser at XP zero and after earned XP. It uses
the owner leaf, validates batch event order and graph shape, contributes MuSig2
signatures, and requires a later indexed expiry. Native tests cover the optional
watchtower leaf's signer closure and exact intent construction.

## Multiplayer

Smoke starts two browser wallets; full starts four. Each receives 330 sats,
issues a distinct PLAYER_ID, activates independently, rejects forged server
consent and location, signs its own opt-in, exchanges authenticated presence and
chat, and appears with independently verified XP/LOG state. Regtest also forces
one signed delegated renewal and verifies revocation. Players then chop disjoint
trees concurrently; a same-tree race must produce one winning state transition.
There is no shared activation reserve or finite ticket supply.

## Known Gaps

Tests do not prove public service availability, denial-of-service resistance,
chat moderation, hidden randomness, geography, unique humans, hardened browser
custody, or future operator/emulator/rollover signer retention.
