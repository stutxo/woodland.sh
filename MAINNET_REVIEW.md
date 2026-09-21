# Mainnet readiness and remediation — 2026-09-21

The six code findings from the initial review have been addressed in the current
working tree. The changes are a **protocol v4 release requiring a fresh genesis**.
Do not fund mainnet until the release verification below is complete. No live
world was changed or deployed.

## Changes

1. **Complete player covenant authentication.** The tree reconstructs all six
   canonical player Taproot leaves and the NUMS internal key, then verifies the
   resulting output key and the executed chop program. Additional escape leaves,
   altered signers, alternate internal keys, and mismatched owners are rejected.
   The tree witness supplies the owner and compressed output parity. To avoid a
   circular script commitment, player chop authenticates the immutable world
   TREE asset, which remains locked in canonical tree contracts from genesis.
   See `src/player_template.rs`, `src/tree.rs`, and `src/player.rs`.

   Every player transition also checks that the equipped axe is backed by earned
   XP. Wooden now requires the first earned XP unit (25 displayed XP); Stone and
   Iron retain their level gates. Pre-entry roll/luck selection remains a
   documented identity-grinding boundary. Once entered, the complete player
   contract preserves the recursive rules. This security argument requires a
   fresh world with the whole initial resource supply in its canonical trees;
   **v3 assets cannot migrate into v4**.

2. **Single active wallet tab.** A lifetime, origin-wide Web Lock is acquired
   before WASM initialization or any wallet write. A second tab and unsupported
   browsers fail closed. Tests exercise shared storage, activation, restore,
   pending transactions, and lock takeover. See `web/app.js` and
   `scripts/test-browser-recovery.mjs`.

   Follow-up recovery checks also found and fixed stale UI snapshots permitting
   key replacement after a failed submission, and partially written backup
   restores. Destructive guards now inspect durable journals; a staged restore
   journal recovers interrupted writes before wallet initialization.
   Interrupted restores also clear the retired wallet's address and funding
   display. Imported activation journals must match the canonical transaction,
   checkpoint metadata, and valid saved owner signatures before replacing custody.

3. **Delegated renewal health and owner fallback.** Delegation now reflects
   successful worker progress and errors. Verification and renewal run
   independently; `/health.json` returns HTTP 503 while unhealthy. Verification
   has a minimum 120-second freshness window and renewal 720 seconds, allowing
   bounded batch work to finish. An online owner attempts renewal during the
   final 30 minutes when due, even if the delegation API remains responsive.
   See `src/server.rs` and `web/app.js`.

   A second review added conditional cache updates so delayed verification
   cannot replace newer renewal state. Slow registrations preserve current
   delegation consent, and queued renewals recheck consent before starting.

4. **Bounded service-info requests.** Batch connection has a 15-second deadline
   on native and WASM targets, closing the unbounded SDK `get_info()` wait before
   batch registration. A local HTTP blackhole regression verifies the deadline.
   See `src/batch.rs`.

5. **Mainnet reset protection.** Destructive test-wallet and profile-reset
   controls are hidden and handler-blocked on mainnet. Pending transaction
   journals also prevent key replacement. See `web/app.js` and `web/index.html`.

6. **Explicit world selection.** Web builds require an explicit manifest;
   mainnet builds reject a non-Bitcoin manifest. Server startup verifies that
   bundled `world.json` equals the configured, authenticated API manifest. The
   mainnet runbook now supplies the correct build environment. See
   `scripts/build-web.sh`, `src/server.rs`, and `mainnet/README.md`.

Package, manifest schema, protocol, ruleset, and manifest-signature domain are
version 4. Existing cryptographic namespaces and local backup formats retain
compatible versions. Clients and artifact assembly reject v3 manifests.

## Verification

The reusable `scripts/test-covenants.sh` exports actual Rust builder transactions
and runs their covenant bytecode in the unmodified interpreter pinned to the
stock emulator v0.0.7-rc.1. CI now runs this gate. It does not replace live arkd
signature, admission, concurrency, or batch lifecycle testing.

Completed on the revised working tree:

- All-target/all-feature Rust tests: 96 library tests and two key-generation
  tests passed, including the loopback HTTP timeout and intent cleanup tests.
- Stock Arkade VM: **375 vectors passed**, with 58 accepted and 317 rejected;
  covers the complete player template, chop, craft, withdrawal, and renewal.
- Native all-feature Clippy and production WASM Clippy passed with warnings
  denied; Rust formatting passed.
- Browser recovery and web artifact assembly suites passed.
- Rust 1.88 minimum-version compatibility check passed for all targets/features.

A fresh `cargo-audit 0.22.2` run used RustSec database commit `57ad4063`
(2026-09-21): 223 dependencies, **zero known vulnerabilities**, two warnings:

- `paste 1.0.15` is unmaintained (RUSTSEC-2024-0436), through the SDK fee evaluator.
- `secp256k1 0.32.0-beta.2` is yanked. Both Woodland and pinned `ark-core` use it;
  replacement needs a coordinated SDK compatibility update. No associated
  vulnerability or confirmed yank reason was established by this review.

## Remaining launch gates

- Run stock-arkd/emulator smoke, full, progression, regrowth, fee-funded renewal,
  and sustained contention/expiry profiles against a **fresh v4 world and the
  final committed release**. Rehearse the browser lock and interrupted restore
  in real browsers. Docker, Firefox, and Geckodriver were unavailable in this
  environment; older regtest artifacts predate these changes.
- Confirm the production provider includes the required atomic-spend fix
  `c7c3184f5cd416e231023f717489a5b0550960cc` or equivalent, and verify the deployed
  emulator version. A reported service version alone does not establish this.
- Generate and independently verify the production manifest; rehearse offline
  key recovery and player backup recovery; fund watcher fees and monitor worker
  health, minimum remaining lifetime, missing expiry, and fee-wallet balance.
- Browser keys still use the documented localStorage custody model. The lock
  coordinates current clients on one origin; it does not synchronize devices or
  revoke an already-open older client. Close old client tabs during rollout.

These fixes and tests improve readiness; they are not evidence of a completed
production deployment or an independent security audit.
