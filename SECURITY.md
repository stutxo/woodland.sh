# Security Policy

## Supported versions

Security fixes are made against the current protocol v1 release and its latest
source. Pre-release protocol generations are unsupported and are not migrated.

| Version | Supported |
| --- | --- |
| Current v1 release | Yes |
| Pre-release generations | No |

## Project status

woodland.sh v1 is exercised on Mutinynet and includes an experimental mainnet
deployment path. Mainnet availability is not a claim of production-grade browser
custody or an independent security audit. The documented trust assumptions and
known boundaries are part of the protocol design rather than guarantees
provided by this policy.

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

- Arkade Script bypasses in player chop, tree chop, regrowth, or renewal.
- TREE, LOG, XP_FUEL, or PLAYER_ID inflation, substitution, assignment, control,
  metadata, or conservation failures.
- PLAYER_ID/profile confusion, competing lineage selection, or activation retry
  divergence.
- Forged player identity, position, XP, health, roll, or reward transitions.
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
- Unauthorized regrowth or renewal, expiry-policy bypass, and exploitable
  gameplay races caused by outpoint rotation.

Public deterministic randomness, frontend-only movement and adjacency, Sybil
creation, Arkade or emulator availability, the maintenance-controlled regrowth
clock, signer retirement without rotation, NUMS-exit recovery limits, and the
reference browser's localStorage custody are known boundaries documented in
[`README.md`](README.md), [`PLAYER.md`](PLAYER.md), and [`TREE.md`](TREE.md).
