# woodland.sh Regtest

This directory provides the minimal local stack used by woodland.sh protocol v1:
Bitcoin Core, indexers, stock arkd/arkd-wallet, Redis, and the Arkade Script
emulator.

## Run

```bash
./scripts/regtest.sh clean --force
./scripts/run-web.sh
```

Open `http://127.0.0.1:8000/`. `woodland-server` serves both `dist/` and every
`/v1/*` API on that origin. Gameplay still calls local arkd on 7070 and the
emulator on 7073 directly; a separate local renewal watcher runs alongside
Axum.

Protocol v1 uses manifest schema 1. A fresh world creates three fixed-supply
groups:

```text
group 0:  2,100 TREE
group 1: 21,000,000 LOG
group 2: 21,000,000 XP
```

Each tree receives one TREE, 1,000 LOG, 1,000 XP, health five, and 330 sats.
The supply vault holds the remaining 18,900,000 LOG and 18,900,000 XP with
330 sats. Bootstrap funding is 693,330 sats. There is no control asset,
PLAYER_TICKET, allocator reserve, invitation, or player registry.

A browser wallet receives one exact 330-sat VTXO and, in one transaction,
issues a unique uncontrolled PLAYER_ID into recursive player state with its
canonical roll and initial 8,000 luck credit. Harvested LOG and XP remain in
that state; numeric XP must equal held XP.

## Commands

```text
./scripts/regtest.sh start
./scripts/regtest.sh start-tree
./scripts/regtest.sh renew-world
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

## Stock arkd

The wrapper checks out unmodified commit:

```text
c7c3184f5cd416e231023f717489a5b0550960cc
```

The source defaults to `.cache/arkd-stock`. Startup verifies expected image tags
and emulator version before deployment. Protocol v1 requires no custom server
patch.

## Renewal

The manifest pins one `rolloverSigner` for optional player watchtower
authorization. Active players renew directly with their owner key. Tree and
vault renewal are permissionless covenant self-sends; the watcher submits them
before expiry and refills stumps without altering their reserves.

Run one pre-game pass with:

```bash
WOODLAND_RENEWAL_STARTUP=1 ./scripts/regtest.sh renew-world
```

`run-web.sh` starts one file-locked watcher for tree, vault, and optional
delegated-player renewal. Local watcher health is exposed at `/health.json`.

## Mutinynet

Create ignored configuration at `.cache/mutinynet.env`, then run the world and
renewal process:

```bash
./scripts/run-mutinynet.sh
```

Required secrets are the deployer and rollover keys. They remain on the
operator host and are never provided to GitHub Pages. The script writes the live
manifest to its configured ignored path and builds a local `dist/` bundle.

To publish Pages after a real deployment, copy the verified public schema-1
manifest to a deliberate tracked deployment path, then set
`WOODLAND_PAGES_MANIFEST` to that path. Set `WOODLAND_SERVER_URL` to enable the
optional social, leaderboard, and renewal-delegation UI.

The browser contacts the manifest-pinned public Arkade and emulator endpoints
directly. The manifest and GitHub Pages artifact contain no secrets.

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
./scripts/test-regtest.sh soak
./scripts/test-regtest.sh restock
```

Smoke uses two browsers and full uses four. Soak defaults to 12 independent
players racing one shared tree for 30 rounds; player count, rounds, tree groups,
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
browser reload recovery, and sustained stump renewal:

```bash
./scripts/test-aggressive.sh
```

The profiles are destructive only to resources owned by this checkout. If port
3000 is already in use, set `MEMPOOL_WEB_PORT` to a free host port.

## Security

Regtest uses deterministic keys, fixed passwords, unauthenticated HTTP, and
zero-fee accounting. Published fixture ports bind to `127.0.0.1`. Never expose
this stack or reuse fixture secrets.
