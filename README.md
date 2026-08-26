# woodland.sh

woodland.sh is an Arkade woodcutting protocol with recursive player and tree
state. Ten trees use one shared covenant template but occupy independent VTXOs,
so unrelated players and trees do not share a mutable input.

The reference browser is a client, not an authority. Arkade Script, fixed Asset
V1 supplies, indexed lineage, and the schema 1 world manifest define the game.

## Protocol v1

Protocol v1 uses permissionless activation with a self-issued PLAYER_ID.
A player deposits exactly 330 sats, issues one uncontrolled marker in that same
transaction, and sends both into an owner-specific recursive player contract.
There is no allocator, invitation, protocol registry, player cap, or extra transaction.

One genesis transaction creates three uncontrolled, fixed-supply assets:

| Genesis group | Asset | Supply | Initial allocation |
| --- | --- | ---: | --- |
| 0 | TREE | 10 | one per tree |
| 1 | LOG | 100 | ten per tree |
| 2 | XP | 100 | ten per tree |

Genesis metadata commits `game=woodland.sh`, `protocol=1`, and the asset name.
All groups have `control_asset=None`; stock arkd rejects reissuance.

With Arkade dust `D = 330 sats`:

```text
player state: D sats + 1 PLAYER_ID + identity + position + numeric XP packet
              + optional LOG + optional XP asset
initial tree: 1 TREE + 10 LOG + 10 XP + health 5 + fixed 6D (1,980 sats)
```

Numeric XP is not independently forgeable: it must equal the XP held by
the recursive player state. A successful swing transfers one LOG and one
XP from the selected tree into player state and increments XP by one. A
miss transfers nothing. Total LOG and XP remain 100.
The current 100-XP world cannot reach the level-10 chance bonus; higher
probability tiers are forward-compatible policy, not active Season 1 balance.

## Atomic Swing

Every swing spends and recreates exactly two gameplay VTXOs:

```text
inputs:  player state, selected tree
outputs: player state, selected tree, merged extension, canonical anchor
groups:  PLAYER_ID, TREE, LOG, XP
```

Group zero moves the player's exact one-unit PLAYER_ID from player input zero to
player output zero. Groups one through three are the world's TREE, LOG, and
XP. Together, the reciprocal Arkade covenants enforce the
two-input/four-output shape, scripts, values, packet continuity, group order,
empty transfer metadata/control fields, and anchor. The tree covenant
additionally enforces:

- exactly one TREE marker;
- `Rnext = SHA256(Rprevious)`;
- the public XP-dependent drop threshold;
- health, LOG, and XP deltas equal to the same drop bit;
- player XP equals player-held XP before and after the swing.

The player tapleaf requires player, Arkade operator, and script-tweaked emulator
signatures. The shared tree tapleaf requires operator and tweaked emulator.

## Activation and Renewal

Activation is permissionless and browser-owned:

1. derive the player's ordinary Arkade address;
2. receive one exact 330-sat VTXO from any Arkade wallet;
3. issue one uncontrolled `PLAYER_ID` into the personalized player state while
   attaching identity, position, and zero XP;
4. derive its AssetId from the activation txid and persist that exact ID before
   submission;
5. index only state carrying that one-unit marker.

Anyone can issue a different marker or copy the public player script, but cannot
reproduce the selected AssetId without reproducing its transaction ID. Decoy
states therefore do not block discovery of the real recursive lineage.

Player state has two exact-self-send batch-renewal paths:

- an owner-authorized path used directly by the browser;
- an optional rollover-signer path for an unattended watchtower.

Every renewal carries PLAYER_ID group zero, so zero-XP state still has an Asset
V1 packet. The owner path means a player does not need a Woodland API to remain
live. The browser registers the intent, follows Arkade's batch event stream,
contributes tree nonces/signatures, obtains emulator forfeit signatures, and
validates the new expiry itself.

Trees still need independent maintenance services for timed regrowth and
near-expiry rollover. Those services do not hold player keys or proxy player
traffic.

## Reviewer Map

The protocol-critical review surface is intentionally small:

- `src/protocol.rs`: canonical transaction, output, packet, and asset indexes;
- `src/player.rs`: player state encoding, chop covenant, and signer verification;
- `src/tree.rs`: shared chop, regrowth, renewal covenants, and host mirrors;
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

The manifest pins the Arkade and emulator service URLs and signer keys. Gameplay
calls those services directly. The default deployment serves the static bundle,
leaderboard, presence, chat, and delegation API from one Axum origin; the
server receives no deployer, maintenance, or player secret.

```text
browser ──same origin──> Axum server: static app + social API
        ──direct───────> Arkade service
        ──direct───────> Arkade emulator

maintenance host ──> tree regrowth and tree rollover
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
`location.origin`. **Join server** creates a BIP340 consent signature bound to
that exact origin. Every location, chat, and delegation update has a separate
signed action, timestamp, and payload hash. The player key never leaves WASM.

The server independently verifies:

- the one-unit uncontrolled PLAYER_ID supply and canonical owner metadata;
- the exact live player script, marker, indexed creating transaction, and state
  identity;
- numeric XP against the XP asset held by the same state.

It publishes the verified leaderboard and a two-second combined social snapshot.
Claimed map locations expire after 60 seconds and are not covenant-enforced
movement. Chat accepts one signed line of at most 280 characters every two
seconds and retains only the newest 200 messages in memory.

Players can separately enable or revoke delegated renewal. When configured with
the manifest's rollover key, the server renews opted-in state near expiry through
the covenant's exact-self-send watchtower leaf. That key cannot transfer or
alter player state. If the server becomes unreachable, an online browser falls
back to its owner-authorized renewal path.

Registration and delegation choices persist; presence and chat do not survive a
server restart. Registration has no self-service deletion endpoint, so clearing
browser storage does not remove the server record. Aggregation adds linkability
and the server or reverse proxy may log IP addresses. It provides neither
unique-human nor Sybil guarantees. Gameplay remains on the direct Arkade path
when the server is unavailable.

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

The map is always a bounded camera viewport. The player is a separate fixed
overlay at its center; every movement step translates the map layer underneath.
This remains true at world edges and after orientation changes.

Browser storage uses `woodland.sh:web:v1:*`. **New test wallet** clears the
local key, profile (including PLAYER_ID), pending swing, position, and local
server preference. It does not delete a durable server registration.

For the public Mutinynet world, `run-mutinynet.sh` deploys or resumes the world,
builds the same-origin bundle, and runs both maintenance and Axum:

```bash
./scripts/run-mutinynet.sh
```

GitHub Pages deployment is available as a separate-static-host alternative. Set
`WOODLAND_PAGES_MANIFEST` to the tracked manifest and `WOODLAND_SERVER_URL` to
the canonical external Axum origin. The Pages artifact contains no secrets;
gameplay still talks directly to the manifest-pinned Arkade and emulator
services.

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

The stack builds unmodified arkd commit
`8b34e352859595cc03ba22ffa35088ab88b87fd9` and uses the pinned Arkade Script
emulator image.

## Operator Commands

The native operator manages only shared world lifecycle:

```text
woodland-operator status <manifest>
woodland-operator ensure <manifest>
woodland-operator regrow-due <manifest>
woodland-operator maintain-once <manifest>
woodland-operator watch <manifest>
woodland-operator renew <manifest> tree <tree-id>
woodland-operator renew <manifest> player <owner-pubkey> <player-asset> # optional watchtower
```

Player activation and owner renewal are browser operations, not operator APIs.

## Mainnet Option

Mainnet is an explicit, experimental deployment option. It is not a claim that
the reference browser provides production-grade custody. Before funding a world,
independently verify every service URL, signer, version, and fee policy.

Generate the deployer and operations roots offline with `woodland-keygen` and
complete the recovery rehearsal in [`mainnet/README.md`](mainnet/README.md)
before creating a funding address. Do not generate real mainnet roots on the
online deployment or maintenance host.

Create an ignored configuration file such as `.cache/mainnet.env`:

```bash
WOODLAND_NETWORK=bitcoin
WOODLAND_ARKADE_SERVICE_URL=https://arkade.computer
WOODLAND_EMULATOR_URL=https://emulator.arkade.computer
WOODLAND_EXPECTED_ARKADE_SIGNER="replace-with-verified-xonly-key"
WOODLAND_EXPECTED_ARKADE_VERSION="replace-with-verified-version"
WOODLAND_EXPECTED_EMULATOR_SIGNER="replace-with-verified-xonly-key"
WOODLAND_EXPECTED_EMULATOR_VERSION="replace-with-verified-version"
WOODLAND_DEPLOYER_SECRET="replace-with-dedicated-32-byte-hex-key"
WOODLAND_TREE_MAINTENANCE_SECRET="replace-with-dedicated-32-byte-hex-key"
WOODLAND_ROLLOVER_SECRET="replace-with-dedicated-32-byte-hex-key"
WOODLAND_WORLD_MANIFEST="/absolute/path/to/woodland-mainnet.json"
```

Deploy from a clean shell:

```bash
set -a
source .cache/mainnet.env
set +a
mkdir -p "$(dirname "$WOODLAND_WORLD_MANIFEST")"

cargo build --release --locked --features woodland-app --bin woodland-operator
export WOODLAND_OPERATOR_BIN="$PWD/target/release/woodland-operator"

"$WOODLAND_OPERATOR_BIN" status "$WOODLAND_WORLD_MANIFEST"
# Fund the exact address and amount reported above using a compatible
# mainnet Arkade wallet, then:
"$WOODLAND_OPERATOR_BIN" ensure "$WOODLAND_WORLD_MANIFEST"

unset WOODLAND_DEPLOYER_SECRET
WOODLAND_WASM_FEATURES=woodland-app ./scripts/build-web.sh

# Run this separately under a process supervisor; never provide these secrets
# to GitHub Pages.
"$WOODLAND_OPERATOR_BIN" watch "$WOODLAND_WORLD_MANIFEST"
```

Commit the public mainnet manifest, set `WOODLAND_PAGES_MANIFEST` to its path,
and, if deployed, set `WOODLAND_SERVER_URL` to the public game-server origin.
Push `main`; the Pages jobs build and deploy only after the full CI/regtest gate
passes. Attach the Pages site to the production custom domain in repository
settings.

The operator fails closed unless endpoints use HTTPS, the service network
matches Bitcoin, 330-sat VTXOs and extensions are supported, current and
scheduled offchain fees are zero, and exact Arkade/emulator signer and version
pins are present. The static bundle builder independently rejects non-HTTPS
mainnet service URLs and generates a manifest-specific CSP.

The reference browser stores its signing key and PLAYER_ID profile in
`localStorage`; production custody should replace both with a hardened wallet
integration. Arkade Script is evaluated by the emulator, not Bitcoin consensus.
Bitcoin Taproot still enforces every signer closure: bypassing covenant
evaluation requires a compromised emulator plus every other signer required by
the selected leaf.

Protocol v1 does not claim hidden randomness, covenant-enforced movement,
unique humans, Sybil resistance, a canonical leaderboard, or service
availability.

## Security and License

Report vulnerabilities privately to
[`stutxo@proton.me`](mailto:stutxo@proton.me). See
[`SECURITY.md`](SECURITY.md) for scope, coordinated disclosure guidance, and the
PGP fingerprint. The project is available under the [MIT License](LICENSE).

## Protocol Documents

- [HOW_IT_WORKS.md](HOW_IT_WORKS.md): transaction walkthrough
- [CLIENT.md](CLIENT.md): alternate client integration
- [PLAYER.md](PLAYER.md): recursive player state and self-renewal
- [TREE.md](TREE.md): tree covenant, regrowth, and rollover
- [SCALING.md](SCALING.md): contention and capacity analysis
- [TESTING.md](TESTING.md): verification strategy
- [mainnet/README.md](mainnet/README.md): offline key ceremony, recovery, deployment, and maintenance
