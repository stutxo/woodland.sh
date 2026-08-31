# woodland.sh Mainnet Operations

This directory contains public templates only. Never commit generated mnemonics,
child private keys, filled environment files, or deployment logs containing
secrets.

## Two-root key hierarchy

Use two independent 24-word BIP39 roots:

```text
deployment root (offline, disposable after verification)
└─ deployer

operations root (permanent offline recovery material)
└─ rollover
```

`woodland-keygen` uses BIP39 with an empty passphrase and hardened BIP32
children. The project namespace is the hardened integer `1464815428'`
(`0x574f4f44`, ASCII `WOOD`), followed by the stable key-derivation namespace
`1'`, Bitcoin network `0'`, and the role:

```text
deployer:    m/1464815428'/1'/0'/0'
rollover:    m/1464815428'/1'/0'/1'
```

Role `0'` derives the deployer child from the deployment root. Role `1'` derives
the rollover child from the independent operations root. Tree renewal and
two-tip regrowth are permissionless covenant leaves, so no additional
project-held lifecycle key exists.

The paths and `public.json` are public. The mnemonic and derived secret files are
not. Do not add a memory-only BIP39 passphrase; this utility intentionally does
not support one.

Build dependencies before disconnecting the ceremony machine, or transfer a
release binary from a trusted build host together with a recorded SHA-256
checksum. Key generation itself must happen offline; an air-gapped machine
cannot fetch missing Cargo dependencies.

## Offline generation ceremony

Build and run the utility on an offline Linux machine from a reviewed checkout:

```bash
cargo build --release --locked --features keygen --bin woodland-keygen
umask 077
./target/release/woodland-keygen generate /media/offline/woodland-mainnet-v2
```

The command refuses to overwrite an existing destination and creates:

```text
deployment-root.txt  24-word deployment root
operations-root.txt  24-word operations root
deployment.env       derived deployer child only
operations.env       derived rollover child for the optional player watchtower
public.json          fingerprints, paths, and x-only public keys
README.txt           handling reminder
```

On Unix, the directory is created as `0700` and every file as `0600`.
Immediately move the two root files to separate offline backup media. Never copy
either root file to the watcher host.

Recommended minimum backup:

- two geographically separate physical copies of each root;
- exact derivation paths printed with each copy;
- a printed copy of `public.json`;
- one recovery rehearsal before deployment.

## Recovery rehearsal

Recover child files into a new empty directory:

```bash
woodland-keygen recover \
  /media/backup-a/deployment-root.txt \
  /media/backup-b/operations-root.txt \
  /tmp/woodland-recovered
```

Verify recovered public keys against the original ceremony record:

```bash
woodland-keygen verify \
  /media/backup-a/deployment-root.txt \
  /media/backup-b/operations-root.txt \
  /path/to/original/public.json
```

Require `recovery verified` and compare both x-only public keys manually.
Delete `/tmp/woodland-recovered` after the rehearsal.

## Deployment environment

Copy `deployment.env.example` to an ignored file with mode `0600`. Fill service
pins independently, then copy in both child secrets from `deployment.env`
and `operations.env` generated during the ceremony.

The deployment machine temporarily needs both children because the final
manifest commits the rollover public key. It never needs either root mnemonic.
The manifest also commits the Arkade service's forfeit public key and forfeit
address at genesis; renewal clients reject any batch or payout that
does not match them, so treat a changed forfeit identity on the service as a
new deployment requiring a new manifest, not a resumable one.

The Arkade service must include upstream arkd commit
`c7c3184f5cd416e231023f717489a5b0550960cc` or its equivalent offchain
cache/DB projection fix. Older builds can accept concurrent spends of one VTXO
after finalization removes its live reservation but before DB projection marks
it spent. The Woodland contention soak reproduced permanent TREE, LOG, and XP
supply inflation on the older pinned build. Verify this fix independently in
the provider's exact version before funding.

`WOODLAND_EMULATOR_URL` must be the public HTTPS woodland gate, not a stock
emulator endpoint. Run the pinned stock emulator on loopback behind that gate
and give the gate loopback access to a fully validated Bitcoin Core node. The
gate proxies `/v1/info` unchanged, so the independently verified emulator signer
and version pins still refer to the stock emulator.

Follow the deployment sequence in the repository `README.md`:

1. run `woodland-operator status` before funding;
2. require the proposed manifest to report schema 2 and protocol 2;
3. fund clean VTXOs at the reported address whose total exactly equals the
   reported amount (138,600 sats at 330-sat dust);
4. run `woodland-operator ensure` until complete;
5. require `woodland-operator status` to report `ready`;
6. verify exactly 420 tree VTXOs, each with 50,000 LOG/XP, health ten, stump
   height zero, and fixed total asset supplies;
7. back up and commit the public manifest.

After verification, remove `WOODLAND_DEPLOYER_SECRET` from online systems. If
the deployer has no spendable VTXO or change, its deployment root may be archived
or destroyed. Copy `WOODLAND_ROLLOVER_SECRET` only to an optional game-server
host when delegated player renewal is enabled; otherwise keep it offline. It has
no ongoing use on the deployment machine or tree watcher. Keep the operations
root offline permanently.

## Emulator gate host

The gate, stock emulator, and Bitcoin Core RPC form one security boundary. Run
the stock emulator at `127.0.0.1:7073`, Bitcoin Core RPC at
`127.0.0.1:8332`, and the gate at `127.0.0.1:7074`. Firewall both private
ports; never publish the stock emulator or RPC endpoint.

Build and install the gate:

```bash
cargo build --release --locked --features server --bin woodland-emulator-gate
sudo useradd --system --home /nonexistent --shell /usr/sbin/nologin woodland-gate
```

```text
/opt/woodland/woodland-emulator-gate                 root:root           0555
/etc/woodland/emulator-gate.env                      root:woodland-gate  0640
/etc/systemd/system/woodland-emulator-gate.service   root:root           0644
```

Populate `emulator-gate.env` from `emulator-gate.env.example`. Use a dedicated
Bitcoin Core `rpcauth` credential and the one canonical HTTPS frontend origin.
The RPC credential grants access to a security-sensitive Core endpoint; do not
place it in the world manifest, reverse-proxy configuration, service command
line, or logs.

Terminate TLS in a reverse proxy and expose the gate origin from
`WOODLAND_EMULATOR_URL`. Forward only to `127.0.0.1:7074`, preserve request
bodies, permit at least the gate's 32 MiB body limit, and rate-limit POST routes
by source IP. The gate's exact CORS origin must match the deployed frontend.
CORS is not an authentication or denial-of-service boundary. A separate Pages
frontend therefore requires its own configured origin rather than a wildcard.

Enable and verify:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now woodland-emulator-gate.service
sudo journalctl -u woodland-emulator-gate.service -f
curl --fail https://emulator-gate.example/health.json
curl --fail https://emulator-gate.example/v1/block-tip
```

Require `ready: true`; compare the returned height and hash to the local
`bitcoin-cli getblockchaininfo`, and require `/v1/info` to match the independently
recorded stock-emulator signer and version. Monitor gate restarts, Core RPC
latency, `/health.json`, `/v1/block-tip`, and rejected attestation counts at the
reverse proxy. A dishonest or bypassed gate can waive the two-tip delay.

## Renewal watcher host

Build `woodland-operator` for the watcher host and install:

```text
/opt/woodland/woodland-operator
/etc/woodland/woodland-world.json
/etc/woodland/mainnet.env
/etc/systemd/system/woodland-renewal.service
```

Use:

```text
/opt/woodland/woodland-operator       root:root      0555
/etc/woodland/woodland-world.json     root:root      0444
/etc/woodland/mainnet.env             root:woodland  0640
```

Populate `mainnet.env` from `operations.env.example`; do not copy in either
generated child secret or either root. The tree watcher receives no private key.
Tree renewal and regrowth are permissionless `{operator, emulator}` covenant
self-sends. Active renewal preserves all tree state. A funded zero-health stump
regrows to health ten only after the emulator gate observes two Bitcoin tip
advances since the final chop; a zero-reserve stump remains terminal.

Enable the watcher:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now woodland-renewal.service
sudo journalctl -u woodland-renewal.service -f
```

Require the log line:

```text
woodland.sh renewal watcher ready
```

Active-tree renewal triggers when remaining lifetime drops below half the
observed batch lifetime, clamped to a maximum of 12 hours. Eligible funded
stumps are selected as soon as the gate reports `stumpHeight + 2`. Trees
deployed in the same session expire close together, so expect correlated
renewal waves; the watcher renews up to eight trees concurrently and reports a
`missingExpiry` count whenever the indexer omits a live tree's expiry. Any
nonzero count, renewal failure, or persistent gate-tip failure warrants
immediate attention: an expired tree is swept by the Arkade service and cannot
be recovered. A forced rollover bypasses earliness and safety-margin checks but
never expiry, local-reserve invariants, or the two-tip regrowth requirement.

Registration failures and abandoned pre-forfeit joins are cleaned up by a
signed ownership proof before the watcher retries, including queued intents
left by a prior process. Once forfeits have reached the emulator, the watcher
preserves the intent and requires indexed lineage reconciliation rather than
deleting potentially committed state.

```bash
sudo WOODLAND_FORCE_ROLLOVER=1 /opt/woodland/woodland-operator renew /etc/woodland/woodland-world.json tree <tree-id>
```

Run one active watcher. Additional operators should remain passive standbys;
concurrent watchers can race the same tree outpoints. Monitor process uptime,
reconnect errors, Arkade `/v1/info`, gate `/health.json` and `/v1/block-tip`,
Bitcoin tip lag, and tree renewal failures.

## Optional game-server host

Build the Axum server and a same-origin web bundle separately from the watcher:

```bash
cargo build --release --locked --features server --bin woodland-server
WOODLAND_SERVER_URL=self \
  WOODLAND_WASM_FEATURES=woodland-app \
  ./scripts/build-web.sh
sudo useradd --system --home /nonexistent --shell /usr/sbin/nologin woodland-server
```

Install:

```text
/opt/woodland/woodland-server
/opt/woodland/web/
/etc/woodland/woodland-world.json
/etc/woodland/server.env
/etc/systemd/system/woodland-server.service
```

Use:

```text
/opt/woodland/woodland-server       root:root             0555
/opt/woodland/web/                  root:root             0555
/etc/woodland/woodland-world.json   root:root             0444
/etc/woodland/server.env            root:woodland-server  0640
```

Populate `server.env` from `server.env.example`.
`WOODLAND_SERVER_PUBLIC_URL` is the canonical public app/API origin signed by
players. Registration signatures bind that origin and the world genesis, so
changing the public URL or pointing the server at a redeployed world
invalidates every stored registration and the server refuses to start: stop
it, remove `players.json`, and let players re-register.
`WOODLAND_SERVER_WEB_ROOT` makes Axum serve the bundle on that same origin.
`WOODLAND_SERVER_ORIGIN` is needed only to allow a separate static frontend
such as GitHub Pages. Public origins must use HTTPS on mainnet.

The optional `WOODLAND_ROLLOVER_SECRET` enables signed player delegation. The
process must never receive the deployer child, either root mnemonic, or a
player key. Compromise of the rollover child can force or race exact-self-send
player renewals, but cannot transfer or mutate player or tree state.

Terminate TLS in a reverse proxy in front of the loopback listener. The same
public origin serves `/`, static bundle files, and:

```text
GET  /health.json
GET  /v1/presence?minX=&minY=&maxX=&maxY=
GET  /v1/chat
GET  /v1/leaderboard?limit=&offset=
POST /v1/players
POST /v1/location
POST /v1/chat
POST /v1/delegation
```

Cap request bodies at 4 KiB and rate-limit all POST routes by source IP. CORS is
not an authentication or denial-of-service boundary. Chat has no moderation
system; set an abuse policy before public launch. Choose and disclose a short
access-log retention period because requests link IP addresses to public
PLAYER_ID values.

Enable the service:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now woodland-server.service
sudo journalctl -u woodland-server.service -f
curl --fail https://replace-with-server-api.example/health.json
```

Monitor `ready`, `lastRefreshAt`, `lastError`, `onlinePlayers`,
`delegationAvailable`, process restarts, registry size, Arkade/emulator-gate
latency, and disk writes. Signed registration and delegation persist in
`/var/lib/woodland-server/players.json`; presence expires after 60 seconds and
the newest 200 chat messages remain in memory only. Registration has no
self-service deletion API. Honor removal requests by stopping the service,
removing the entry from a backup copy, validating the JSON, atomically replacing
the file, and restarting. Game-server failure must not be treated as gameplay
downtime.

## Separate GitHub Pages Option

Same-origin Axum hosting is the default. To host only the static bundle on
GitHub Pages instead, commit the public manifest, set `WOODLAND_PAGES_MANIFEST`
to its path, and set `WOODLAND_SERVER_URL` to the canonical external Axum
origin. Leaving it unset removes the social UI and API origin from the Pages
artifact. Push `main`; the gated Pages workflow builds and deploys the static
site only after CI and regtest pass.

Set `WOODLAND_EMULATOR_GATE_ORIGIN` to the Pages origin before publishing; the
gate intentionally supports one exact browser origin, not wildcard CORS.
