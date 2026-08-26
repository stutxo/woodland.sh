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
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header::CONTENT_TYPE, HeaderValue, Method, StatusCode};
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tower_http::cors::CorsLayer;

const REGISTRY_SCHEMA: u32 = 2;
const DEFAULT_BIND: &str = "127.0.0.1:8090";
const DEFAULT_REFRESH_SECS: u64 = 15;
const MAX_REGISTERED_PLAYERS: usize = 10_000;
const VERIFY_CONCURRENCY: usize = 8;
const MAX_CHAT_MESSAGES: usize = 200;
const PRESENCE_TTL_MS: u64 = 60_000;
const ACTION_CLOCK_SKEW_MS: u64 = 300_000;
const LOCATION_INTERVAL_MS: u64 = 500;
const CHAT_INTERVAL_MS: u64 = 2_000;

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
    state: Option<LeaderboardPlayer>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    id: u64,
    player_asset: String,
    message: String,
    created_at_ms: u64,
}

#[derive(Default)]
struct RefreshStatus {
    last_refresh_at: Option<i64>,
    last_error: Option<String>,
}

struct AppState {
    verifier: Arc<Verifier>,
    rollover_keys: Option<Arc<Keys>>,
    registry: RwLock<RegistryFile>,
    registry_path: PathBuf,
    persist_lock: Mutex<()>,
    refresh_status: RwLock<RefreshStatus>,
    locations: RwLock<BTreeMap<String, PlayerLocation>>,
    chat: RwLock<VecDeque<ChatMessage>>,
    action_timestamps: Mutex<BTreeMap<String, u64>>,
    next_chat_id: AtomicU64,
    force_renewal_once: AtomicBool,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaderboardResponse {
    generated_at: i64,
    players: Vec<LeaderboardPlayer>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialResponse {
    generated_at_ms: u64,
    players: Vec<LeaderboardPlayer>,
    locations: Vec<PlayerLocation>,
    messages: Vec<ChatMessage>,
    delegated_player_assets: Vec<String>,
    delegation_available: bool,
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
        let state = player::player_state_from_tx(&previous_tx)?
            .ok_or_else(|| anyhow!("player creating transaction has no state packets"))?;
        let expected_identity = player::derive_player_identity(owner, self.world.genesis_txid);
        let xp_balance = record.asset_amount(self.world.xp_asset).unwrap_or(0);
        if state.identity != expected_identity || state.xp.value() != xp_balance {
            bail!("player identity or XP backing is invalid");
        }
        let now = now_unix();
        Ok(Some(LeaderboardPlayer {
            owner: owner.to_string(),
            player_asset: player_asset.to_string(),
            xp: state.xp.value(),
            level: player::level_from_xp(state.xp.value()),
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
        if entry.state.as_ref().is_some_and(|state| {
            state.owner != entry.owner || state.player_asset != entry.player_asset
        }) {
            bail!("cached verified state does not match its registration");
        }
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
    {
        let registry = state.registry.read().await;
        let registered = registry.players.get(&key).is_some_and(|entry| {
            entry.owner == owner.to_string()
                && entry.state.as_ref().is_some_and(|player| {
                    player.active
                        && player
                            .expires_at
                            .is_none_or(|expires_at| expires_at > now_unix())
                })
        });
        if !registered {
            return Err(ApiError::not_found(
                "player is not active and registered with this server",
            ));
        }
    }
    if now_ms().abs_diff(request.timestamp_ms) > ACTION_CLOCK_SKEW_MS {
        return Err(ApiError::bad_request(
            "server action timestamp is outside the allowed window",
        ));
    }
    let action_key = format!("{}:{key}", request.action);
    let mut timestamps = state.action_timestamps.lock().await;
    if let Some(previous) = timestamps.get(&action_key) {
        if request.timestamp_ms <= *previous
            || request.timestamp_ms.saturating_sub(*previous) < request.minimum_interval_ms
        {
            return Err(ApiError::too_many_requests(
                "server action was replayed or sent too quickly",
            ));
        }
    }
    timestamps.insert(action_key, request.timestamp_ms);
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
        .locations
        .write()
        .await
        .insert(player_asset.to_string(), location.clone());
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
    let message = ChatMessage {
        id: state.next_chat_id.fetch_add(1, Ordering::Relaxed),
        player_asset: player_asset.to_string(),
        message: request.message,
        created_at_ms: now_ms(),
    };
    let mut chat = state.chat.write().await;
    chat.push_back(message.clone());
    while chat.len() > MAX_CHAT_MESSAGES {
        chat.pop_front();
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
    let (registered_at, delegated_renewal, delegation_updated_at_ms) =
        state.registry.read().await.players.get(&key).map_or_else(
            || (now_unix(), false, 0),
            |entry| {
                (
                    entry.registered_at,
                    entry.delegated_renewal,
                    entry.delegation_updated_at_ms,
                )
            },
        );
    let verified = state
        .verifier
        .verify(owner, player_asset, registered_at)
        .await
        .map_err(ApiError::upstream)?
        .ok_or_else(|| ApiError::not_found("no live state for this owner and PLAYER_ID"))?;

    {
        let mut registry = state.registry.write().await;
        if !registry.players.contains_key(&key) && registry.players.len() >= MAX_REGISTERED_PLAYERS
        {
            return Err(ApiError::bad_request("server registration limit reached"));
        }
        registry.players.insert(
            key,
            RegisteredPlayer {
                owner: owner.to_string(),
                player_asset: player_asset.to_string(),
                registration_signature: signature.to_string(),
                delegated_renewal,
                delegation_updated_at_ms,
                registered_at,
                state: Some(verified.clone()),
            },
        );
    }
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

async fn leaderboard(State(state): State<Arc<AppState>>) -> Json<LeaderboardResponse> {
    let registry = state.registry.read().await;
    let players = leaderboard_players(&registry);
    Json(LeaderboardResponse {
        generated_at: now_unix(),
        players,
    })
}

async fn social(State(state): State<Arc<AppState>>) -> Json<SocialResponse> {
    let (players, delegated_player_assets) = {
        let registry = state.registry.read().await;
        let players = leaderboard_players(&registry);
        let delegated = registry
            .players
            .values()
            .filter(|entry| entry.delegated_renewal)
            .map(|entry| entry.player_asset.clone())
            .collect::<Vec<_>>();
        (players, delegated)
    };
    let active = players
        .iter()
        .filter(|player| player.active)
        .map(|player| player.player_asset.clone())
        .collect::<BTreeSet<_>>();
    let current_time = now_ms();
    let locations = {
        let mut locations = state.locations.write().await;
        locations.retain(|player_asset, location| {
            active.contains(player_asset)
                && current_time.saturating_sub(location.updated_at_ms) <= PRESENCE_TTL_MS
        });
        locations.values().cloned().collect()
    };
    let messages = state.chat.read().await.iter().cloned().collect();
    Json(SocialResponse {
        generated_at_ms: current_time,
        players,
        locations,
        messages,
        delegated_player_assets,
        delegation_available: state.rollover_keys.is_some(),
    })
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let registry = state.registry.read().await;
    let status = state.refresh_status.read().await;
    let cutoff = now_ms().saturating_sub(PRESENCE_TTL_MS);
    let online_players = state
        .locations
        .read()
        .await
        .values()
        .filter(|location| location.updated_at_ms >= cutoff)
        .count();
    Json(HealthResponse {
        ready: true,
        registered_players: registry.players.len(),
        last_refresh_at: status.last_refresh_at,
        last_error: status.last_error.clone(),
        online_players,
        delegation_available: state.rollover_keys.is_some(),
    })
}

async fn refresh_all(state: Arc<AppState>) -> Result<()> {
    let registrations = state
        .registry
        .read()
        .await
        .players
        .values()
        .cloned()
        .collect::<Vec<_>>();
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
                Ok::<_, anyhow::Error>((entry.player_asset, result))
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
                Ok((key, Ok(Some(player)))) => {
                    if let Some(entry) = registry.players.get_mut(&key) {
                        entry.state = Some(player);
                    }
                }
                Ok((key, Ok(None))) => {
                    if let Some(entry) = registry.players.get_mut(&key) {
                        if let Some(player) = &mut entry.state {
                            player.active = false;
                            player.updated_at = now;
                        }
                    }
                }
                Ok((key, Err(error))) => {
                    first_error.get_or_insert_with(|| format!("{key}: {error:#}"));
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| format!("{error:#}"));
                }
            }
        }
    }
    persist(&state).await?;
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
        .filter(|entry| {
            entry.delegated_renewal
                && entry.state.as_ref().is_some_and(|player| {
                    player.active
                        && (force_requested
                            || player.expires_at.is_some_and(|expires_at| {
                                let remaining = expires_at - now;
                                remaining > 0 && remaining < player.rollover_margin_seconds
                            }))
                })
        })
        .map(|entry| {
            (
                entry.owner.clone(),
                entry.player_asset.clone(),
                entry.registered_at,
            )
        })
        .collect::<Vec<_>>();
    if due.is_empty() {
        return Ok(());
    }
    let force = force_requested && state.force_renewal_once.swap(false, Ordering::Relaxed);
    let mut first_error = None;
    let mut changed = false;
    for (owner_text, player_asset_text, registered_at) in due {
        let owner = XOnlyPublicKey::from_str(&owner_text).context("parse delegated owner")?;
        let player_asset =
            AssetId::from_str(&player_asset_text).context("parse delegated PLAYER_ID")?;
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
                match state
                    .verifier
                    .verify(owner, player_asset, registered_at)
                    .await
                {
                    Ok(Some(player)) => {
                        if let Some(entry) = state
                            .registry
                            .write()
                            .await
                            .players
                            .get_mut(&player_asset_text)
                        {
                            entry.state = Some(player);
                            changed = true;
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
    }
    if changed {
        persist(&state).await?;
    }
    if let Some(error) = first_error {
        bail!("delegated renewal failed: {error}");
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

    let manifest = WorldManifest::from_json(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("read world manifest {}", manifest_path.display()))?,
    )?;
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
    let origin = canonical_http_origin(
        &setting("WOODLAND_SERVER_ORIGIN")?,
        "WOODLAND_SERVER_ORIGIN",
        mainnet,
    )?;
    let origin = HeaderValue::from_str(&origin).context("encode server CORS origin")?;
    let server_url = canonical_http_origin(
        &setting("WOODLAND_SERVER_PUBLIC_URL")?,
        "WOODLAND_SERVER_PUBLIC_URL",
        mainnet,
    )?;
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
    let mut registry = load_registry(&registry_path)?;
    validate_registry_consents(&registry, &verifier)?;
    for entry in registry.players.values_mut() {
        entry.state = None;
    }
    let state = Arc::new(AppState {
        verifier,
        rollover_keys,
        registry: RwLock::new(registry),
        registry_path,
        persist_lock: Mutex::new(()),
        refresh_status: RwLock::new(RefreshStatus::default()),
        locations: RwLock::new(BTreeMap::new()),
        chat: RwLock::new(VecDeque::new()),
        action_timestamps: Mutex::new(BTreeMap::new()),
        next_chat_id: AtomicU64::new(1),
        force_renewal_once: AtomicBool::new(force_renewal_once),
    });
    if let Err(error) = refresh_all(state.clone()).await {
        eprintln!("woodland.sh server initial refresh: {error:#}");
    }
    if let Err(error) = renew_delegated_players(state.clone()).await {
        eprintln!("woodland.sh server initial delegated renewal: {error:#}");
    }

    let refresh_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = refresh_all(refresh_state.clone()).await {
                eprintln!("woodland.sh server refresh: {error:#}");
            }
            if let Err(error) = renew_delegated_players(refresh_state.clone()).await {
                eprintln!("woodland.sh server delegated renewal: {error:#}");
                refresh_state.refresh_status.write().await.last_error =
                    Some(format!("delegated renewal: {error:#}"));
            }
        }
    });

    let app = Router::new()
        .route("/health.json", get(health))
        .route("/v1/leaderboard", get(leaderboard))
        .route("/v1/social", get(social))
        .route("/v1/players", post(register_player))
        .route("/v1/location", post(update_location))
        .route("/v1/chat", post(post_chat))
        .route("/v1/delegation", post(set_delegation))
        .layer(DefaultBodyLimit::max(4096))
        .layer(
            CorsLayer::new()
                .allow_origin(origin)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([CONTENT_TYPE]),
        )
        .with_state(state);
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

    fn player(asset: &str, xp: u64, active: bool, registered_at: i64) -> LeaderboardPlayer {
        LeaderboardPlayer {
            owner: "00".repeat(32),
            player_asset: asset.to_owned(),
            xp,
            level: player::level_from_xp(xp),
            logs: xp,
            state_outpoint: format!("{}:0", "00".repeat(32)),
            expires_at: Some(100),
            rollover_margin_seconds: 600,
            active,
            registered_at,
            updated_at: 1,
        }
    }

    #[test]
    fn leaderboard_order_is_active_then_xp_then_registration() {
        let mut players = [
            player("c", 5, false, 0),
            player("b", 1, true, 2),
            player("a", 1, true, 1),
            player("d", 2, true, 3),
        ];
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
        assert_eq!(load_registry(&path).unwrap().players.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
