# woodland.sh Regtest

This directory provides the minimal local stack used by woodland.sh protocol
v4: Bitcoin Core, indexers, stock arkd/arkd-wallet, Redis, and the stock Arkade
Script emulator.

## Run

```bash
./scripts/regtest.sh clean --force
./scripts/run-web.sh
```

Open `http://127.0.0.1:8000/`. `woodland-server` serves both `dist/` and every
`/v1/*` API on that origin. Gameplay calls local arkd on 7070 and the stock
emulator on 7073. A separate local renewal watcher runs alongside Axum.

Protocol v4 uses signed manifest schema 4. A fresh world creates five fixed-supply
groups:

```text
group 0:       420 TREE
group 1: 21,000,000 LOG
group 2: 21,000,000 XP asset units
group 3: 21,000,000 STONE
group 4: 21,000,000 IRON ORE
```

Version 4 requires a fresh genesis. Never reuse v3 assets, deployment outpoints,
or manifests. Clean the local test stack before creating the v4 world.

Each tree receives one TREE, 50,000 units each of LOG, XP, STONE, and IRON ORE,
health ten, and 330 sats. Every earned XP unit represents 25 Woodcutting XP, so
each tree backs 1,250,000 Woodcutting XP. All four complete inventory supplies
are distributed across exactly 420 tree-local reserves.
Bootstrap funding is 138,600 sats. There is no vault, control asset,
PLAYER_TICKET, allocator reserve, invitation, or player registry.

A browser wallet receives one exact 330-sat VTXO and, in one transaction,
issues a unique uncontrolled PLAYER_ID into recursive player state with its
canonical roll and initial 8,000 luck credit. Harvested LOG and the soulbound
XP asset remain in that state; user-facing Woodcutting XP is exactly 25 times
the held XP asset balance.

## Commands

With Go 1.26.5 or newer, run the covenant vectors through the pinned stock
interpreter before the live profiles:

```bash
./scripts/test-covenants.sh
```

The v4 vectors cover full six-leaf player-template authentication by the tree
and XP-backed axe tiers. Wooden requires one earned XP asset unit (25
Woodcutting XP, one successful chop) before its one-LOG craft; Stone and Iron
keep their existing level gates. Permissionless pre-entry roll/credit selection
remains possible within the luck corridor, without granting a free axe.

```text
./scripts/regtest.sh start
./scripts/regtest.sh start-tree
./scripts/regtest.sh renew-world
./scripts/regtest.sh fees
./scripts/regtest.sh stop
./scripts/regtest.sh clean --force
./scripts/regtest.sh build-images [arkd-source]
./scripts/regtest.sh fund <ark-address> <sats>
./scripts/regtest.sh balance
./scripts/regtest.sh vtxos
./scripts/regtest.sh info
./scripts/regtest.sh mine [blocks]
./scripts/regtest.sh rpc <bitcoin-cli args...>
./scripts/regtest.sh ark <ark-cli args...>
./scripts/regtest.sh arkd <arkd-cli args...>
```

`fees` reapplies the configured live arkd intent policy. `fund` is a serialized
test faucet: it pauses intent fees only long enough to create the requested
exact VTXO, then restores that policy before returning.

## Stock arkd

The wrapper checks out unmodified commit:

```text
c7c3184f5cd416e231023f717489a5b0550960cc
```

The source defaults to `.cache/arkd-stock`. Startup verifies expected image tags
and stock-emulator version before deployment. Protocol v4 requires no custom
arkd, emulator patch, or policy proxy.

## Renewal

The manifest pins one `rolloverSigner` for optional player watchtower
authorization and exact-state tree maintenance. Active players renew directly
with their owner key. Funded-stump regrowth remains permissionless; active trees
and terminal stumps require rollover-authorized maintenance. The watcher
submits funded stumps immediately and maintains other trees near expiry.

Run one pre-game pass with:

```bash
WOODLAND_RENEWAL_STARTUP=1 ./scripts/regtest.sh renew-world
```

`run-web.sh` starts one file-locked tree watcher. The Axum server separately
handles opted-in delegated player renewal and exposes both states at `/health.json`.

## Mutinynet

Create ignored configuration at `.cache/mutinynet.env`, then run the world and
renewal process:

```bash
./scripts/run-mutinynet.sh
```

Initial deployment needs the deployer and rollover children because the
deployer signs every manifest field and genesis metadata commits both public
keys. After `ensure`, the launcher removes the deployer child. The tree watcher
and optional delegated-renewal server receive the lower-authority rollover
child; neither receives the deployer secret, and no secret reaches GitHub Pages.
The script writes the live manifest to its configured ignored path and builds a
local `dist/` bundle.

To publish Pages after a real deployment, copy the verified public signed
schema-4 manifest to a deliberate tracked deployment path, then set
`WOODLAND_PAGES_MANIFEST` to that path. Set `WOODLAND_SERVER_URL` to enable the
optional social, leaderboard, and renewal-delegation UI.

The browser contacts the manifest-pinned public Arkade service and stock
emulator directly. The manifest and GitHub Pages artifact contain no secrets.

## Deployment Configuration

The operator accepts:

```text
WOODLAND_ARKADE_SERVICE_URL
WOODLAND_EMULATOR_URL
WOODLAND_NETWORK
WOODLAND_EXPECTED_ARKADE_SIGNER
WOODLAND_EXPECTED_ARKADE_VERSION
WOODLAND_EXPECTED_EMULATOR_SIGNER
WOODLAND_EXPECTED_EMULATOR_VERSION
WOODLAND_DEPLOYER_SECRET
WOODLAND_ROLLOVER_SECRET
WOODLAND_WORLD_MANIFEST
```

The unified server additionally accepts:

```text
WOODLAND_SERVER_PUBLIC_URL
WOODLAND_SERVER_BIND
WOODLAND_SERVER_WEB_ROOT
WOODLAND_SERVER_DB
WOODLAND_SERVER_REFRESH_SECONDS
```

Mainnet requires HTTPS and explicit service pins. Reconfirm signer/version values
independently before creating irreversible assets.

## Tests

```bash
./scripts/test-regtest.sh smoke
./scripts/test-regtest.sh full
./scripts/test-regtest.sh progression
./scripts/test-regtest.sh soak
./scripts/test-regtest.sh chaos
./scripts/test-regtest.sh regrowth
```

Smoke uses two browsers and full uses four. Progression farms both materials,
crafts all three axe tiers, rejects stale recipe retries, and renews Iron Axe
state. Soak defaults to 12 independent players racing one shared tree for 30
rounds; player count, rounds, tree groups,
activation/race concurrency, inter-round delay, and rotating browser reloads are
configurable through `WOODLAND_SOAK_*`. All profiles serve the bundle and API from
`woodland-server` on port 8090.

For repeated fresh-world full and soak cycles over a fixed duration:

```bash
./scripts/test-overnight.sh
```

It stops at the first failure and writes an atomic summary plus per-cycle
artifacts under `regtest/_build/overnight/`.

For a four-hour matrix covering 24-player bursts, four simultaneous tree groups,
browser reload recovery, renewal, and one-batch stump regrowth:

```bash
./scripts/test-aggressive.sh
```

The profiles are destructive only to resources owned by this checkout. If port
3000 is already in use, set `MEMPOOL_WEB_PORT` to a free host port.

## Security

Regtest uses deterministic keys, fixed passwords, unauthenticated HTTP, and a
test intent-fee policy (1% per offchain input by default). Published fixture
ports bind to `127.0.0.1`. Never expose this stack or reuse fixture secrets.
