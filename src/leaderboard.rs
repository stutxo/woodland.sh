//! Opt-in player directory and independently verified XP leaderboard.
//!
//! Registration stores only public owner and PLAYER_ID values. Every displayed
//! score is reconstructed from the current Arkade VTXO, its creating transaction,
//! the personalized covenant, and the player's conserved XP asset balance.

use crate::arkade::{now_unix, ArkadeRest, EmulatorParams, EmulatorRest, ServerParams, VtxoRecord};
use crate::player::{self, PlayerContract};
use crate::tree::TreeContract;
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
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tower_http::cors::CorsLayer;

const REGISTRY_SCHEMA: u32 = 1;
const DEFAULT_BIND: &str = "127.0.0.1:8090";
const DEFAULT_REFRESH_SECS: u64 = 15;
const MAX_REGISTERED_PLAYERS: usize = 10_000;
const VERIFY_CONCURRENCY: usize = 8;

#[derive(Clone)]
struct Verifier {
    rest: ArkadeRest,
    params: ServerParams,
    emulator: EmulatorParams,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    rollover_signer: XOnlyPublicKey,
    genesis_txid: bitcoin::Txid,
    leaderboard_url: String,
    tree_contract: TreeContract,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegisteredPlayer {
    owner: String,
    player_asset: String,
    registration_signature: String,
    registered_at: i64,
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
    pub active: bool,
    pub registered_at: i64,
    pub updated_at: i64,
}

#[derive(Default)]
struct RefreshStatus {
    last_refresh_at: Option<i64>,
    last_error: Option<String>,
}

struct AppState {
    verifier: Arc<Verifier>,
    registry: RwLock<RegistryFile>,
    registry_path: PathBuf,
    persist_lock: Mutex<()>,
    refresh_status: RwLock<RefreshStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegisterPlayerRequest {
    owner: String,
    player_asset: String,
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
struct HealthResponse {
    ready: bool,
    registered_players: usize,
    last_refresh_at: Option<i64>,
    last_error: Option<String>,
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

    fn upstream(error: anyhow::Error) -> Self {
        eprintln!("woodland.sh leaderboard verification: {error:#}");
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: "player verification failed".to_owned(),
        }
    }

    fn internal(error: anyhow::Error) -> Self {
        eprintln!("woodland.sh leaderboard: {error:#}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "leaderboard storage failed".to_owned(),
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
        verifier.rollover_signer,
        verifier.params.unilateral_exit_delay,
        verifier.params.network,
        verifier.tree_asset,
        verifier.log_asset,
        verifier.xp_asset,
        verifier.params.dust_sats,
        &verifier.tree_contract.vtxo.script_pubkey(),
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
        let message = player::leaderboard_registration_message(
            self.genesis_txid,
            owner,
            player_asset,
            &self.leaderboard_url,
        );
        Secp256k1::verification_only()
            .verify_schnorr(&signature, &message, &owner)
            .context("registration signature does not authorize this leaderboard")
    }

    async fn verify(
        &self,
        owner: XOnlyPublicKey,
        player_asset: AssetId,
        registered_at: i64,
    ) -> Result<Option<LeaderboardPlayer>> {
        if [self.tree_asset, self.log_asset, self.xp_asset].contains(&player_asset) {
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
        let expected_identity = player::derive_player_identity(owner, self.genesis_txid);
        let xp_balance = record.asset_amount(self.xp_asset).unwrap_or(0);
        if state.identity != expected_identity || state.xp.value() != xp_balance {
            bail!("player identity or XP backing is invalid");
        }
        let now = now_unix();
        Ok(Some(LeaderboardPlayer {
            owner: owner.to_string(),
            player_asset: player_asset.to_string(),
            xp: state.xp.value(),
            level: player::level_from_xp(state.xp.value()),
            logs: record.asset_amount(self.log_asset).unwrap_or(0),
            state_outpoint: record.outpoint.to_string(),
            expires_at: record.expires_at,
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
            .with_context(|| format!("read leaderboard registry {}", path.display()))?,
    )
    .with_context(|| format!("parse leaderboard registry {}", path.display()))?;
    if registry.schema_version != REGISTRY_SCHEMA {
        bail!(
            "unsupported leaderboard registry schema {}",
            registry.schema_version
        );
    }
    if registry.players.len() > MAX_REGISTERED_PLAYERS {
        bail!("leaderboard registry exceeds player limit");
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
            bail!("leaderboard registry key does not match PLAYER_ID");
        }
        let signature = schnorr::Signature::from_str(&entry.registration_signature)
            .with_context(|| format!("parse registration signature for {key}"))?;
        verifier
            .verify_registration_signature(owner, player_asset, signature)
            .with_context(|| format!("verify registration consent for {key}"))?;
        if entry.state.as_ref().is_some_and(|state| {
            state.owner != entry.owner || state.player_asset != entry.player_asset
        }) {
            bail!("cached leaderboard state does not match its registration");
        }
    }
    Ok(())
}

fn save_registry(path: &Path, registry: &RegistryFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("leaderboard registry path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create leaderboard directory {}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(
        &temporary,
        format!("{}\n", serde_json::to_string_pretty(registry)?),
    )
    .with_context(|| format!("write leaderboard registry {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("publish leaderboard registry {}", path.display()))
}

async fn persist(state: &AppState) -> Result<()> {
    let _guard = state.persist_lock.lock().await;
    let snapshot = state.registry.read().await.clone();
    save_registry(&state.registry_path, &snapshot)
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
            ApiError::bad_request("signature does not authorize this leaderboard registration")
        })?;
    let key = player_asset.to_string();
    let registered_at = state
        .registry
        .read()
        .await
        .players
        .get(&key)
        .map_or_else(now_unix, |entry| entry.registered_at);
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
            return Err(ApiError::bad_request(
                "leaderboard registration limit reached",
            ));
        }
        registry.players.insert(
            key,
            RegisteredPlayer {
                owner: owner.to_string(),
                player_asset: player_asset.to_string(),
                registration_signature: signature.to_string(),
                registered_at,
                state: Some(verified.clone()),
            },
        );
    }
    persist(&state).await.map_err(ApiError::internal)?;
    Ok(Json(verified))
}

async fn leaderboard(State(state): State<Arc<AppState>>) -> Json<LeaderboardResponse> {
    let registry = state.registry.read().await;
    let mut players = registry
        .players
        .values()
        .filter_map(|entry| entry.state.clone())
        .collect::<Vec<_>>();
    let now = now_unix();
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
    Json(LeaderboardResponse {
        generated_at: now_unix(),
        players,
    })
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let registry = state.registry.read().await;
    let status = state.refresh_status.read().await;
    Json(HealthResponse {
        ready: true,
        registered_players: registry.players.len(),
        last_refresh_at: status.last_refresh_at,
        last_error: status.last_error.clone(),
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
        "WOODLAND_LEADERBOARD_DB",
        ".cache/woodland-leaderboard.json",
    ));
    let bind = optional_setting("WOODLAND_LEADERBOARD_BIND", DEFAULT_BIND)
        .parse::<SocketAddr>()
        .context("parse WOODLAND_LEADERBOARD_BIND")?;
    let refresh_secs = optional_setting(
        "WOODLAND_LEADERBOARD_REFRESH_SECS",
        &DEFAULT_REFRESH_SECS.to_string(),
    )
    .parse::<u64>()
    .context("parse WOODLAND_LEADERBOARD_REFRESH_SECS")?
    .max(5);

    let manifest = WorldManifest::from_json(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("read world manifest {}", manifest_path.display()))?,
    )?;
    let rest = ArkadeRest::new(&manifest.arkade_service_url);
    let emulator_rest = EmulatorRest::new(&manifest.emulator_url);
    let params = rest.get_info().await.context("read Arkade service info")?;
    let emulator = emulator_rest
        .get_info()
        .await
        .context("read emulator service info")?;
    let validated: ValidatedWorld = manifest
        .validate(&Secp256k1::new(), &params, &emulator)
        .context("validate leaderboard world")?;
    validated.verify_indexed_assets(&rest).await?;
    let mainnet = params.network == bitcoin::Network::Bitcoin;
    let origin = canonical_http_origin(
        &setting("WOODLAND_LEADERBOARD_ORIGIN")?,
        "WOODLAND_LEADERBOARD_ORIGIN",
        mainnet,
    )?;
    let origin = HeaderValue::from_str(&origin).context("encode leaderboard CORS origin")?;
    let leaderboard_url = canonical_http_origin(
        &setting("WOODLAND_LEADERBOARD_PUBLIC_URL")?,
        "WOODLAND_LEADERBOARD_PUBLIC_URL",
        mainnet,
    )?;
    let verifier = Arc::new(Verifier {
        rest,
        params,
        emulator,
        tree_asset: validated.tree_asset,
        log_asset: validated.log_asset,
        xp_asset: validated.xp_asset,
        rollover_signer: validated.rollover_signer,
        genesis_txid: validated.genesis_txid,
        leaderboard_url,
        tree_contract: validated.contract,
    });
    let mut registry = load_registry(&registry_path)?;
    validate_registry_consents(&registry, &verifier)?;
    for entry in registry.players.values_mut() {
        entry.state = None;
    }
    let state = Arc::new(AppState {
        verifier,
        registry: RwLock::new(registry),
        registry_path,
        persist_lock: Mutex::new(()),
        refresh_status: RwLock::new(RefreshStatus::default()),
    });
    if let Err(error) = refresh_all(state.clone()).await {
        eprintln!("woodland.sh leaderboard initial refresh: {error:#}");
    }

    let refresh_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = refresh_all(refresh_state.clone()).await {
                eprintln!("woodland.sh leaderboard refresh: {error:#}");
            }
        }
    });

    let app = Router::new()
        .route("/health.json", get(health))
        .route("/v1/leaderboard", get(leaderboard))
        .route("/v1/players", post(register_player))
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
        .with_context(|| format!("bind leaderboard service at {bind}"))?;
    eprintln!("woodland.sh leaderboard ready at http://{bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve leaderboard")
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
        let message = player::leaderboard_registration_message(
            genesis,
            owner,
            asset,
            "https://leaderboard.example",
        );
        let signature = secp.sign_schnorr_no_aux_rand(&message, &keypair);
        assert!(secp.verify_schnorr(&signature, &message, &owner).is_ok());
        let other_service = player::leaderboard_registration_message(
            genesis,
            owner,
            asset,
            "https://other.example",
        );
        assert!(secp
            .verify_schnorr(&signature, &other_service, &owner)
            .is_err());
    }

    #[test]
    fn registry_round_trips_atomically() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("woodland-leaderboard-{unique}"));
        let path = root.join("registry.json");
        let mut registry = RegistryFile::default();
        registry.players.insert(
            "player".to_owned(),
            RegisteredPlayer {
                owner: "owner".to_owned(),
                player_asset: "player".to_owned(),
                registration_signature: "signature".to_owned(),
                registered_at: 1,
                state: Some(player("player", 7, true, 1)),
            },
        );
        save_registry(&path, &registry).unwrap();
        assert_eq!(load_registry(&path).unwrap().players.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
