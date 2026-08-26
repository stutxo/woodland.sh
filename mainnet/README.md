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
├─ maintenance
└─ rollover
```

`woodland-keygen` uses BIP39 with an empty passphrase and hardened BIP32
children. The project namespace is the hardened integer `1464815428'`
(`0x574f4f44`, ASCII `WOOD`), followed by protocol `1'`, Bitcoin network `0'`,
and the role:

```text
deployer:    m/1464815428'/1'/0'/0'
maintenance: m/1464815428'/1'/0'/1'
rollover:    m/1464815428'/1'/0'/2'
```

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
./target/release/woodland-keygen generate /media/offline/woodland-mainnet-v1
```

The command refuses to overwrite an existing destination and creates:

```text
deployment-root.txt  24-word deployment root
operations-root.txt  24-word operations root
deployment.env       derived deployer child only
maintenance.env      derived maintenance and rollover children only
public.json          fingerprints, paths, and x-only public keys
README.txt           handling reminder
```

On Unix, the directory is created as `0700` and every file as `0600`.
Immediately move the two root files to separate offline backup media. Never copy
either root file to the maintenance host.

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

Require `recovery verified` and compare all three x-only public keys manually.
Delete `/tmp/woodland-recovered` after the rehearsal.

## Deployment environment

Copy `deployment.env.example` to an ignored file with mode `0600`. Fill service
pins independently, then copy in the three child secrets from `deployment.env`
and `maintenance.env` generated during the ceremony.

The deployment machine temporarily needs all three children because the final
manifest commits the maintenance and rollover public keys. It never needs either
root mnemonic.

Follow the deployment sequence in the repository `README.md`:

1. run `woodland-operator status` before funding;
2. fund the exact Arkade address and amount it reports;
3. run `woodland-operator ensure` until complete;
4. require `woodland-operator status` to report `ready`;
5. verify ten tree VTXOs and fixed asset supplies;
6. back up and commit the public manifest.

After verification, remove `WOODLAND_DEPLOYER_SECRET` from online systems. If
the deployer has no spendable VTXO or change, its deployment root may be archived
or destroyed. Keep the operations root offline permanently.

## Maintenance host

Build `woodland-operator` for the maintenance host and install:

```text
/opt/woodland/woodland-operator
/etc/woodland/woodland-world.json
/etc/woodland/mainnet.env
/etc/systemd/system/woodland-maintenance.service
```

Use:

```text
/opt/woodland/woodland-operator       root:root      0555
/etc/woodland/woodland-world.json     root:root      0444
/etc/woodland/mainnet.env             root:woodland  0640
```

Populate `mainnet.env` from `maintenance.env.example` and the generated
`maintenance.env`. It must not contain the deployer child or either root.

Enable the watcher:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now woodland-maintenance.service
sudo journalctl -u woodland-maintenance.service -f
```

Require the log line:

```text
woodland.sh maintenance watcher ready
```

Run one active watcher. Additional operators should remain passive standbys;
concurrent watchers can race the same tree outpoints. Monitor process uptime,
reconnect errors, service `/v1/info` availability, and tree renewal failures.
The host clock must remain synchronized because the regrowth delay is signer
policy rather than a covenant clock.

## Optional game-server host

Build the Axum server and a same-origin web bundle separately from maintenance:

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
players. `WOODLAND_SERVER_WEB_ROOT` makes Axum serve the bundle on that same
origin. `WOODLAND_SERVER_ORIGIN` is needed only to allow a separate static
frontend such as GitHub Pages. Public origins must use HTTPS on mainnet.

The optional `WOODLAND_ROLLOVER_SECRET` enables signed player delegation. The
process must never receive the deployer child, maintenance child, either root
mnemonic, or a player key. Compromise of the rollover child can force or race
exact-self-send renewals, but the covenant does not let it transfer or mutate
player state.

Terminate TLS in a reverse proxy in front of the loopback listener. The same
public origin serves `/`, static bundle files, and:

```text
GET  /health.json
GET  /v1/leaderboard
GET  /v1/social
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
`delegationAvailable`, process restarts, registry size, Arkade/emulator latency,
and disk writes. Signed registration and delegation persist in
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
