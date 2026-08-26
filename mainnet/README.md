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

## GitHub Pages

The public manifest contains no secrets. Commit it, set the repository variable
`WOODLAND_PAGES_MANIFEST` to its tracked path, and push `main`. The gated Pages
workflow builds and deploys the static site only after CI and regtest pass.
