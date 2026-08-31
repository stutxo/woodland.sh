# Security Policy

## Supported versions

Security fixes target protocol v1 and the latest source.

| Version | Supported |
| --- | --- |
| v1 | Yes |

## Project status

woodland.sh v1 has an experimental mainnet deployment path. The repository ships
no live deployment manifest. Mainnet availability is not a claim of
production-grade browser custody or an independent security audit. Documented
trust assumptions and known boundaries are part of the protocol design rather
than guarantees provided by this policy.

## Report a vulnerability

Send vulnerability reports privately to
[`stutxo@proton.me`](mailto:stutxo@proton.me). Encrypt sensitive reports with
the repository's [`SECURITY-PGP.asc`](SECURITY-PGP.asc) key.

```text
Fingerprint: 2985 A7C4 18D7 7EB6 EEEF 608D 4B57 B009 340A EBD2
Key ID:      4B57 B009 340A EBD2
```

The same public key is independently published by
[GitHub](https://github.com/stutxo.gpg) and
[keys.openpgp.org](https://keys.openpgp.org/vks/v1/by-fingerprint/2985A7C418D77EB6EEEF608D4B57B009340AEBD2).
Verify the full fingerprint before encrypting.

Include the affected commit or version, impact, prerequisites, reproduction
steps, and proposed mitigation. Remove wallet secrets, private keys, profiles,
PSBTs containing sensitive metadata, passwords, tokens, and capabilities from
examples.

Do not disclose exploit details in a public issue or discussion before a fix
and coordinated disclosure date have been agreed. Reports are handled on a
best-effort basis; this project does not promise a response or remediation SLA.

## Relevant scope

Security-sensitive areas include:

- Arkade Script bypasses in player chop, tree chop, stump-refill renewal, or
  retire-and-restock; player renewal or LOG withdrawal; or vault restock or
  renewal.
- TREE, LOG, XP, or PLAYER_ID inflation, substitution, assignment, control,
  metadata, or conservation failures.
- Soulbound-XP bypasses: any path that moves XP out of player state, detaches
  the numeric XP packet from the XP asset balance, or smuggles XP through the
  withdrawal or renewal leaves.
- Vault restock conservation or identity failures: wrong reserve amounts, a
  replacement tree at the wrong coordinate, duplicated or redirected TREE
  markers, or vault change that leaks supply.
- Withdraw-leaf LOG leakage: sats, PLAYER_ID, XP, or more LOG than declared
  leaving player state, or a destination funded by anything but the wallet
  dust input.
- PLAYER_ID/profile confusion, competing lineage selection, activation retry
  divergence, or bypass of canonical initial player roll and luck credit.
- Forged player identity, position, XP, health, roll, credit, or reward transitions.
- Incorrect Taproot signer closures, emulator tweaks, PSBT response checks,
  checkpoint signatures, batch graph validation, or forfeit handling.
- Manifest validation, signer or service pinning, creating-transaction binding,
  or indexed-asset reconstruction failures.
- Pending-transaction journaling, unknown-submission recovery, replay, or crash
  consistency failures.
- Deterministic key derivation, mnemonic exposure, incorrect recovery, output
  permissions, or role/path confusion in `woodland-keygen`.
- Browser signing-key or PLAYER_ID-profile disclosure, unsafe persistence, or
  direct-service origin policy failures.
- Server registration or action-signature replay across origins, forged
  leaderboard state, location/chat injection, registry corruption, or CORS
  exposure beyond the configured frontend origin.
- Unauthorized delegation, rollover-key mismatch or disclosure, unsafe
  watchtower renewal, expiry-policy bypass, and exploitable races caused by
  outpoint rotation.

Public deterministic player luck, PLAYER_ID Sybil and identity grinding,
unprovable pre-covenant PLAYER_ID ancestry, non-covenant map location and
adjacency, chat content and moderation, Arkade/emulator/server availability,
request-rate abuse, public identity aggregation, signer retirement without
rotation, NUMS-exit recovery limits, and the reference browser's localStorage
custody are known boundaries documented in [`README.md`](README.md),
[`PLAYER.md`](PLAYER.md), and [`TREE.md`](TREE.md). Player-bound entropy
prevents choosing a favorable tree but does not make permissionless identities
unique. An intermediate marker output can select any starting roll and credit
within the corridor; the recursive rate budget and streak bounds remain enforced.
Stump refill and vault restock are permissionless covenant paths — no signer
clock or project-held key gates them — and the operator watcher is a convenience,
not a trust root.

Service-endpoint authentication rests on HTTPS and the manifest's URL, signer,
and forfeit pins; arkd's per-boot ephemeral batch-operator key cannot be pinned,
so an attacker who defeats those channels could disrupt or strand renewal
batches but cannot redirect the pinned sweep key, forfeit payout, or covenant
outputs.
