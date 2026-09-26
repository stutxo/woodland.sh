//! Authenticated social server, verified leaderboard, and optional player
//! watchtower for woodland.sh.
//!
//! The public HTTP process persists signed player registration and delegation.
//! Locations and bounded chat are ephemeral. Scores and renewals are always
//! reconstructed from live Arkade state.

use crate::arkade::{now_unix, ArkadeRest, EmulatorParams, EmulatorRest, ServerParams, VtxoRecord};
use crate::keys::Keys;
use crate::player::{self, PlayerContract};
use crate::watchtower::{self, WatchtowerServices};
use crate::world::{ValidatedWorld, WorldManifest, GAME_ID, PROTOCOL_VERSION};
use anyhow::{anyhow, bail, Context, Result};
use ark_core::asset::AssetId;
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{
    header::{CACHE_CONTROL, CONTENT_TYPE},
    HeaderValue, Method, StatusCode,
};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bitcoin::secp256k1::{schnorr, Secp256k1, XOnlyPublicKey};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

const REGISTRY_SCHEMA: u32 = 1;
const DEFAULT_BIND: &str = "127.0.0.1:8090";
const DEFAULT_REFRESH_SECS: u64 = 15;
const MAX_REGISTERED_PLAYERS: usize = 10_000;
const VERIFY_CONCURRENCY: usize = 8;
const VERIFY_BATCH_SIZE: usize = 256;
const MAX_CHAT_MESSAGES: usize = 200;
const PRESENCE_TTL_MS: u64 = 60_000;
const PRESENCE_CHUNK_SIZE: u16 = 32;
const MAX_PRESENCE_QUERY_SPAN: u16 = 128;
const MAX_PRESENCE_RESULTS: usize = 2_000;
const DEFAULT_LEADERBOARD_LIMIT: usize = 100;
const MAX_LEADERBOARD_LIMIT: usize = 200;
const ACTION_CLOCK_SKEW_MS: u64 = 300_000;
const LOCATION_INTERVAL_MS: u64 = 500;
const CHAT_INTERVAL_MS: u64 = 2_000;
const MIN_WORKER_FRESHNESS_SECS: i64 = 120;
// A healthy renewal can spend ten minutes joining a batch and another minute
// cleaning up an abandoned intent. Give that work time to finish before an
// online owner takes over, without extending the verification freshness gate.
const MIN_RENEWAL_FRESHNESS_SECS: i64 = 720;

#[derive(Clone)]
struct Verifier {
    rest: ArkadeRest,
    arkade_url: String,
    emulator_rest: EmulatorRest,
    params: ServerParams,
    emulator: EmulatorParams,
    world: Arc<ValidatedWorld>,
    server_url: String,
    map_width: u16,
    map_height: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegisteredPlayer {
    owner: String,
    player_asset: String,
    registration_signature: String,
    registered_at: i64,
    #[serde(default)]
    delegated_renewal: bool,
    #[serde(default)]
    delegation_updated_at_ms: u64,
    #[serde(skip)]
    state: Option<LeaderboardPlayer>,
}

impl RegisteredPlayer {
    fn same_registration(&self, snapshot: &Self) -> bool {
        self.owner == snapshot.owner
            && self.player_asset == snapshot.player_asset
            && self.registration_signature == snapshot.registration_signature
            && self.registered_at == snapshot.registered_at
    }

    /// Async verification may finish after a renewal or another request has
    /// published newer state. Only update the cache that was actually read.
    fn apply_verified_state(
        &mut self,
        snapshot: &Self,
        verified: Option<LeaderboardPlayer>,
        now: i64,
    ) -> bool {
        if !self.same_registration(snapshot) || self.state != snapshot.state {
            return false;
        }
        if let Some(player) = verified {
            self.state = Some(player);
        } else if let Some(player) = &mut self.state {
            player.active = false;
            player.updated_at = now;
        }
        true
    }

    fn renewal_due(&self, now: i64, force: bool) -> bool {
        self.delegated_renewal
            && self.state.as_ref().is_some_and(|player| {
                player.active
                    && (force
                        || player.expires_at.is_some_and(|expires_at| {
                            let remaining = expires_at - now;
                            remaining > 0 && remaining < player.rollover_margin_seconds
                        }))
            })
    }

    fn queued_renewal_authorized(&self, queued: &Self, now: i64, force: bool) -> bool {
        self.same_registration(queued)
            && self.delegation_updated_at_ms == queued.delegation_updated_at_ms
            && self.renewal_due(now, force)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegistryFile {
    schema_version: u32,
    players: BTreeMap<String, RegisteredPlayer>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA,
            players: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaderboardPlayer {
    pub owner: String,
    pub player_asset: String,
    pub xp: u64,
    pub level: u64,
    pub logs: u64,
    pub state_outpoint: String,
    pub expires_at: Option<i64>,
    pub rollover_margin_seconds: i64,
    pub active: bool,
    pub registered_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlayerLocation {
    player_asset: String,
    x: u16,
    y: u16,
    updated_at_ms: u64,
}

#[derive(Default)]
struct LocationIndex {
    by_player: BTreeMap<String, PlayerLocation>,
    chunks: BTreeMap<(u16, u16), BTreeSet<String>>,
}

impl LocationIndex {
    fn chunk(x: u16, y: u16) -> (u16, u16) {
        (x / PRESENCE_CHUNK_SIZE, y / PRESENCE_CHUNK_SIZE)
    }

    fn remove(&mut self, player_asset: &str) {
        let Some(previous) = self.by_player.remove(player_asset) else {
            return;
        };
        let chunk = Self::chunk(previous.x, previous.y);
        if let Some(players) = self.chunks.get_mut(&chunk) {
            players.remove(player_asset);
            if players.is_empty() {
                self.chunks.remove(&chunk);
            }
        }
    }

    fn upsert(&mut self, location: PlayerLocation) {
        let player_asset = location.player_asset.clone();
        self.remove(&player_asset);
        self.chunks
            .entry(Self::chunk(location.x, location.y))
            .or_default()
            .insert(player_asset.clone());
        self.by_player.insert(player_asset, location);
    }

    fn retain_active(&mut self, active: &BTreeSet<String>, cutoff: u64) {
        let stale = self
            .by_player
            .iter()
            .filter(|(player_asset, location)| {
                !active.contains(*player_asset) || location.updated_at_ms < cutoff
            })
            .map(|(player_asset, _)| player_asset.clone())
            .collect::<Vec<_>>();
        for player_asset in stale {
            self.remove(&player_asset);
        }
    }

    fn query(&self, query: &PresenceQuery) -> (Vec<PlayerLocation>, bool) {
        let min_chunk = Self::chunk(query.min_x, query.min_y);
        let max_chunk = Self::chunk(query.max_x, query.max_y);
        let mut locations = Vec::new();
        for chunk_y in min_chunk.1..=max_chunk.1 {
            for chunk_x in min_chunk.0..=max_chunk.0 {
                let Some(players) = self.chunks.get(&(chunk_x, chunk_y)) else {
                    continue;
                };
                for player_asset in players {
                    let Some(location) = self.by_player.get(player_asset) else {
                        continue;
                    };
                    if location.x < query.min_x
                        || location.x > query.max_x
                        || location.y < query.min_y
                        || location.y > query.max_y
                    {
                        continue;
                    }
                    if locations.len() == MAX_PRESENCE_RESULTS {
                        return (locations, true);
                    }
                    locations.push(location.clone());
                }
            }
        }
        (locations, false)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    id: u64,
    player_asset: String,
    message: String,
    created_at_ms: u64,
}

struct MultiplayerState {
    locations: LocationIndex,
    chat: VecDeque<ChatMessage>,
    action_timestamps: BTreeMap<String, u64>,
    next_chat_id: u64,
}

impl Default for MultiplayerState {
    fn default() -> Self {
        Self {
            locations: LocationIndex::default(),
            chat: VecDeque::new(),
            action_timestamps: BTreeMap::new(),
            next_chat_id: 1,
        }
    }
}

#[derive(Default)]
struct RefreshStatus {
    last_refresh_at: Option<i64>,
    last_error: Option<String>,
    last_renewal_at: Option<i64>,
    last_renewal_error: Option<String>,
}

impl RefreshStatus {
    fn fresh(timestamp: Option<i64>, now: i64, max_age: i64) -> bool {
        timestamp.is_some_and(|timestamp| (0..=max_age).contains(&now.saturating_sub(timestamp)))
    }

    fn ready(&self, now: i64, max_age: i64, renewal_configured: bool) -> bool {
        self.last_error.is_none()
            && Self::fresh(self.last_refresh_at, now, max_age)
            && (!renewal_configured || self.renewal_available(now, max_age))
    }

    fn renewal_available(&self, now: i64, max_age: i64) -> bool {
        self.last_error.is_none()
            && self.last_renewal_error.is_none()
            && Self::fresh(self.last_refresh_at, now, max_age)
            && Self::fresh(
                self.last_renewal_at,
                now,
                max_age.max(MIN_RENEWAL_FRESHNESS_SECS),
            )
    }
}

struct AppState {
    verifier: Arc<Verifier>,
    rollover_keys: Option<Arc<Keys>>,
    registry: RwLock<RegistryFile>,
    registry_path: PathBuf,
    persist_lock: Mutex<()>,
    refresh_status: RwLock<RefreshStatus>,
    worker_freshness_secs: i64,
    multiplayer: RwLock<MultiplayerState>,
    force_renewal_once: AtomicBool,
    refresh_cursor: AtomicUsize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegisterPlayerRequest {
    owner: String,
    player_asset: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocationRequest {
    owner: String,
    player_asset: String,
    timestamp_ms: u64,
    x: u16,
    y: u16,
    signature: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChatRequest {
    owner: String,
    player_asset: String,
    timestamp_ms: u64,
    message: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DelegationRequest {
    owner: String,
    player_asset: String,
    timestamp_ms: u64,
    enabled: bool,
    signature: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PresenceQuery {
    min_x: u16,
    min_y: u16,
    max_x: u16,
    max_y: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaderboardQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaderboardResponse {
    generated_at: i64,
    total: usize,
    players: Vec<LeaderboardPlayer>,
    delegated_player_assets: Vec<String>,
    delegation_available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresenceResponse {
    generated_at_ms: u64,
    locations: Vec<PlayerLocation>,
    truncated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatResponse {
    generated_at_ms: u64,
    messages: Vec<ChatMessage>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DelegationResponse {
    enabled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    ready: bool,
    registered_players: usize,
    last_refresh_at: Option<i64>,
    last_error: Option<String>,
    last_renewal_at: Option<i64>,
    online_players: usize,
    delegation_available: bool,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }

    fn upstream(error: anyhow::Error) -> Self {
        eprintln!("woodland.sh server verification: {error:#}");
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: "player verification failed".to_owned(),
        }
    }

    fn internal(error: anyhow::Error) -> Self {
        eprintln!("woodland.sh server: {error:#}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "server storage failed".to_owned(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

fn player_contract(verifier: &Verifier, owner: XOnlyPublicKey) -> Result<PlayerContract> {
    player::build_player_contract(
        &Secp256k1::new(),
        owner,
        verifier.params.signer_pk,
        verifier.emulator.signer_pk,
        verifier.world.rollover_signer,
        verifier.params.unilateral_exit_delay,
        verifier.params.network,
        verifier.world.tree_asset,
        verifier.world.log_asset,
        verifier.world.xp_asset,
        verifier.world.stone_asset,
        verifier.world.iron_ore_asset,
        verifier.params.dust_sats,
        &verifier.world.contract.vtxo.script_pubkey(),
    )
}

fn player_asset_metadata(owner: XOnlyPublicKey) -> Vec<u8> {
    let entries = [
        ("game", GAME_ID.to_owned()),
        ("protocol", PROTOCOL_VERSION.to_string()),
        ("asset", "PLAYER_ID".to_owned()),
        ("owner", owner.to_string()),
    ];
    let mut encoded = Vec::new();
    ark_core::extension::encode_uvarint(&mut encoded, entries.len() as u64);
    for (key, value) in entries {
        ark_core::extension::encode_uvarint(&mut encoded, key.len() as u64);
        encoded.extend_from_slice(key.as_bytes());
        ark_core::extension::encode_uvarint(&mut encoded, value.len() as u64);
        encoded.extend_from_slice(value.as_bytes());
    }
    encoded
}

fn select_player_record(
    records: &[VtxoRecord],
    contract: &PlayerContract,
    player_asset: AssetId,
) -> Result<Option<VtxoRecord>> {
    let candidates = records
        .iter()
        .filter(|record| {
            player::validate_player_state_record(record, contract, player_asset).is_ok()
        })
        .cloned()
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [] => Ok(None),
        [record] => Ok(Some(record.clone())),
        _ => Err(anyhow!("PLAYER_ID has multiple live state VTXOs")),
    }
}

impl Verifier {
    fn verify_registration_signature(
        &self,
        owner: XOnlyPublicKey,
        player_asset: AssetId,
        signature: schnorr::Signature,
    ) -> Result<()> {
        let message = player::server_registration_message(
            self.world.genesis_txid,
            owner,
            player_asset,
            &self.server_url,
        );
        Secp256k1::verification_only()
            .verify_schnorr(&signature, &message, &owner)
            .context("registration signature does not authorize this server")
    }

    fn verify_server_action_signature(
        &self,
        owner: XOnlyPublicKey,
        player_asset: AssetId,
        action: &str,
        timestamp_ms: u64,
        payload: &str,
        signature: schnorr::Signature,
    ) -> Result<()> {
        let message = player::server_action_message(
            self.world.genesis_txid,
            owner,
            player_asset,
            &self.server_url,
            action,
            timestamp_ms,
            payload,
        );
        Secp256k1::verification_only()
            .verify_schnorr(&signature, &message, &owner)
            .context("signature does not authorize this server action")
    }

    async fn verify(
        &self,
        owner: XOnlyPublicKey,
        player_asset: AssetId,
        registered_at: i64,
    ) -> Result<Option<LeaderboardPlayer>> {
        if [
            self.world.tree_asset,
            self.world.log_asset,
            self.world.xp_asset,
        ]
        .contains(&player_asset)
        {
            bail!("PLAYER_ID collides with a world asset");
        }
        let details = self
            .rest
            .get_asset_details(player_asset)
            .await
            .context("read PLAYER_ID metadata")?;
        if details.supply != 1
            || details.control_asset.is_some()
            || details.metadata != player_asset_metadata(owner)
        {
            bail!("PLAYER_ID issuance metadata or supply is invalid");
        }

        let contract = player_contract(self, owner)?;
        let records = self
            .rest
            .get_vtxos(
                &contract.vtxo.script_pubkey().to_hex_string(),
                "spendableOnly",
            )
            .await?;
        let Some(record) = select_player_record(&records, &contract, player_asset)? else {
            return Ok(None);
        };
        let previous_tx = self
            .rest
            .get_virtual_txs(&[record.outpoint.txid])
            .await?
            .remove(&record.outpoint.txid)
            .ok_or_else(|| anyhow!("indexer omitted the player creating transaction"))?;
        record.validate_creating_transaction(&previous_tx)?;
        let _state = player::player_state_from_tx(&previous_tx)?
            .ok_or_else(|| anyhow!("player creating transaction has no state packets"))?;
        let xp_balance = record.asset_amount(self.world.xp_asset).unwrap_or(0);
        let woodcutting_xp = player::woodcutting_xp(xp_balance);
        let now = now_unix();
        Ok(Some(LeaderboardPlayer {
            owner: owner.to_string(),
            player_asset: player_asset.to_string(),
            xp: woodcutting_xp,
            level: player::level_from_xp(woodcutting_xp),
            logs: record.asset_amount(self.world.log_asset).unwrap_or(0),
            state_outpoint: record.outpoint.to_string(),
            expires_at: record.expires_at,
            rollover_margin_seconds: record.rollover_margin_seconds(),
            active: record.expires_at.is_none_or(|expires_at| expires_at > now),
            registered_at,
            updated_at: now,
        }))
    }
}

fn load_registry(path: &Path) -> Result<RegistryFile> {
    if !path.exists() {
        return Ok(RegistryFile::default());
    }
    let registry: RegistryFile = serde_json::from_str(
        &std::fs::read_to_string(path)
            .with_context(|| format!("read server registry {}", path.display()))?,
    )
    .with_context(|| format!("parse server registry {}", path.display()))?;
    if registry.schema_version != REGISTRY_SCHEMA {
        bail!(
            "unsupported server registry schema {}",
            registry.schema_version
        );
    }
    if registry.players.len() > MAX_REGISTERED_PLAYERS {
        bail!("server registry exceeds player limit");
    }
    Ok(registry)
}

fn validate_registry_consents(registry: &RegistryFile, verifier: &Verifier) -> Result<()> {
    for (key, entry) in &registry.players {
        let owner = XOnlyPublicKey::from_str(&entry.owner)
            .with_context(|| format!("parse registered owner for {key}"))?;
        let player_asset = AssetId::from_str(&entry.player_asset)
            .with_context(|| format!("parse registered PLAYER_ID for {key}"))?;
        if key != &player_asset.to_string() {
            bail!("server registry key does not match PLAYER_ID");
        }
        let signature = schnorr::Signature::from_str(&entry.registration_signature)
            .with_context(|| format!("parse registration signature for {key}"))?;
        verifier
            .verify_registration_signature(owner, player_asset, signature)
            .with_context(|| format!("verify registration consent for {key}"))?;
    }
    Ok(())
}

fn save_registry(path: &Path, registry: &RegistryFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("server registry path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create server state directory {}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(
        &temporary,
        format!("{}\n", serde_json::to_string_pretty(registry)?),
    )
    .with_context(|| format!("write server registry {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("publish server registry {}", path.display()))
}

async fn persist(state: &AppState) -> Result<()> {
    let _guard = state.persist_lock.lock().await;
    let snapshot = state.registry.read().await.clone();
    save_registry(&state.registry_path, &snapshot)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis()
        .try_into()
        .expect("system time exceeds u64 milliseconds")
}

struct ActionAuthorization<'a> {
    owner: &'a str,
    player_asset: &'a str,
    signature: &'a str,
    timestamp_ms: u64,
    action: &'a str,
    payload: &'a str,
    minimum_interval_ms: u64,
}

async fn authorize_action(
    state: &AppState,
    request: ActionAuthorization<'_>,
) -> Result<(XOnlyPublicKey, AssetId), ApiError> {
    if request.owner.len() > 128
        || request.player_asset.len() > 128
        || request.signature.len() > 128
    {
        return Err(ApiError::bad_request("action fields are too long"));
    }
    let owner = XOnlyPublicKey::from_str(request.owner.trim())
        .map_err(|_| ApiError::bad_request("owner is not an x-only public key"))?;
    let player_asset = AssetId::from_str(request.player_asset.trim())
        .map_err(|_| ApiError::bad_request("playerAsset is invalid"))?;
    let signature = schnorr::Signature::from_str(request.signature.trim())
        .map_err(|_| ApiError::bad_request("signature is not a BIP340 signature"))?;
    state
        .verifier
        .verify_server_action_signature(
            owner,
            player_asset,
            request.action,
            request.timestamp_ms,
            request.payload,
            signature,
        )
        .map_err(|_| ApiError::bad_request("signature does not authorize this server action"))?;
    let key = player_asset.to_string();
    let (snapshot, active) = {
        let registry = state.registry.read().await;
        let entry = registry
            .players
            .get(&key)
            .filter(|entry| entry.owner == owner.to_string())
            .ok_or_else(|| ApiError::not_found("player is not registered with this server"))?;
        let active = entry.state.as_ref().is_some_and(|player| {
            player.active
                && player
                    .expires_at
                    .is_none_or(|expires_at| expires_at > now_unix())
        });
        (entry.clone(), active)
    };
    if !active {
        let verified = state
            .verifier
            .verify(owner, player_asset, snapshot.registered_at)
            .await
            .map_err(ApiError::upstream)?
            .ok_or_else(|| ApiError::not_found("player has no live registered state"))?;
        if let Some(entry) = state.registry.write().await.players.get_mut(&key) {
            entry.apply_verified_state(&snapshot, Some(verified), now_unix());
        }
    }
    if now_ms().abs_diff(request.timestamp_ms) > ACTION_CLOCK_SKEW_MS {
        return Err(ApiError::bad_request(
            "server action timestamp is outside the allowed window",
        ));
    }
    let action_key = format!("{}:{key}", request.action);
    let mut multiplayer = state.multiplayer.write().await;
    if let Some(previous) = multiplayer.action_timestamps.get(&action_key) {
        if request.timestamp_ms <= *previous
            || request.timestamp_ms.saturating_sub(*previous) < request.minimum_interval_ms
        {
            return Err(ApiError::too_many_requests(
                "server action was replayed or sent too quickly",
            ));
        }
    }
    multiplayer
        .action_timestamps
        .insert(action_key, request.timestamp_ms);
    Ok((owner, player_asset))
}

async fn update_location(
    State(state): State<Arc<AppState>>,
    Json(request): Json<LocationRequest>,
) -> Result<Json<PlayerLocation>, ApiError> {
    if request.x >= state.verifier.map_width || request.y >= state.verifier.map_height {
        return Err(ApiError::bad_request(
            "player location is outside the world map",
        ));
    }
    let payload = format!("x={}\ny={}\n", request.x, request.y);
    let (_, player_asset) = authorize_action(
        &state,
        ActionAuthorization {
            owner: &request.owner,
            player_asset: &request.player_asset,
            signature: &request.signature,
            timestamp_ms: request.timestamp_ms,
            action: player::SERVER_ACTION_LOCATION,
            payload: &payload,
            minimum_interval_ms: LOCATION_INTERVAL_MS,
        },
    )
    .await?;
    let location = PlayerLocation {
        player_asset: player_asset.to_string(),
        x: request.x,
        y: request.y,
        updated_at_ms: now_ms(),
    };
    state
        .multiplayer
        .write()
        .await
        .locations
        .upsert(location.clone());
    Ok(Json(location))
}

async fn post_chat(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<Json<ChatMessage>, ApiError> {
    if request.message.trim().is_empty()
        || request.message.chars().count() > 280
        || request.message.contains(['\r', '\n'])
    {
        return Err(ApiError::bad_request(
            "chat message must be one non-empty line of at most 280 characters",
        ));
    }
    let (_, player_asset) = authorize_action(
        &state,
        ActionAuthorization {
            owner: &request.owner,
            player_asset: &request.player_asset,
            signature: &request.signature,
            timestamp_ms: request.timestamp_ms,
            action: player::SERVER_ACTION_CHAT,
            payload: &request.message,
            minimum_interval_ms: CHAT_INTERVAL_MS,
        },
    )
    .await?;
    let mut multiplayer = state.multiplayer.write().await;
    let message = ChatMessage {
        id: multiplayer.next_chat_id,
        player_asset: player_asset.to_string(),
        message: request.message,
        created_at_ms: now_ms(),
    };
    multiplayer.next_chat_id = multiplayer.next_chat_id.wrapping_add(1);
    multiplayer.chat.push_back(message.clone());
    while multiplayer.chat.len() > MAX_CHAT_MESSAGES {
        multiplayer.chat.pop_front();
    }
    Ok(Json(message))
}

async fn set_delegation(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DelegationRequest>,
) -> Result<Json<DelegationResponse>, ApiError> {
    if request.enabled && state.rollover_keys.is_none() {
        return Err(ApiError::unavailable(
            "delegated player renewal is not configured on this server",
        ));
    }
    let payload = format!("enabled={}\n", request.enabled);
    let (_, player_asset) = authorize_action(
        &state,
        ActionAuthorization {
            owner: &request.owner,
            player_asset: &request.player_asset,
            signature: &request.signature,
            timestamp_ms: request.timestamp_ms,
            action: player::SERVER_ACTION_DELEGATION,
            payload: &payload,
            minimum_interval_ms: 0,
        },
    )
    .await?;
    {
        let mut registry = state.registry.write().await;
        let entry = registry
            .players
            .get_mut(&player_asset.to_string())
            .ok_or_else(|| ApiError::not_found("player is not registered"))?;
        if request.timestamp_ms <= entry.delegation_updated_at_ms {
            return Err(ApiError::bad_request("delegation update is stale"));
        }
        entry.delegated_renewal = request.enabled;
        entry.delegation_updated_at_ms = request.timestamp_ms;
    }
    persist(&state).await.map_err(ApiError::internal)?;
    Ok(Json(DelegationResponse {
        enabled: request.enabled,
    }))
}

/// An identical replay of a stored registration is already verified consent,
/// so it is answered from the registry without the upstream lineage verify
/// or a registry rewrite; anything else falls through to the full path.
fn replayed_registration(
    registry: &RegistryFile,
    owner: XOnlyPublicKey,
    player_asset: AssetId,
    signature: schnorr::Signature,
) -> Option<LeaderboardPlayer> {
    let entry = registry.players.get(&player_asset.to_string())?;
    if entry.owner != owner.to_string() || entry.registration_signature != signature.to_string() {
        return None;
    }
    let mut player = entry.state.clone()?;
    if player
        .expires_at
        .is_some_and(|expires_at| expires_at <= now_unix())
    {
        player.active = false;
    }
    Some(player)
}

fn merge_verified_registration(
    registry: &mut RegistryFile,
    snapshot: Option<&RegisteredPlayer>,
    signature: String,
    mut verified: LeaderboardPlayer,
) -> Result<LeaderboardPlayer, ApiError> {
    if let Some(entry) = registry.players.get_mut(&verified.player_asset) {
        if entry.owner != verified.owner {
            return Err(ApiError::bad_request("registered player owner changed"));
        }
        // Preserve concurrent consent updates and the original registration
        // time. A slow registration must not resurrect revoked delegation.
        verified.registered_at = entry.registered_at;
        if let Some(snapshot) = snapshot {
            entry.apply_verified_state(snapshot, Some(verified.clone()), now_unix());
        }
        entry.registration_signature = signature;
        return Ok(entry.state.clone().unwrap_or(verified));
    }
    if registry.players.len() >= MAX_REGISTERED_PLAYERS {
        return Err(ApiError::bad_request("server registration limit reached"));
    }
    registry.players.insert(
        verified.player_asset.clone(),
        RegisteredPlayer {
            owner: verified.owner.clone(),
            player_asset: verified.player_asset.clone(),
            registration_signature: signature,
            delegated_renewal: false,
            delegation_updated_at_ms: 0,
            registered_at: verified.registered_at,
            state: Some(verified.clone()),
        },
    );
    Ok(verified)
}

async fn register_player(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RegisterPlayerRequest>,
) -> Result<Json<LeaderboardPlayer>, ApiError> {
    if request.owner.len() > 128
        || request.player_asset.len() > 128
        || request.signature.len() > 128
    {
        return Err(ApiError::bad_request("registration fields are too long"));
    }
    let owner = XOnlyPublicKey::from_str(request.owner.trim())
        .map_err(|_| ApiError::bad_request("owner is not an x-only public key"))?;
    let player_asset = AssetId::from_str(request.player_asset.trim())
        .map_err(|_| ApiError::bad_request("playerAsset is invalid"))?;
    let signature = schnorr::Signature::from_str(request.signature.trim())
        .map_err(|_| ApiError::bad_request("signature is not a BIP340 signature"))?;
    state
        .verifier
        .verify_registration_signature(owner, player_asset, signature)
        .map_err(|_| {
            ApiError::bad_request("signature does not authorize this server registration")
        })?;
    let key = player_asset.to_string();
    if let Some(player) = replayed_registration(
        &*state.registry.read().await,
        owner,
        player_asset,
        signature,
    ) {
        return Ok(Json(player));
    }
    let snapshot = state.registry.read().await.players.get(&key).cloned();
    let registered_at = snapshot
        .as_ref()
        .map_or_else(now_unix, |entry| entry.registered_at);
    let verified = state
        .verifier
        .verify(owner, player_asset, registered_at)
        .await
        .map_err(ApiError::upstream)?
        .ok_or_else(|| ApiError::not_found("no live state for this owner and PLAYER_ID"))?;

    let verified = {
        let mut registry = state.registry.write().await;
        merge_verified_registration(
            &mut registry,
            snapshot.as_ref(),
            signature.to_string(),
            verified,
        )?
    };
    persist(&state).await.map_err(ApiError::internal)?;
    Ok(Json(verified))
}

fn leaderboard_players(registry: &RegistryFile) -> Vec<LeaderboardPlayer> {
    let now = now_unix();
    let mut players = registry
        .players
        .values()
        .filter_map(|entry| entry.state.clone())
        .collect::<Vec<_>>();
    for player in &mut players {
        if player
            .expires_at
            .is_some_and(|expires_at| expires_at <= now)
        {
            player.active = false;
        }
    }
    players.sort_by(|left, right| {
        right
            .active
            .cmp(&left.active)
            .then_with(|| right.xp.cmp(&left.xp))
            .then_with(|| left.registered_at.cmp(&right.registered_at))
            .then_with(|| left.player_asset.cmp(&right.player_asset))
    });
    players
}

async fn leaderboard(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LeaderboardQuery>,
) -> Json<LeaderboardResponse> {
    let registry = state.registry.read().await;
    let players = leaderboard_players(&registry);
    let total = players.len();
    let offset = query.offset.unwrap_or(0).min(total);
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LEADERBOARD_LIMIT)
        .clamp(1, MAX_LEADERBOARD_LIMIT);
    let players = players.into_iter().skip(offset).take(limit).collect();
    let delegated_player_assets = registry
        .players
        .values()
        .filter(|entry| entry.delegated_renewal)
        .map(|entry| entry.player_asset.clone())
        .collect();
    let delegation_available = state.rollover_keys.is_some()
        && state
            .refresh_status
            .read()
            .await
            .renewal_available(now_unix(), state.worker_freshness_secs);
    Json(LeaderboardResponse {
        generated_at: now_unix(),
        total,
        players,
        delegated_player_assets,
        delegation_available,
    })
}

async fn presence(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PresenceQuery>,
) -> Result<Json<PresenceResponse>, ApiError> {
    if query.min_x > query.max_x
        || query.min_y > query.max_y
        || query.max_x >= state.verifier.map_width
        || query.max_y >= state.verifier.map_height
        || query.max_x - query.min_x + 1 > MAX_PRESENCE_QUERY_SPAN
        || query.max_y - query.min_y + 1 > MAX_PRESENCE_QUERY_SPAN
    {
        return Err(ApiError::bad_request(
            "presence viewport is invalid or too large",
        ));
    }
    let current_time = now_ms();
    let active = {
        let registry = state.registry.read().await;
        leaderboard_players(&registry)
            .into_iter()
            .filter(|player| player.active)
            .map(|player| player.player_asset)
            .collect::<BTreeSet<_>>()
    };
    let (locations, truncated) = {
        let mut multiplayer = state.multiplayer.write().await;
        multiplayer
            .locations
            .retain_active(&active, current_time.saturating_sub(PRESENCE_TTL_MS));
        multiplayer.locations.query(&query)
    };
    Ok(Json(PresenceResponse {
        generated_at_ms: current_time,
        locations,
        truncated,
    }))
}

async fn chat(State(state): State<Arc<AppState>>) -> Json<ChatResponse> {
    Json(ChatResponse {
        generated_at_ms: now_ms(),
        messages: state
            .multiplayer
            .read()
            .await
            .chat
            .iter()
            .cloned()
            .collect(),
    })
}

async fn health(State(state): State<Arc<AppState>>) -> (StatusCode, Json<HealthResponse>) {
    let registry = state.registry.read().await;
    let status = state.refresh_status.read().await;
    let cutoff = now_ms().saturating_sub(PRESENCE_TTL_MS);
    let online_players = state
        .multiplayer
        .read()
        .await
        .locations
        .by_player
        .values()
        .filter(|location| location.updated_at_ms >= cutoff)
        .count();
    let now = now_unix();
    let ready = status.ready(
        now,
        state.worker_freshness_secs,
        state.rollover_keys.is_some(),
    );
    let last_error = status
        .last_error
        .clone()
        .or_else(|| status.last_renewal_error.clone())
        .or_else(|| {
            (!ready).then(|| {
                "server verification or renewal worker has no recent successful progress".to_owned()
            })
        });
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(HealthResponse {
            ready,
            registered_players: registry.players.len(),
            last_refresh_at: status.last_refresh_at,
            last_error,
            last_renewal_at: status.last_renewal_at,
            online_players,
            delegation_available: state.rollover_keys.is_some()
                && status.renewal_available(now, state.worker_freshness_secs),
        }),
    )
}

async fn refresh_batch(state: Arc<AppState>) -> Result<()> {
    let all_registrations = state
        .registry
        .read()
        .await
        .players
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let mut selected = BTreeMap::new();
    if !all_registrations.is_empty() {
        let batch_len = VERIFY_BATCH_SIZE.min(all_registrations.len());
        let start =
            state.refresh_cursor.fetch_add(batch_len, Ordering::Relaxed) % all_registrations.len();
        for offset in 0..batch_len {
            let entry = all_registrations[(start + offset) % all_registrations.len()].clone();
            selected.insert(entry.player_asset.clone(), entry);
        }
        for entry in all_registrations
            .into_iter()
            .filter(|entry| entry.delegated_renewal)
        {
            selected.insert(entry.player_asset.clone(), entry);
        }
    }
    let registrations = selected.into_values().collect::<Vec<_>>();
    let verifier = state.verifier.clone();
    let results = stream::iter(registrations)
        .map(|entry| {
            let verifier = verifier.clone();
            async move {
                let owner =
                    XOnlyPublicKey::from_str(&entry.owner).context("parse registered owner")?;
                let player_asset =
                    AssetId::from_str(&entry.player_asset).context("parse registered PLAYER_ID")?;
                let result = verifier
                    .verify(owner, player_asset, entry.registered_at)
                    .await;
                Ok::<_, anyhow::Error>((entry, result))
            }
        })
        .buffer_unordered(VERIFY_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let now = now_unix();
    let mut first_error = None;
    {
        let mut registry = state.registry.write().await;
        for result in results {
            match result {
                Ok((snapshot, Ok(verified))) => {
                    if let Some(entry) = registry.players.get_mut(&snapshot.player_asset) {
                        entry.apply_verified_state(&snapshot, verified, now);
                    }
                }
                Ok((snapshot, Err(error))) => {
                    first_error
                        .get_or_insert_with(|| format!("{}: {error:#}", snapshot.player_asset));
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| format!("{error:#}"));
                }
            }
        }
    }
    let mut status = state.refresh_status.write().await;
    status.last_refresh_at = Some(now);
    status.last_error = first_error;
    Ok(())
}

async fn renew_delegated_players(state: Arc<AppState>) -> Result<()> {
    let Some(rollover_keys) = state.rollover_keys.clone() else {
        return Ok(());
    };
    let now = now_unix();
    let force_requested = state.force_renewal_once.load(Ordering::Relaxed);
    let due = state
        .registry
        .read()
        .await
        .players
        .values()
        .filter(|entry| entry.renewal_due(now, force_requested))
        .cloned()
        .collect::<Vec<_>>();
    if due.is_empty() {
        return Ok(());
    }
    let force = force_requested && state.force_renewal_once.swap(false, Ordering::Relaxed);
    let mut first_error = None;
    let mut changed = false;
    for queued in due {
        // Another player's batch can take minutes. Honor revocation and newer
        // state before starting queued work; an already submitted intent is
        // allowed to finish its exact-state renewal.
        let still_authorized = state
            .registry
            .read()
            .await
            .players
            .get(&queued.player_asset)
            .is_some_and(|entry| entry.queued_renewal_authorized(&queued, now_unix(), force));
        if !still_authorized {
            continue;
        }
        let owner = XOnlyPublicKey::from_str(&queued.owner).context("parse delegated owner")?;
        let player_asset =
            AssetId::from_str(&queued.player_asset).context("parse delegated PLAYER_ID")?;
        let result = watchtower::renew_player(
            &rollover_keys,
            WatchtowerServices {
                arkade_url: &state.verifier.arkade_url,
                rest: &state.verifier.rest,
                emulator_rest: &state.verifier.emulator_rest,
                params: &state.verifier.params,
                emulator: &state.verifier.emulator,
            },
            &state.verifier.world,
            owner,
            player_asset,
            force,
        )
        .await;
        match result {
            Ok(outcome) => {
                eprintln!(
                    "woodland.sh server renewed player {}: {} -> {}",
                    player_asset, outcome.old_outpoint, outcome.new_outpoint
                );
                let snapshot = state
                    .registry
                    .read()
                    .await
                    .players
                    .get(&queued.player_asset)
                    .cloned();
                match state
                    .verifier
                    .verify(owner, player_asset, queued.registered_at)
                    .await
                {
                    Ok(Some(player)) => {
                        if let (Some(snapshot), Some(entry)) = (
                            snapshot.as_ref(),
                            state
                                .registry
                                .write()
                                .await
                                .players
                                .get_mut(&queued.player_asset),
                        ) {
                            changed |=
                                entry.apply_verified_state(snapshot, Some(player), now_unix());
                        }
                    }
                    Ok(None) => {
                        first_error.get_or_insert_with(|| {
                            format!("{player_asset}: renewed state was not indexed")
                        });
                    }
                    Err(error) => {
                        first_error.get_or_insert_with(|| format!("{player_asset}: {error:#}"));
                    }
                }
            }
            Err(error) => {
                first_error.get_or_insert_with(|| format!("{player_asset}: {error:#}"));
            }
        }
        let mut status = state.refresh_status.write().await;
        if let Some(error) = &first_error {
            // Publish failures immediately, even if a later player's batch is
            // slow. A working HTTP listener is not proof of working renewal.
            status.last_renewal_error = Some(error.clone());
        } else {
            status.last_renewal_at = Some(now_unix());
        }
    }
    if changed {
        persist(&state).await?;
    }
    if let Some(error) = first_error {
        bail!("delegated renewal failed: {error}");
    }
    Ok(())
}

async fn run_renewal_pass(state: Arc<AppState>) {
    let result = renew_delegated_players(state.clone()).await;
    let mut status = state.refresh_status.write().await;
    match result {
        Ok(()) => {
            status.last_renewal_at = Some(now_unix());
            status.last_renewal_error = None;
        }
        Err(error) => {
            eprintln!("woodland.sh server delegated renewal: {error:#}");
            status.last_renewal_error = Some(format!("delegated renewal: {error:#}"));
        }
    }
}

fn validate_web_manifest(root: &Path, manifest: &WorldManifest) -> Result<()> {
    let path = root.join("world.json");
    let bundled = WorldManifest::from_json(
        &std::fs::read_to_string(&path)
            .with_context(|| format!("read bundled world manifest {}", path.display()))?,
    )
    .context("validate bundled world manifest")?;
    if serde_json::to_value(&bundled)? != serde_json::to_value(manifest)? {
        bail!("bundled world.json does not match WOODLAND_WORLD_MANIFEST; rebuild the web bundle with the configured manifest");
    }
    Ok(())
}

fn setting(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set"))
}

fn optional_setting(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

fn canonical_http_origin(value: &str, name: &str, mainnet: bool) -> Result<String> {
    let url = reqwest::Url::parse(value).with_context(|| format!("parse {name}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || (mainnet && url.scheme() != "https")
    {
        bail!("{name} must be a canonical HTTP origin");
    }
    Ok(url.origin().ascii_serialization())
}

fn content_addressed_asset(path: &str) -> bool {
    fn is_digest(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    if let Some(digest) = path
        .strip_prefix("/app.")
        .and_then(|name| name.strip_suffix(".js"))
    {
        return is_digest(digest);
    }
    let Some((digest, filename)) = path
        .strip_prefix("/pkg/")
        .and_then(|path| path.split_once('/'))
    else {
        return false;
    };
    is_digest(digest)
        && (filename.ends_with(".js") || filename.ends_with(".wasm"))
        && !filename.contains(['%', '\\'])
        && filename
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

async fn no_store(request: Request, next: Next) -> Response {
    let immutable = matches!(*request.method(), Method::GET | Method::HEAD)
        && content_addressed_asset(request.uri().path());
    let mut response = next.run(request).await;
    let cache_control = if immutable
        && (response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED)
    {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    response
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn run_cli() -> Result<()> {
    let manifest_path = PathBuf::from(setting("WOODLAND_WORLD_MANIFEST")?);
    let registry_path = PathBuf::from(optional_setting(
        "WOODLAND_SERVER_DB",
        ".cache/woodland-server.json",
    ));
    let bind = optional_setting("WOODLAND_SERVER_BIND", DEFAULT_BIND)
        .parse::<SocketAddr>()
        .context("parse WOODLAND_SERVER_BIND")?;
    let refresh_secs = optional_setting(
        "WOODLAND_SERVER_REFRESH_SECONDS",
        &DEFAULT_REFRESH_SECS.to_string(),
    )
    .parse::<u64>()
    .context("parse WOODLAND_SERVER_REFRESH_SECONDS")?
    .max(5);
    let web_root = std::env::var("WOODLAND_SERVER_WEB_ROOT")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    if let Some(root) = &web_root {
        for required in ["index.html", "world.json", "404.html"] {
            let path = root.join(required);
            if !path.is_file() {
                bail!("server web root is missing {}", path.display());
            }
        }
    }

    let manifest = WorldManifest::from_json(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("read world manifest {}", manifest_path.display()))?,
    )?;
    if let Some(root) = &web_root {
        validate_web_manifest(root, &manifest)?;
    }
    let map_width = manifest.map_width;
    let map_height = manifest.map_height;
    let arkade_url = manifest.arkade_service_url.clone();
    let rest = ArkadeRest::new(&manifest.arkade_service_url);
    let emulator_rest = EmulatorRest::new(&manifest.emulator_url);
    let params = rest.get_info().await.context("read Arkade service info")?;
    let emulator = emulator_rest
        .get_info()
        .await
        .context("read emulator service info")?;
    let validated: ValidatedWorld = manifest
        .validate(&Secp256k1::new(), &params, &emulator)
        .context("validate server world")?;
    validated.verify_indexed_assets(&rest).await?;
    let mainnet = params.network == bitcoin::Network::Bitcoin;
    let force_renewal_once =
        std::env::var("WOODLAND_SERVER_FORCE_RENEWAL_ONCE").as_deref() == Ok("1");
    if force_renewal_once && params.network != bitcoin::Network::Regtest {
        bail!("WOODLAND_SERVER_FORCE_RENEWAL_ONCE is restricted to regtest");
    }
    let server_url = canonical_http_origin(
        &setting("WOODLAND_SERVER_PUBLIC_URL")?,
        "WOODLAND_SERVER_PUBLIC_URL",
        mainnet,
    )?;
    let origin = canonical_http_origin(
        &optional_setting("WOODLAND_SERVER_ORIGIN", &server_url),
        "WOODLAND_SERVER_ORIGIN",
        mainnet,
    )?;
    let origin = HeaderValue::from_str(&origin).context("encode server CORS origin")?;
    let rollover_keys = std::env::var("WOODLAND_ROLLOVER_SECRET")
        .ok()
        .filter(|secret| !secret.trim().is_empty())
        .map(|secret| Keys::from_hex(secret.trim()))
        .transpose()
        .context("parse WOODLAND_ROLLOVER_SECRET")?
        .map(Arc::new);
    if rollover_keys
        .as_ref()
        .is_some_and(|keys| keys.owner_pk() != validated.rollover_signer)
    {
        bail!("WOODLAND_ROLLOVER_SECRET does not match the world manifest");
    }
    let world = Arc::new(validated);
    let verifier = Arc::new(Verifier {
        rest,
        arkade_url,
        emulator_rest,
        params,
        emulator,
        world,
        server_url,
        map_width,
        map_height,
    });
    let registry = load_registry(&registry_path)?;
    validate_registry_consents(&registry, &verifier)?;
    let state = Arc::new(AppState {
        verifier,
        rollover_keys,
        registry: RwLock::new(registry),
        registry_path,
        persist_lock: Mutex::new(()),
        refresh_status: RwLock::new(RefreshStatus::default()),
        worker_freshness_secs: i64::try_from(refresh_secs.saturating_mul(3))
            .unwrap_or(i64::MAX)
            .max(MIN_WORKER_FRESHNESS_SECS),
        multiplayer: RwLock::new(MultiplayerState::default()),
        force_renewal_once: AtomicBool::new(force_renewal_once),
        refresh_cursor: AtomicUsize::new(0),
    });
    let refresh_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = refresh_batch(refresh_state.clone()).await {
                eprintln!("woodland.sh server refresh: {error:#}");
                refresh_state.refresh_status.write().await.last_error =
                    Some(format!("verification: {error:#}"));
            }
        }
    });
    // Bind HTTP without draining a correlated renewal wave first, and keep
    // verification independent of the potentially long-running batch joins.
    let renewal_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            run_renewal_pass(renewal_state.clone()).await;
        }
    });

    let app = Router::new()
        .route("/health.json", get(health))
        .route("/v1/leaderboard", get(leaderboard))
        .route("/v1/presence", get(presence))
        .route("/v1/chat", get(chat).post(post_chat))
        .route("/v1/players", post(register_player))
        .route("/v1/location", post(update_location))
        .route("/v1/delegation", post(set_delegation))
        .layer(DefaultBodyLimit::max(4096))
        .layer(
            CorsLayer::new()
                .allow_origin(origin)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([CONTENT_TYPE]),
        )
        .with_state(state);
    let app = if let Some(root) = web_root {
        let not_found = ServeFile::new(root.join("404.html"));
        app.fallback_service(
            ServeDir::new(root)
                .append_index_html_on_directories(true)
                .not_found_service(not_found),
        )
    } else {
        app
    };
    let app = app.layer(axum::middleware::from_fn(no_store));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind woodland.sh server at {bind}"))?;
    eprintln!("woodland.sh server ready at http://{bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve woodland.sh server")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Keypair, SecretKey};
    use bitcoin::Txid;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn only_successful_addressed_runtime_assets_are_cached() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("woodland-web-cache-{unique}"));
        let digest = "a".repeat(64);
        let entry = format!("app.{digest}.js");
        let module = format!("pkg/{digest}/woodland.js");
        let wasm = format!("pkg/{digest}/woodland_bg.wasm");
        let snippet = format!("pkg/{digest}/snippets/example/helper.js");
        for (filename, body) in [
            ("index.html", "entry page"),
            ("404.html", "not found"),
            ("world.json", "fresh manifest"),
            ("app.js", "unversioned entry"),
            ("pkg/woodland.js", "unversioned module"),
            (&entry, "addressed entry"),
            (&module, "addressed module"),
            (&wasm, "addressed wasm"),
            (&snippet, "addressed snippet"),
        ] {
            let file = root.join(filename);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        let app = Router::new()
            .route("/v1/leaderboard", get(|| async { "fresh leaderboard" }))
            .fallback_service(
                ServeDir::new(&root)
                    .append_index_html_on_directories(true)
                    .not_found_service(ServeFile::new(root.join("404.html"))),
            )
            .layer(axum::middleware::from_fn(no_store));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (filename, body) in [
            (&entry, "addressed entry"),
            (&module, "addressed module"),
            (&wasm, "addressed wasm"),
            (&snippet, "addressed snippet"),
        ] {
            let response = client
                .get(format!("{base}/{filename}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{filename}");
            assert_eq!(
                response.headers()[CACHE_CONTROL],
                "public, max-age=31536000, immutable",
                "{filename}"
            );
            assert_eq!(response.text().await.unwrap(), body, "{filename}");
        }
        let missing = format!("pkg/{digest}/missing.js");
        for (filename, status, body) in [
            ("", StatusCode::OK, "entry page"),
            ("world.json", StatusCode::OK, "fresh manifest"),
            ("v1/leaderboard", StatusCode::OK, "fresh leaderboard"),
            ("app.js?version=123", StatusCode::OK, "unversioned entry"),
            ("pkg/woodland.js", StatusCode::OK, "unversioned module"),
            (missing.as_str(), StatusCode::NOT_FOUND, "not found"),
        ] {
            let response = client
                .get(format!("{base}/{filename}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{filename}");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{filename}");
            assert_eq!(response.text().await.unwrap(), body, "{filename}");
        }
        let response = client.head(format!("{base}/{wasm}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let modified = response.headers()[axum::http::header::LAST_MODIFIED].clone();
        let response = client
            .get(format!("{base}/{wasm}"))
            .header(axum::http::header::IF_MODIFIED_SINCE, modified)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response.headers()[CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let response = client.post(format!("{base}/{entry}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn player(asset: &str, xp_balance: u64, active: bool, registered_at: i64) -> LeaderboardPlayer {
        let woodcutting_xp = player::woodcutting_xp(xp_balance);
        LeaderboardPlayer {
            owner: "00".repeat(32),
            player_asset: asset.to_owned(),
            xp: woodcutting_xp,
            level: player::level_from_xp(woodcutting_xp),
            logs: xp_balance,
            state_outpoint: format!("{}:0", "00".repeat(32)),
            expires_at: Some(100),
            rollover_margin_seconds: 600,
            active,
            registered_at,
            updated_at: 1,
        }
    }

    fn registered_player() -> RegisteredPlayer {
        let state = player("player", 1, true, 1);
        RegisteredPlayer {
            owner: state.owner.clone(),
            player_asset: state.player_asset.clone(),
            registration_signature: "initial registration".into(),
            registered_at: state.registered_at,
            delegated_renewal: true,
            delegation_updated_at_ms: 1,
            state: Some(state),
        }
    }

    #[test]
    fn stale_verification_cannot_overwrite_a_completed_renewal() {
        let mut entry = registered_player();
        let before = entry.clone();
        let mut renewed = before.state.clone().unwrap();
        renewed.state_outpoint = format!("{}:0", "01".repeat(32));
        renewed.expires_at = Some(10_000);
        renewed.updated_at = 2;
        assert!(entry.apply_verified_state(&before, Some(renewed.clone()), 2));

        // An earlier refresh may report either the old input or the temporary
        // indexing gap between it and the renewed output. Neither may win.
        assert!(!entry.apply_verified_state(&before, before.state.clone(), 3));
        assert!(!entry.apply_verified_state(&before, None, 3));
        assert_eq!(entry.state, Some(renewed));

        // The same guard covers a renewal verification overtaken by a newer
        // refresh, even when both operations complete in the same second.
        let pending_renewal = entry.clone();
        let mut latest = entry.state.clone().unwrap();
        latest.state_outpoint = format!("{}:0", "02".repeat(32));
        assert!(entry.apply_verified_state(&pending_renewal, Some(latest.clone()), 3));
        assert!(!entry.apply_verified_state(&pending_renewal, pending_renewal.state.clone(), 3));
        assert_eq!(entry.state, Some(latest));
    }

    #[test]
    fn queued_renewal_rechecks_consent_identity_and_current_expiry() {
        let queued = registered_player();
        assert!(queued.queued_renewal_authorized(&queued, 1, false));
        let mut current = queued.clone();
        current.delegated_renewal = false;
        current.delegation_updated_at_ms = 2;
        assert!(!current.queued_renewal_authorized(&queued, 1, false));
        current.delegated_renewal = true;
        current.delegation_updated_at_ms = 3;
        assert!(!current.queued_renewal_authorized(&queued, 1, false));

        current = queued.clone();
        current.registration_signature = "new registration".into();
        assert!(!current.queued_renewal_authorized(&queued, 1, false));
        current = queued.clone();
        current.state.as_mut().unwrap().expires_at = Some(10_000);
        assert!(!current.queued_renewal_authorized(&queued, 1, false));
    }

    #[test]
    fn slow_registration_preserves_revocation_and_newer_cached_state() {
        let snapshot = registered_player();
        let mut current = snapshot.clone();
        current.delegated_renewal = false;
        current.delegation_updated_at_ms = 2;
        let mut renewed = current.state.clone().unwrap();
        renewed.state_outpoint = format!("{}:0", "01".repeat(32));
        renewed.expires_at = Some(10_000);
        current.state = Some(renewed.clone());
        let mut registry = RegistryFile::default();
        registry
            .players
            .insert(current.player_asset.clone(), current);
        let response = merge_verified_registration(
            &mut registry,
            Some(&snapshot),
            "new registration".into(),
            snapshot.state.clone().unwrap(),
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        let entry = &registry.players[&snapshot.player_asset];
        assert!(!entry.delegated_renewal);
        assert_eq!(entry.delegation_updated_at_ms, 2);
        assert_eq!(entry.registered_at, snapshot.registered_at);
        assert_eq!(entry.registration_signature, "new registration");
        assert_eq!(entry.state, Some(renewed.clone()));
        assert_eq!(response, renewed);
    }

    fn app_state() -> Arc<AppState> {
        let (secp, params, emulator, manifest) = crate::world::tests::fixture();
        let world = Arc::new(manifest.validate(&secp, &params, &emulator).unwrap());
        Arc::new(AppState {
            verifier: Arc::new(Verifier {
                rest: ArkadeRest::new(&manifest.arkade_service_url),
                arkade_url: manifest.arkade_service_url.clone(),
                emulator_rest: EmulatorRest::new(&manifest.emulator_url),
                params,
                emulator,
                world,
                server_url: "https://server.example".to_owned(),
                map_width: manifest.map_width,
                map_height: manifest.map_height,
            }),
            rollover_keys: Some(Arc::new(Keys::from_hex(&"07".repeat(32)).unwrap())),
            registry: RwLock::new(RegistryFile::default()),
            registry_path: PathBuf::from("unused-test-registry.json"),
            persist_lock: Mutex::new(()),
            refresh_status: RwLock::new(RefreshStatus::default()),
            worker_freshness_secs: MIN_WORKER_FRESHNESS_SECS,
            multiplayer: RwLock::new(MultiplayerState::default()),
            force_renewal_once: AtomicBool::new(false),
            refresh_cursor: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn delegation_and_readiness_require_recent_successful_worker_progress() {
        let state = app_state();
        assert_eq!(
            health(State(state.clone())).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let now = now_unix();
        *state.refresh_status.write().await = RefreshStatus {
            last_refresh_at: Some(now),
            last_renewal_at: Some(now),
            ..Default::default()
        };
        assert_eq!(health(State(state.clone())).await.0, StatusCode::OK);
        // A normal batch join may outlast the shorter verification lease.
        state.refresh_status.write().await.last_renewal_at = Some(now - 600);
        assert_eq!(health(State(state.clone())).await.0, StatusCode::OK);
        for failure in ["renewal error", "renewal stalled", "verification stalled"] {
            {
                let mut status = state.refresh_status.write().await;
                status.last_refresh_at = Some(now);
                status.last_renewal_at = Some(now);
                status.last_renewal_error = None;
                match failure {
                    "renewal error" => {
                        status.last_renewal_error = Some("missing fee funding".into())
                    }
                    "renewal stalled" => {
                        status.last_renewal_at = Some(now - MIN_RENEWAL_FRESHNESS_SECS - 1)
                    }
                    _ => status.last_refresh_at = Some(now - MIN_WORKER_FRESHNESS_SECS - 1),
                }
            }
            let (code, Json(health)) = health(State(state.clone())).await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{failure}");
            assert!(!health.ready && !health.delegation_available, "{failure}");
            let Json(board) = leaderboard(
                State(state.clone()),
                Query(LeaderboardQuery {
                    limit: None,
                    offset: None,
                }),
            )
            .await;
            assert!(!board.delegation_available, "{failure}");
        }
        state.refresh_status.write().await.last_refresh_at = Some(now);
        run_renewal_pass(state.clone()).await;
        assert_eq!(health(State(state.clone())).await.0, StatusCode::OK);
    }

    #[test]
    fn served_world_must_match_the_configured_signed_manifest() {
        let (secp, params, emulator, manifest) = crate::world::tests::fixture();
        let world = manifest.validate(&secp, &params, &emulator).unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("woodland-web-manifest-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("world.json");
        std::fs::write(&path, manifest.to_json().unwrap()).unwrap();
        validate_web_manifest(&root, &manifest).unwrap();

        // A separately signed, internally valid manifest must still be rejected.
        let deployments = world
            .trees
            .iter()
            .map(|tree| (tree.state, tree.deployment_txid))
            .collect::<Vec<_>>();
        let other = WorldManifest::new(
            &params,
            &emulator,
            &Keys::from_hex(&"08".repeat(32)).unwrap(),
            "http://different-world.example",
            &manifest.emulator_url,
            world.rollover_signer,
            world.tree_asset,
            world.log_asset,
            world.xp_asset,
            world.stone_asset,
            world.iron_ore_asset,
            &world.contract,
            world.genesis_txid,
            &deployments,
        )
        .unwrap();
        std::fs::write(&path, other.to_json().unwrap()).unwrap();
        let error = validate_web_manifest(&root, &manifest).unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match WOODLAND_WORLD_MANIFEST"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaderboard_order_is_active_then_xp_then_registration() {
        let mut players = [
            player("c", 5, false, 0),
            player("b", 1, true, 2),
            player("a", 1, true, 1),
            player("d", 2, true, 3),
        ];
        assert_eq!(players[0].xp, 125);
        assert_eq!(players[0].level, 2);
        assert_eq!(players[1].xp, 25);
        assert_eq!(players[1].level, 1);
        players.sort_by(|left, right| {
            right
                .active
                .cmp(&left.active)
                .then_with(|| right.xp.cmp(&left.xp))
                .then_with(|| left.registered_at.cmp(&right.registered_at))
                .then_with(|| left.player_asset.cmp(&right.player_asset))
        });
        assert_eq!(
            players.map(|player| player.player_asset),
            ["d", "a", "b", "c"]
        );
    }

    #[test]
    fn player_metadata_commits_owner_and_protocol() {
        let keypair =
            Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[3; 32]).unwrap());
        let owner = keypair.x_only_public_key().0;
        let metadata = player_asset_metadata(owner);
        assert!(metadata
            .windows(GAME_ID.len())
            .any(|window| window == GAME_ID.as_bytes()));
        assert!(metadata
            .windows("PLAYER_ID".len())
            .any(|window| window == b"PLAYER_ID"));
        assert!(metadata
            .windows(owner.to_string().len())
            .any(|window| window == owner.to_string().as_bytes()));
    }

    #[test]
    fn registration_signature_is_bound_to_service() {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[3; 32]).unwrap());
        let owner = keypair.x_only_public_key().0;
        let genesis = Txid::from_byte_array([4; 32]);
        let asset = AssetId {
            txid: Txid::from_byte_array([5; 32]),
            group_index: 0,
        };
        let message =
            player::server_registration_message(genesis, owner, asset, "https://server.example");
        let signature = secp.sign_schnorr_no_aux_rand(&message, &keypair);
        assert!(secp.verify_schnorr(&signature, &message, &owner).is_ok());
        let other_service =
            player::server_registration_message(genesis, owner, asset, "https://other.example");
        assert!(secp
            .verify_schnorr(&signature, &other_service, &owner)
            .is_err());
    }

    #[test]
    fn server_action_signature_binds_action_time_and_payload() {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[6; 32]).unwrap());
        let owner = keypair.x_only_public_key().0;
        let genesis = Txid::from_byte_array([7; 32]);
        let asset = AssetId {
            txid: Txid::from_byte_array([8; 32]),
            group_index: 0,
        };
        let message = player::server_action_message(
            genesis,
            owner,
            asset,
            "https://server.example",
            player::SERVER_ACTION_LOCATION,
            1234,
            "x=3\ny=17\n",
        );
        let signature = secp.sign_schnorr_no_aux_rand(&message, &keypair);
        assert!(secp.verify_schnorr(&signature, &message, &owner).is_ok());
        for invalid in [
            player::server_action_message(
                genesis,
                owner,
                asset,
                "https://server.example",
                player::SERVER_ACTION_LOCATION,
                1234,
                "x=4\ny=17\n",
            ),
            player::server_action_message(
                genesis,
                owner,
                asset,
                "https://server.example",
                player::SERVER_ACTION_CHAT,
                1234,
                "x=3\ny=17\n",
            ),
        ] {
            assert!(secp.verify_schnorr(&signature, &invalid, &owner).is_err());
        }
    }

    #[test]
    fn location_index_moves_players_between_viewport_chunks() {
        let mut index = LocationIndex::default();
        index.upsert(PlayerLocation {
            player_asset: "a".to_owned(),
            x: 1,
            y: 2,
            updated_at_ms: 100,
        });
        index.upsert(PlayerLocation {
            player_asset: "b".to_owned(),
            x: 40,
            y: 2,
            updated_at_ms: 100,
        });
        let first_chunk = PresenceQuery {
            min_x: 0,
            min_y: 0,
            max_x: 31,
            max_y: 31,
        };
        assert_eq!(index.query(&first_chunk).0[0].player_asset, "a");
        index.upsert(PlayerLocation {
            player_asset: "a".to_owned(),
            x: 41,
            y: 3,
            updated_at_ms: 200,
        });
        assert!(index.query(&first_chunk).0.is_empty());
        let second_chunk = PresenceQuery {
            min_x: 32,
            min_y: 0,
            max_x: 63,
            max_y: 31,
        };
        assert_eq!(index.query(&second_chunk).0.len(), 2);
        index.retain_active(&BTreeSet::from(["a".to_owned()]), 150);
        assert_eq!(index.query(&second_chunk).0[0].player_asset, "a");
    }

    #[test]
    fn registration_replay_short_circuits_only_on_identical_signature() {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[9; 32]).unwrap());
        let owner = keypair.x_only_public_key().0;
        let asset = AssetId {
            txid: Txid::from_byte_array([10; 32]),
            group_index: 0,
        };
        let genesis = Txid::from_byte_array([4; 32]);
        let message =
            player::server_registration_message(genesis, owner, asset, "https://server.example");
        let signature = secp.sign_schnorr_no_aux_rand(&message, &keypair);
        let other_message =
            player::server_registration_message(genesis, owner, asset, "https://other.example");
        let other = secp.sign_schnorr_no_aux_rand(&other_message, &keypair);
        let mut registry = RegistryFile::default();
        registry.players.insert(
            asset.to_string(),
            RegisteredPlayer {
                owner: owner.to_string(),
                player_asset: asset.to_string(),
                registration_signature: signature.to_string(),
                delegated_renewal: false,
                delegation_updated_at_ms: 0,
                registered_at: 1,
                state: Some(player(&asset.to_string(), 7, true, 1)),
            },
        );
        // Identical replay: served from the registry, no upstream verify.
        let replayed = replayed_registration(&registry, owner, asset, signature)
            .expect("identical replay hits the fast path");
        assert_eq!(replayed.player_asset, asset.to_string());
        assert_eq!(replayed.registered_at, 1);
        // A different signature, owner, or unknown PLAYER_ID falls through to
        // the full upstream verify.
        assert!(replayed_registration(&registry, owner, asset, other).is_none());
        let stranger = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[11; 32]).unwrap())
            .x_only_public_key()
            .0;
        assert!(replayed_registration(&registry, stranger, asset, signature).is_none());
        let unknown = AssetId {
            txid: Txid::from_byte_array([12; 32]),
            group_index: 0,
        };
        assert!(replayed_registration(&registry, owner, unknown, signature).is_none());
        // Without cached live state there is nothing to answer with.
        registry.players.get_mut(&asset.to_string()).unwrap().state = None;
        assert!(replayed_registration(&registry, owner, asset, signature).is_none());
    }

    #[test]
    fn registry_round_trips_atomically() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("woodland-server-{unique}"));
        let path = root.join("registry.json");
        let mut registry = RegistryFile::default();
        registry.players.insert(
            "player".to_owned(),
            RegisteredPlayer {
                owner: "owner".to_owned(),
                player_asset: "player".to_owned(),
                registration_signature: "signature".to_owned(),
                delegated_renewal: true,
                delegation_updated_at_ms: 2,
                registered_at: 1,
                state: Some(player("player", 7, true, 1)),
            },
        );
        save_registry(&path, &registry).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("\"state\""));
        let loaded = load_registry(&path).unwrap();
        assert_eq!(loaded.players.len(), 1);
        assert!(loaded.players["player"].state.is_none());
        assert!(loaded.players["player"].delegated_renewal);
        std::fs::remove_dir_all(root).unwrap();
    }
}
