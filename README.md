# woodland.sh

woodland.sh is an Arkade woodcutting protocol with recursive player and tree
state. Its exactly 420 trees use one shared covenant template but occupy
independent VTXOs, so unrelated players and trees do not share a mutable input.

The reference browser is a client, not an authority. Arkade Script, fixed Asset
V1 supplies, indexed lineage, the pinned stock emulator, and the signed schema
3 world manifest define the game.

## Protocol v3

Protocol v3 uses permissionless activation with a self-issued PLAYER_ID.
A player deposits exactly 330 sats, issues one uncontrolled marker in that same
transaction, and sends both into an owner-specific recursive player contract.
There is no allocator, invitation, protocol registry, player cap, or extra transaction.

One genesis transaction creates three uncontrolled, fixed-supply assets:

| Genesis group | Asset | Supply | Initial allocation |
| --- | --- | ---: | --- |
| 0 | TREE | 420 | one per tree |
| 1 | LOG | 21,000,000 | 50,000 in each tree-local reserve |
| 2 | XP | 21,000,000 | 50,000 in each tree-local reserve |

Genesis metadata commits `game=woodland.sh`, `protocol=3`,
`ruleset=woodland.sh/forest/v3`, the asset name, and the exact deployer and
rollover signers. All groups have `control_asset=None`; stock arkd rejects
reissuance.

With Arkade dust `D = 330 sats`:

```text
player state: D sats + 1 PLAYER_ID + player roll + luck credit
              + optional LOG + optional XP
initial tree: 1 TREE + 50,000 LOG + 50,000 XP + health 10
              + fixed D (330 sats)
```

XP has one canonical representation: the soulbound XP asset balance held by
the recursive player state. A successful swing transfers one LOG and one XP
from the selected tree into player state. A miss transfers nothing. Total LOG
and XP remain 21,000,000 across trees and players.

Base LOG chance is 20%, rising by two percentage points at levels 10, 20, 30,
40, and 50 of the reachable `woodland-xp-v1` curve
(1,154 / 4,470 / 13,363 / 37,224 / 101,333 XP) to a 30% cap. Reward entropy
belongs to the player lineage, not the selected tree, so changing targets
cannot search for a better next outcome. A bounded luck-credit accumulator
keeps realized rewards within two LOG of accumulated expected value, permits
at most two consecutive successes, and limits the base rate to ten consecutive
misses (at least one reward per eleven swings). The roll remains public and
predictable; this is variance control, not hidden randomness. XP is soulbound.
LOG is liquid and the protocol can withdraw it to any destination. The alpha
browser intentionally leaves withdrawal to marketplaces and custom clients.

## Atomic Swing

Every swing spends and recreates exactly two gameplay VTXOs:

```text
inputs:  player state, selected tree
outputs: player state, selected tree, merged extension, canonical anchor
groups:  PLAYER_ID, TREE, LOG, XP
```

Group zero moves the player's exact one-unit PLAYER_ID from player input zero to
player output zero. Groups one through three are the world's TREE, LOG, and XP.
Together, the reciprocal Arkade covenants enforce the two-input/four-output
shape, scripts, values, packet continuity, group order, empty transfer
metadata/control fields, and anchor. Both covenants additionally enforce:

- exactly one TREE marker;
- player roll advancement `Rnext = SHA256(Rprevious)`;
- input-XP-asset-dependent 20–30% threshold and bounded luck-credit transition;
- health, LOG, and XP deltas equal to the same reward bit.

The player tapleaf requires player, Arkade operator, and script-tweaked emulator
signatures. The shared tree tapleaf requires operator and tweaked emulator.

## Stumps and One-Batch Regrowth

Each tree owns its entire 50,000 LOG and 50,000 XP reserve. There is no shared
supply vault and no restock path. Ten successful drops reduce health from ten
to zero while moving exactly ten LOG and ten XP into player state.

A funded stump regrows to health ten in one fresh tree-renewal batch. Its TREE
marker, coordinate, script, sats, and remaining local LOG/XP reserve are
preserved exactly. Any caller may submit this covenant path; no player key,
timer, block-height witness, or project-held lifecycle key participates. A
terminal stump whose local reserve has reached zero cannot regrow or draw
supply from another tree.

## Activation and Renewal

Activation is permissionless and browser-owned:

1. derive the player's ordinary Arkade address;
2. receive one exact 330-sat VTXO from any Arkade wallet;
3. issue one uncontrolled `PLAYER_ID` into the personalized player state while
   attaching a roll derived from the player contract's P2TR witness program and
   8,000 luck credit;
4. derive its AssetId from the activation txid and persist that exact ID before
   submission;
5. index only state carrying that one-unit marker.

For the direct issuance-to-state transaction above, every player covenant leaf
recognizes the first recursive spend because the PLAYER_ID issuance txid equals
the state input txid. It requires the script-derived roll and 8,000 credit, so
renewal or withdrawal cannot launder a malformed direct activation.

The covenant cannot inspect ancestry from before a marker entered player state.
A malicious owner can route a newly issued marker through an unconstrained
intermediate output and choose any valid starting roll and credit in the
0–20,000 corridor. After entry, every swing still enforces the exact rate
budget, corridor, streak bounds, and hash transition. This is part of the
permissionless identity-grinding boundary, not a route to tree-target or supply
inflation. The reference activation never creates that shape.

Anyone can issue a different marker or copy the public player script, but cannot
reproduce the selected AssetId without reproducing its transaction ID. Decoy
states therefore do not block discovery of the selected recursive lineage.

Player state has two exact-state batch-renewal paths:

- an owner-authorized path used directly by the browser;
- an optional rollover-signer path for an unattended watchtower.

Every renewal carries PLAYER_ID group zero, so zero-XP state still has an Asset
V1 packet. If arkd's current or scheduled intent policy charges a fee, renewal
adds one clean, asset-free wallet VTXO and returns exact same-script change;
state sats and assets remain untouched. The browser registers the intent,
follows Arkade's batch event stream, contributes tree nonces/signatures,
obtains emulator forfeit signatures, and validates the new expiry itself.

Tree lifecycle uses two disjoint leaves. Funded-stump regrowth is permissionless
apart from the Arkade operator and tweaked emulator closure, and may only change
health from zero to ten. Exact-state maintenance handles active trees and
terminal stumps and additionally requires the low-authority rollover signer.
It cannot alter health, assets, P2TR, or sats. The operator `watch` command uses
that rollover key for near-expiry maintenance and submits funded-stump regrowth
immediately; it holds no player keys and proxies no player traffic.

## Reviewer Map

The protocol-critical review surface is intentionally small:

- `src/protocol.rs`: canonical transaction, output, packet, and asset indexes;
- `src/player.rs`: player state encoding, chop and withdrawal covenants, and signer verification;
- `src/tree.rs`: shared chop, funded-stump regrowth, and guarded maintenance covenants and host mirrors;
- `src/world.rs`: mandatory manifest validation and contract reconstruction;
- `src/renewal.rs`: exact-self-send intent construction and approval checks;
- `src/asset_packet.rs`: strict indexed-asset reconstruction;
- `src/server.rs`: authenticated social API and verified public-state refresh;
- `src/watchtower.rs`: covenant-constrained delegated player renewal.

`src/web_app.rs`, `src/batch.rs`, and `src/arkade.rs` are reference-client and
transport code. Contract tests sit at the end of their corresponding source
files so the production invariants read first.

The core deliberately uses concrete structs and functions rather than
project-owned “trait-first” abstractions. There is one implementation of each
covenant and service client; keeping asset IDs, signer sets, and transaction
shapes explicit is easier to audit. Existing polymorphism is limited to real
boundaries such as `Secp256k1<C: Verification>` and signing closures. Add a new
trait only when a second implementation or substitutable external boundary
actually exists.

## Network Architecture

The signed manifest pins the Arkade and emulator service URLs, service signer
keys, deployer signer, rollover signer, ruleset, assets, and covenant scripts.
Clients verify its BIP340 deployer signature before using any URL. It also pins
the Arkade forfeit key and address: renewal clients recompute every batch tree
sweep leaf from the pinned forfeit key, sign forfeits only to the pinned
address, and require the renewed output to be the byte-exact covenant state
self-send. A spoofed or redirected Arkade endpoint therefore cannot substitute
its own sweep key or forfeit payout. Gameplay calls those services directly.
The manifest emulator URL points at the pinned stock emulator. The default
deployment serves the static bundle, leaderboard, presence, chat, and
delegation API from one Axum origin; the server receives no deployer, rollover,
or player secret.

```text
browser ──same origin──> Axum server: static app + social API
        ──direct───────> Arkade service
        ──direct───────> stock Arkade Script emulator

renewal watcher ──> active-tree renewal and funded-stump regrowth
```

GitHub Pages remains an optional separate static origin. In that mode the
browser still calls the external Axum origin configured at build time.

`scripts/build-web.sh` generates a deploy-ready `dist/` directory with a
manifest-specific CSP meta policy, `.nojekyll`, and an explicit `404.html`.
GitHub Pages cannot supply the stronger response headers a configurable CDN can;
put a proxy in front later if COOP, frame, or Permissions Policy headers become
release requirements.

## Game Server

`woodland-server` serves `dist/` and every `/v1/*` route on one port when
`WOODLAND_SERVER_WEB_ROOT` is configured. Build the bundle with
`WOODLAND_SERVER_URL=self`; the browser then derives the API URL from
`location.origin`. After activation, it automatically registers the PLAYER_ID
with a BIP340 signature bound to that exact origin. Every location, chat, and
delegation update has a separate signed action, timestamp, and payload hash. The
player key never leaves WASM.

The server independently verifies:

- the one-unit uncontrolled PLAYER_ID supply and canonical owner metadata;
- the exact live player contract, marker, indexed creating transaction, and
  recursive lineage;
- progression from the XP asset held by that same state.

The browser polls viewport-bounded `/v1/presence` once per second, bounded chat
every two seconds, and a paginated leaderboard every 15 seconds. Presence is
indexed in 32×32 map chunks, expires after 60 seconds, and is capped per response.
Chat accepts one signed line of at most 280 characters every two seconds and
retains only the newest 200 messages in memory.

Players can separately enable or revoke delegated renewal. When configured with
the manifest's rollover key, the server renews opted-in state near expiry through
the covenant's exact-self-send watchtower leaf. That key cannot transfer or
alter player state. If the server becomes unreachable, an online browser falls
back to its owner-authorized renewal path.

Registration persists; presence and chat do not survive a server restart.
Registration has no self-service deletion endpoint, so clearing browser storage
does not remove the server record. Aggregation adds linkability and the server
or reverse proxy may log IP addresses. It provides neither unique-human nor
Sybil guarantees. Gameplay remains on the direct Arkade path when the server is
unavailable.

## Run Locally

Requirements: Rust 1.88+, `wasm-pack`, Clang with a wasm32 target, Node.js 18+,
Docker Compose, Firefox, and `geckodriver` for browser tests.

```bash
./scripts/regtest.sh clean --force
./scripts/run-web.sh
```

Open `http://127.0.0.1:8000/`. The app and social API use that same origin.
Deposit the displayed 330 sats from any Arkade wallet, press **Refresh**, then
**Create player**. Click an empty tile to walk or a tree to approach and chop.
Map movement remains locked until player activation succeeds. Interactive
chopping refreshes covenant state for each swing and starts swing animations on
a one-second cadence.

The map is a bounded Canvas 2D viewport. It draws only visible tiles, sparse
trees, and nearby player clusters; the player remains a separate fixed overlay
while the canvas camera moves underneath. Player details and social UI are
collapsible, with level, XP, LOG, and online count kept in the map HUD.

Browser storage uses `woodland.sh:web:v2:*`. **New test wallet** clears the
local key, profile (including PLAYER_ID), pending swing, and position. It does
not delete a durable server registration.

For a public protocol-v3 Mutinynet world, `run-mutinynet.sh` deploys or resumes
the world, builds the same-origin bundle, and runs the renewal watcher and Axum:

```bash
./scripts/run-mutinynet.sh
```

GitHub Pages deployment is available as a separate-static-host alternative.
Set `WOODLAND_PAGES_MANIFEST` to a verified, signed schema-3 manifest and
`WOODLAND_SERVER_URL` to the canonical external Axum origin. With no manifest
variable the Pages jobs stay disabled. Deployment tools write their live
manifest as ignored runtime output; copy a verified public manifest to a
deliberate tracked deployment path before enabling Pages. The Pages artifact
contains no secrets; gameplay still talks directly to the manifest-pinned
Arkade and emulator services.

## Verification

Native protocol tests:

```bash
cargo test --locked --features woodland-app
```

Destructive stock-arkd smoke profile:

```bash
./scripts/test-regtest.sh smoke
```

Full deterministic browser, renewal, adversarial, and multiplayer profile:

```bash
./scripts/test-regtest.sh full
```

Long configurable same-tree contention profile:

```bash
./scripts/test-regtest.sh soak
```

It defaults to 12 players and 30 rounds. See [`TESTING.md`](TESTING.md) for
bounded remote-service settings and report output.

Eight-hour v3 release soak. It rotates fresh worlds through the full,
contention, burst, fanout, reload, renewal, and one-batch regrowth profiles,
with per-cycle manifests and artifacts:

```bash
./scripts/test-overnight.sh
```

Four-hour 24-player burst, multi-tree fanout, browser-reload, regrowth,
and full adversarial matrix:

```bash
./scripts/test-aggressive.sh
```

The stack builds unmodified arkd commit
`c7c3184f5cd416e231023f717489a5b0550960cc` and uses the pinned Arkade Script
emulator image.

## Operator Commands

The native operator manages only shared world lifecycle:

```text
woodland-operator status <manifest>
woodland-operator ensure <manifest>
WOODLAND_RENEWAL_STARTUP=1 woodland-operator renew-once <manifest>
woodland-operator watch <manifest>
woodland-operator renew <manifest> tree <tree-id>
woodland-operator renew <manifest> player <owner-pubkey> <player-asset> # optional watchtower
```

Funded-stump regrowth needs no dedicated project key and closes over the Arkade
operator and tweaked emulator. Exact-state maintenance of active trees and
terminal stumps additionally requires the dedicated rollover signer. The
watcher holds that lower-authority key; the public `regrow` API never needs it.

Player activation and owner renewal are browser operations, not operator APIs.

## Mainnet Option

Mainnet is an explicit, experimental deployment option. It is not a claim that
the reference browser provides production-grade custody. Before funding a world,
independently verify every service URL, signer, version, and fee policy.

Generate the deployer and operations roots offline with `woodland-keygen` and
complete the recovery rehearsal in [`mainnet/README.md`](mainnet/README.md)
before creating a funding address. The hierarchy holds exactly two children:
deployer for signed genesis and rollover for tree maintenance and the optional
player watchtower. Do not generate real mainnet roots on the online deployment
or watcher host.

Create an ignored configuration file such as `.cache/mainnet.env`:

```bash
WOODLAND_NETWORK=bitcoin
WOODLAND_ARKADE_SERVICE_URL=https://arkade.computer
WOODLAND_EMULATOR_URL=https://emulator.example
WOODLAND_EXPECTED_ARKADE_SIGNER="replace-with-verified-xonly-key"
WOODLAND_EXPECTED_ARKADE_VERSION="replace-with-verified-version"
WOODLAND_EXPECTED_EMULATOR_SIGNER="replace-with-verified-xonly-key"
WOODLAND_EXPECTED_EMULATOR_VERSION="replace-with-verified-version"
WOODLAND_DEPLOYER_SECRET="replace-with-dedicated-32-byte-hex-key"
WOODLAND_ROLLOVER_SECRET="replace-with-dedicated-32-byte-hex-key"
WOODLAND_WORLD_MANIFEST="/absolute/path/to/woodland-mainnet.json"
```
The emulator URL must terminate at the independently verified stock Arkade
Script emulator. Complete the emulator-host procedure in
[`mainnet/README.md`](mainnet/README.md) before running `status`.

Deploy from a clean shell:

```bash
set -a
source .cache/mainnet.env
set +a
mkdir -p "$(dirname "$WOODLAND_WORLD_MANIFEST")"

cargo build --release --locked --features woodland-app --bin woodland-operator
export WOODLAND_OPERATOR_BIN="$PWD/target/release/woodland-operator"

"$WOODLAND_OPERATOR_BIN" status "$WOODLAND_WORLD_MANIFEST"
# Fund the address above until its clean spendable VTXOs sum exactly to the
# reported amount. One transfer or several exact aggregate transfers work.
"$WOODLAND_OPERATOR_BIN" ensure "$WOODLAND_WORLD_MANIFEST"

unset WOODLAND_DEPLOYER_SECRET
WOODLAND_WASM_FEATURES=woodland-app ./scripts/build-web.sh

# Run this separately under a process supervisor with only the rollover child;
# never provide either secret to GitHub Pages.
"$WOODLAND_OPERATOR_BIN" watch "$WOODLAND_WORLD_MANIFEST"
```

Commit the public mainnet manifest, set `WOODLAND_PAGES_MANIFEST` to its path,
and, if deployed, set `WOODLAND_SERVER_URL` to the public game-server origin.
Push `main`; the Pages jobs build and deploy only after the full CI/regtest gate
passes. Attach the Pages site to the production custom domain in repository
settings.

The operator fails closed unless endpoints use HTTPS, the service network
matches Bitcoin, 330-sat VTXOs and extensions are supported, and exact
Arkade/emulator signer and version pins are present. It evaluates current and
scheduled intent fee policies; nonzero renewal fees require a clean asset-free
VTXO controlled by the renewal participant and return same-contract change.
The static bundle builder independently rejects non-HTTPS mainnet service URLs
and generates a manifest-specific CSP.

The reference browser stores its signing key and PLAYER_ID profile in
`localStorage`; production custody should replace both with a hardened wallet
integration. Arkade Script is evaluated by the pinned stock emulator, not
Bitcoin consensus. Bitcoin Taproot still enforces every signer closure.

Protocol v3 does not claim hidden randomness, covenant-enforced movement,
unique humans, Sybil resistance, pre-covenant PLAYER_ID ancestry, a canonical
leaderboard, or service availability.

## Security and License

Report vulnerabilities privately to
[`stutxo@proton.me`](mailto:stutxo@proton.me). See
[`SECURITY.md`](SECURITY.md) for scope, coordinated disclosure guidance, and the
PGP fingerprint. The project is available under the [MIT License](LICENSE).

## Protocol Documents

- [HOW_IT_WORKS.md](HOW_IT_WORKS.md): transaction walkthrough
- [CLIENT.md](CLIENT.md): alternate client integration
- [PLAYER.md](PLAYER.md): recursive player state and self-renewal
- [TREE.md](TREE.md): tree covenant and one-batch stump regrowth
- [SCALING.md](SCALING.md): contention and capacity analysis
- [TESTING.md](TESTING.md): verification strategy
- [mainnet/README.md](mainnet/README.md): offline key ceremony, recovery, deployment, and watcher operation
