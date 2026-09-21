# Security Policy

## Supported versions

Security fixes target protocol v4 and the latest source.

| Version | Supported |
| --- | --- |
| v4 | Yes |
| v3 and earlier | No |

## Project status

woodland.sh v4 has an experimental mainnet deployment path. The repository
ships no live deployment manifest. Mainnet availability is not a claim of
production-grade browser custody or an independent security audit. Documented
trust assumptions and known boundaries are part of the protocol design rather
than guarantees provided by this policy.

Package 4.0.0 requires protocol 4, signed schema 4, and ruleset
`woodland.sh/forest/v4`. Launch a fresh v4 genesis. Never reuse v3 assets,
deployment outpoints, or manifests: these covenant changes cannot upgrade an
existing world in place.

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

- Arkade Script bypasses in player chop, tree chop, tree regrowth or
  maintenance, player renewal, or LOG withdrawal.
- Player-template authentication bypasses: accepting an arbitrary script,
  changed spending leaf, extra escape leaf, or spendable internal key instead
  of the canonical six-leaf player contract. Tree authentication uses the owner
  key and compressed-output prefix witness; player covenants bind the immutable
  TREE AssetId to avoid a circular script dependency.
- Axe progression bypasses, including equipping or crafting Wooden before one
  earned XP asset unit (25 Woodcutting XP), or bypassing Stone/Iron level gates.
- TREE, LOG, XP, or PLAYER_ID inflation, substitution, assignment, control,
  metadata, or conservation failures.
- Soulbound-XP bypasses: any path that moves XP out of player state, derives
  progression from anything but `25 ×` its XP asset balance, changes the signed
  `woodcuttingXpPerLog` scale, or smuggles XP through withdrawal or renewal.
- Tree-local reserve or regrowth failures: minted or redirected LOG/XP,
  changed immutable state or sats, an active tree incorrectly resetting health,
  a funded stump failing to regrow in one batch, or a terminal stump becoming
  active.
- Maintenance-authority failures: bypass of the rollover signer or any
  maintenance change to health, assets, script, state, or sats.
- Renewal-fee failures: state value funding fees, asset-bearing fee inputs,
  incorrect current/scheduled policy selection, sub-minimum change, or fee-input
  signer bypass.
- Withdraw-leaf LOG leakage: sats, PLAYER_ID, XP, or more LOG than declared
  leaving player state, or a destination funded by anything but the wallet
  dust input.
- PLAYER_ID/profile confusion, competing lineage selection, activation retry
  divergence, or bypass of canonical initial player roll and luck credit.
- Forged tree state, health, player roll, luck credit, or reward transitions.
- Incorrect Taproot signer closures, emulator tweaks, PSBT response checks,
  checkpoint signatures, batch graph validation, or forfeit handling.
- Manifest signature, deployer or rollover binding, signer or service pinning,
  creating-transaction binding, genesis metadata, or indexed-asset
  reconstruction failures.
- Stock-emulator endpoint failures: signer/version substitution, incorrect
  Arkade Script execution, request tampering, or serving an origin outside the
  configured policy.
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
Equipped axes remain constrained by their earned-XP thresholds, so pre-entry
luck selection does not authorize a free starting axe.
Funded-stump regrowth is permissionless apart from its operator and tweaked
emulator closure; no project-held lifecycle key gates it. Active-tree and
terminal-stump maintenance additionally requires the low-authority rollover
signer and preserves state exactly.

Service-endpoint identity is committed by the deployer-signed manifest,
including URL, signer, forfeit, ruleset, asset, and script pins. HTTPS still
authenticates and protects each live connection. arkd's per-boot ephemeral
batch-operator key cannot be pinned, so an attacker who defeats those channels
could disrupt or strand renewal batches but cannot redirect the pinned sweep
key, forfeit payout, or covenant outputs. The pinned stock emulator remains a
trust boundary for Arkade Script execution.
