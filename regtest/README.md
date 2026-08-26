# woodland.sh Regtest

This directory provides the minimal local stack used by woodland.sh protocol v1:
Bitcoin Core, indexers, stock arkd/arkd-wallet, Redis, and the Arkade Script
emulator.

## Run

```bash
./scripts/regtest.sh clean --force
./scripts/run-web.sh
```

Open `http://127.0.0.1:8000/`. The browser calls local arkd on 7070 and the
emulator on 7073 directly. The development server serves the same `dist/`
bundle used by GitHub Pages and supervises one local maintenance watcher.

Protocol v1 uses schema 1 and storage under `woodland.sh:web:v1:*`. A fresh
world creates three fixed-supply groups:

```text
group 0:  10 TREE
group 1: 100 LOG
group 2: 100 XP_FUEL
```

Each tree receives one TREE, ten LOG, ten XP_FUEL, health five, and 1,980 sats.
Bootstrap funding is 19,800 sats. There is no control asset, PLAYER_TICKET,
allocator reserve, invitation, or player registry.

A browser wallet receives one exact 330-sat VTXO and, in one transaction,
issues a unique uncontrolled PLAYER_ID into recursive player state. Harvested
LOG and XP_FUEL remain in that state; numeric XP must equal held XP_FUEL.

## Commands

```text
./scripts/regtest.sh start
./scripts/regtest.sh start-tree
./scripts/regtest.sh maintain
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
8b34e352859595cc03ba22ffa35088ab88b87fd9
```

The source defaults to `.cache/arkd-stock`. Startup verifies expected image tags
and emulator version before deployment. Protocol v1 requires no custom server
patch.

## Maintenance

The manifest pins distinct `maintenanceSigner` and `rolloverSigner` keys.
Maintenance authorizes only health reset. Rollover authorizes exact tree/player
self-sends for an optional watchtower. Active players renew directly with their
owner key; neither the Pages site nor the development server exposes a gameplay
API.

Run one pre-game pass with:

```bash
WOODLAND_MAINTENANCE_STARTUP=1 ./scripts/regtest.sh maintain
```

`run-web.sh` starts one file-locked watcher for timed regrowth and tree
rollover. Local maintenance health is exposed at `/health.json`.

## Mutinynet

Create ignored configuration at `.cache/mutinynet.env`, then run the world and
maintenance process:

```bash
./scripts/run-mutinynet.sh
```

Required secrets are deployer, maintenance, and rollover keys. They remain on
the maintenance host and are never provided to GitHub Pages. The script also
builds a local `dist/` bundle.

Push `main` to trigger the gated `pages-build` and `pages-deploy` jobs. Select
**GitHub Actions** as the repository's Pages source. The workflow uses
`mutinynet/woodland-world.json` unless the repository variable
`WOODLAND_PAGES_MANIFEST` names another tracked manifest.

The browser contacts the manifest-pinned public Arkade and emulator endpoints
directly. The manifest and GitHub Pages artifact contain no secrets.

## Production-Compatible Configuration

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
WOODLAND_TREE_MAINTENANCE_SECRET
WOODLAND_ROLLOVER_SECRET
WOODLAND_WORLD_MANIFEST
```

Mainnet requires HTTPS and explicit service pins. Reconfirm signer/version values
independently before creating irreversible assets.

## Tests

```bash
./scripts/test-regtest.sh smoke
./scripts/test-regtest.sh full
```

The profiles are destructive only to resources owned by this checkout. If port
3000 is already in use, set `MEMPOOL_WEB_PORT` to a free host port.

## Security

Regtest uses deterministic keys, fixed passwords, unauthenticated HTTP, and
zero-fee accounting. Published fixture ports bind to `127.0.0.1`. Never expose
this stack or reuse fixture secrets.
