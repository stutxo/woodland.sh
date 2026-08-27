//! Browser/WASM application for direct interaction with woodland.sh covenants.

use crate::arkade::{ArkadeRest, EmulatorParams, EmulatorRest, ServerParams, VtxoRecord};
use crate::chop::PendingChop;
use crate::keys::Keys;
use crate::player::{self, PlayerChopTransition, PlayerContract, PlayerState};
use crate::tree::{self, TreeContract, TreeHealth, TreeState};
use crate::txbuild;
use crate::world::{ValidatedTree, ValidatedWorld, WorldManifest};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::packet::{AssetGroup, AssetInput, AssetOutput, AssetRef, Packet};
use ark_core::asset::AssetId;
use ark_core::send::{
    build_offchain_transactions, sign_ark_transaction, sign_checkpoint_transaction, SendReceiver,
    VtxoInput,
};
use bitcoin::{Amount, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

const INDEX_ATTEMPTS: usize = 80;
const INDEX_POLL_MS: u64 = 250;

#[derive(Clone)]
struct World {
    manifest: WorldManifest,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    rollover_signer: bitcoin::XOnlyPublicKey,
    contract: TreeContract,
    genesis_txid: Txid,
    declared_trees: Vec<ValidatedTree>,
}

#[derive(Clone)]
struct LiveTree {
    state: TreeState,
    roll: tree::TreeRoll,
    health: TreeHealth,
    deployment_txid: Txid,
    record: VtxoRecord,
    previous_tx: Transaction,
    last_attempt_txid: Option<Txid>,
}

#[derive(Clone)]
struct LivePlayerState {
    contract: PlayerContract,
    state: PlayerState,
    record: VtxoRecord,
    previous_tx: Transaction,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlayerProfile {
    genesis_txid: String,
    player_asset: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerRegistration {
    owner: String,
    player_asset: String,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerLocationUpdate {
    owner: String,
    player_asset: String,
    timestamp_ms: f64,
    x: u16,
    y: u16,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerChatMessage {
    owner: String,
    player_asset: String,
    timestamp_ms: f64,
    message: String,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerDelegation {
    owner: String,
    player_asset: String,
    timestamp_ms: f64,
    enabled: bool,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessSnapshot {
    address: String,
    server: String,
    emulator: String,
    emulator_version: String,
    emulator_signer: String,
    dust_sats: u64,
    funding_required_sats: u64,
    wallet_sats: u64,
    funding_ready: bool,
    activation_ready: bool,
    activation_blocked_reason: Option<String>,
    player_active: bool,
    player_xp: u64,
    player_level: u64,
    player_next_level_xp: Option<u64>,
    season_xp_remaining: u64,
    player_state_expires_in_seconds: Option<i64>,
    player_rollover_margin_seconds: Option<i64>,
    log_drop_basis_points: u64,
    player_logs: u64,
    player_state_outpoint: Option<String>,
    full_tree_value_sats: u64,
    wallet_vtxos: Vec<WalletVtxoView>,
    map_width: u16,
    map_height: u16,
    player_asset: Option<String>,
    tree_asset: String,
    log_asset: String,
    xp_asset: String,
    genesis_txid: String,
    covenant_script: String,
    pending_chop_txid: Option<String>,
    last_attempt: Option<AttemptView>,
    trees: Vec<TreeView>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttemptView {
    tree_id: u32,
    success: bool,
}

struct ExpectedChop {
    tree_outpoint: String,
    player_state_outpoint: String,
    drop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChopMutation {
    None,
    InvertXp,
    NonCanonicalXp,
    NonCanonicalHealth,
    SwapWorldGroups,
    SwapLogXpGroups,
    ReplaceXpGroup,
    WrongRoll,
    WrongLogDelta,
    WrongXpDelta,
    DoubleTreeMarker,
    WrongPlayerPosition,
    ExtraOutput,
    WrongAnchor,
    AssetMetadata,
    PlayerMarkerMetadata,
    AssetControl,
    FundExtension,
    SubmissionFailure,
}

impl ChopMutation {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "wrong-roll" => Ok(Self::WrongRoll),
            "wrong-log-delta" => Ok(Self::WrongLogDelta),
            "wrong-xp-delta" => Ok(Self::WrongXpDelta),
            "noncanonical-xp-zero" => Ok(Self::NonCanonicalXp),
            "noncanonical-health-zero" => Ok(Self::NonCanonicalHealth),
            "double-tree-marker" => Ok(Self::DoubleTreeMarker),
            "wrong-player-position" => Ok(Self::WrongPlayerPosition),
            "extra-output" => Ok(Self::ExtraOutput),
            "wrong-anchor" => Ok(Self::WrongAnchor),
            "asset-metadata" => Ok(Self::AssetMetadata),
            "player-marker-metadata" => Ok(Self::PlayerMarkerMetadata),
            "asset-control" => Ok(Self::AssetControl),
            "fund-extension" => Ok(Self::FundExtension),
            _ => Err(anyhow!("unknown chop mutation {name}")),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WalletVtxoView {
    outpoint: String,
    amount_sats: u64,
    assets: Vec<AssetView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetView {
    id: String,
    amount: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TreeView {
    tree_id: u32,
    x: u16,
    y: u16,
    health: u64,
    log_reserve_remaining: u64,
    xp_remaining: u64,
    value_sats: u64,
    tree_outpoint: String,
    deployment_txid: String,
    last_attempt_txid: Option<String>,
    next_roll_bucket: u64,
    next_drop: bool,
    expires_in_seconds: Option<i64>,
    respawn_at: Option<i64>,
    respawn_in_seconds: Option<i64>,
}

#[wasm_bindgen]
pub struct WoodlandApp {
    keys: Keys,
    rest: ArkadeRest,
    emulator: EmulatorRest,
    emulator_params: EmulatorParams,
    params: ServerParams,
    info: ark_core::server::Info,
    world: World,
    trees: Vec<LiveTree>,
    profile: PlayerProfile,
    player_state: Option<LivePlayerState>,
    wallet_records: Vec<VtxoRecord>,
    pending_storage_key: String,
    pending_chop: Option<PendingChop>,
    last_attempt: Option<AttemptView>,
}

#[wasm_bindgen]
impl WoodlandApp {
    #[wasm_bindgen(js_name = init)]
    pub async fn init(
        server: String,
        emulator: String,
        manifest_json: String,
        secret_key: Option<String>,
        player_profile: Option<String>,
    ) -> Result<WoodlandApp, JsValue> {
        console_error_panic_hook::set_once();
        let manifest = WorldManifest::from_json(&manifest_json).map_err(js_err)?;
        let server = server.trim_end_matches('/');
        let emulator = emulator.trim_end_matches('/');
        if server != manifest.arkade_service_url || emulator != manifest.emulator_url {
            return Err(JsValue::from_str(
                "woodland.sh service URLs do not match the world manifest",
            ));
        }
        let expected_network = manifest
            .network
            .parse::<bitcoin::Network>()
            .map_err(|_| JsValue::from_str("world manifest has an invalid network"))?;
        let pending_storage_key = format!("woodland.sh:web:v1:pending:{server}");
        let pending_chop = load_pending_chop(&pending_storage_key).map_err(js_err)?;
        let rest = ArkadeRest::new(server);
        let params = rest.get_info().await.map_err(js_err)?;
        if params.network != expected_network {
            return Err(JsValue::from_str(
                "Arkade service network does not match the world manifest",
            ));
        }
        if !params.zero_offchain_fees {
            return Err(JsValue::from_str(
                "woodland.sh requires zero offchain input and output fees",
            ));
        }
        let emulator = EmulatorRest::new(emulator);
        let emulator_params = emulator.get_info().await.map_err(js_err)?;
        let keys = match secret_key.filter(|value| !value.trim().is_empty()) {
            Some(secret) => Keys::from_hex(secret.trim()).map_err(js_err)?,
            None => Keys::generate().map_err(js_err)?,
        };
        let validated = manifest
            .validate(&keys.secp, &params, &emulator_params)
            .map_err(js_err)?;
        validated
            .verify_indexed_assets(&rest)
            .await
            .map_err(js_err)?;
        let ValidatedWorld {
            tree_asset,
            log_asset,
            xp_asset,
            rollover_signer,
            genesis_txid,
            trees: declared_trees,
            contract,
            ..
        } = validated;
        let profile = match player_profile.filter(|value| !value.trim().is_empty()) {
            Some(json) => {
                let profile: PlayerProfile = serde_json::from_str(&json)
                    .context("decode local player profile")
                    .map_err(js_err)?;
                if profile.genesis_txid != genesis_txid.to_string() {
                    return Err(JsValue::from_str(
                        "local player profile belongs to a different world",
                    ));
                }
                if let Some(asset) = profile.player_asset.as_deref() {
                    let asset = asset
                        .parse::<AssetId>()
                        .context("local player profile has an invalid PLAYER_ID")
                        .map_err(js_err)?;
                    if [tree_asset, log_asset, xp_asset].contains(&asset) {
                        return Err(JsValue::from_str(
                            "local PLAYER_ID collides with a world asset",
                        ));
                    }
                }
                profile
            }
            None => PlayerProfile {
                genesis_txid: genesis_txid.to_string(),
                player_asset: None,
            },
        };
        let info = txbuild::server_info(&params);
        Ok(Self {
            keys,
            rest,
            emulator,
            emulator_params,
            params,
            info,
            world: World {
                manifest,
                tree_asset,
                log_asset,
                xp_asset,
                rollover_signer,
                contract,
                genesis_txid,
                declared_trees,
            },
            trees: Vec::new(),
            profile,
            player_state: None,
            wallet_records: Vec::new(),
            pending_storage_key,
            pending_chop,
            last_attempt: None,
        })
    }

    pub fn address(&self) -> String {
        player_vtxo(&self.keys, &self.params)
            .expect("validated player VTXO")
            .to_ark_address()
            .encode()
    }

    #[wasm_bindgen(js_name = exportKey)]
    pub fn export_key(&self) -> String {
        self.keys.secret_hex()
    }

    #[wasm_bindgen(js_name = exportProfile)]
    pub fn export_profile(&self) -> String {
        serde_json::to_string(&self.profile).expect("player profile serialization cannot fail")
    }

    #[wasm_bindgen(js_name = serverRegistration)]
    pub fn server_registration(&self, server_url: String) -> Result<JsValue, JsValue> {
        if server_url.is_empty() {
            return Err(JsValue::from_str("server URL is empty"));
        }
        let player_asset = self.require_player_asset().map_err(js_err)?;
        let owner = self.keys.owner_pk();
        let message = player::server_registration_message(
            self.world.genesis_txid,
            owner,
            player_asset,
            &server_url,
        );
        let signature = self
            .keys
            .secp
            .sign_schnorr_no_aux_rand(&message, &self.keys.keypair);
        serde_wasm_bindgen::to_value(&ServerRegistration {
            owner: owner.to_string(),
            player_asset: player_asset.to_string(),
            signature: signature.to_string(),
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    #[wasm_bindgen(js_name = serverLocation)]
    pub fn server_location(
        &self,
        server_url: String,
        x: u16,
        y: u16,
        timestamp_ms: f64,
    ) -> Result<JsValue, JsValue> {
        if x >= self.world.manifest.map_width || y >= self.world.manifest.map_height {
            return Err(JsValue::from_str(
                "player location is outside the world map",
            ));
        }
        let payload = format!("x={x}\ny={y}\n");
        let (owner, player_asset, signature) = self
            .sign_server_action(
                &server_url,
                player::SERVER_ACTION_LOCATION,
                timestamp_ms,
                &payload,
            )
            .map_err(js_err)?;
        serde_wasm_bindgen::to_value(&ServerLocationUpdate {
            owner,
            player_asset,
            timestamp_ms,
            x,
            y,
            signature,
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    #[wasm_bindgen(js_name = serverChat)]
    pub fn server_chat(
        &self,
        server_url: String,
        message: String,
        timestamp_ms: f64,
    ) -> Result<JsValue, JsValue> {
        let (owner, player_asset, signature) = self
            .sign_server_action(
                &server_url,
                player::SERVER_ACTION_CHAT,
                timestamp_ms,
                &message,
            )
            .map_err(js_err)?;
        serde_wasm_bindgen::to_value(&ServerChatMessage {
            owner,
            player_asset,
            timestamp_ms,
            message,
            signature,
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    #[wasm_bindgen(js_name = serverDelegation)]
    pub fn server_delegation(
        &self,
        server_url: String,
        enabled: bool,
        timestamp_ms: f64,
    ) -> Result<JsValue, JsValue> {
        let payload = format!("enabled={enabled}\n");
        let (owner, player_asset, signature) = self
            .sign_server_action(
                &server_url,
                player::SERVER_ACTION_DELEGATION,
                timestamp_ms,
                &payload,
            )
            .map_err(js_err)?;
        serde_wasm_bindgen::to_value(&ServerDelegation {
            owner,
            player_asset,
            timestamp_ms,
            enabled,
            signature,
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    #[wasm_bindgen(js_name = exportPendingChop)]
    pub fn export_pending_chop(&self) -> Option<String> {
        self.pending_chop
            .as_ref()
            .and_then(|pending| serde_json::to_string(pending).ok())
    }

    #[wasm_bindgen(js_name = resumePendingChop)]
    pub async fn resume_pending_chop(&mut self) -> Result<JsValue, JsValue> {
        self.resume_pending_chop_inner().await.map_err(js_err)?;
        self.refresh().await
    }

    pub async fn activate(&mut self) -> Result<JsValue, JsValue> {
        self.activate_inner().await.map_err(js_err)?;
        self.refresh().await
    }
    #[wasm_bindgen(js_name = renewPlayer)]
    pub async fn renew_player(&mut self) -> Result<JsValue, JsValue> {
        self.renew_player_inner().await.map_err(js_err)?;
        self.refresh().await
    }

    pub async fn refresh(&mut self) -> Result<JsValue, JsValue> {
        self.sync().await.map_err(js_err)?;
        serde_wasm_bindgen::to_value(&self.snapshot())
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    pub async fn chop(&mut self, tree_id: u32) -> Result<JsValue, JsValue> {
        let success = self.chop_inner(tree_id).await.map_err(js_err)?;
        self.last_attempt = Some(AttemptView { tree_id, success });
        self.refresh().await
    }

    #[wasm_bindgen(js_name = chopExpected)]
    pub async fn chop_expected(
        &mut self,
        tree_id: u32,
        expected_tree_outpoint: String,
        expected_player_state_outpoint: String,
        expected_drop: bool,
    ) -> Result<JsValue, JsValue> {
        let expected = ExpectedChop {
            tree_outpoint: expected_tree_outpoint,
            player_state_outpoint: expected_player_state_outpoint,
            drop: expected_drop,
        };
        let success = self
            .chop_inner_with_options(tree_id, ChopMutation::None, Some(expected))
            .await
            .map_err(js_err)?;
        self.last_attempt = Some(AttemptView { tree_id, success });
        self.refresh().await
    }

    #[cfg(feature = "regtest-e2e")]
    #[wasm_bindgen(js_name = testInvalidXpTransition)]
    pub async fn test_invalid_xp_transition(&mut self, tree_id: u32) -> Result<(), JsValue> {
        self.invalid_xp_transition_probe(tree_id)
            .await
            .map_err(js_err)
    }

    #[cfg(feature = "regtest-e2e")]
    #[wasm_bindgen(js_name = testInvalidAssetGroupOrder)]
    pub async fn test_invalid_asset_group_order(&mut self, tree_id: u32) -> Result<(), JsValue> {
        self.invalid_asset_group_order_probe(tree_id)
            .await
            .map_err(js_err)
    }

    #[cfg(feature = "regtest-e2e")]
    #[wasm_bindgen(js_name = testInvalidXpGroup)]
    pub async fn test_invalid_xp_group(&mut self, tree_id: u32) -> Result<(), JsValue> {
        self.invalid_xp_group_probe(tree_id).await.map_err(js_err)
    }

    #[cfg(feature = "regtest-e2e")]
    #[wasm_bindgen(js_name = testInvalidLogXpGroupOrder)]
    pub async fn test_invalid_log_xp_group_order(&mut self, tree_id: u32) -> Result<(), JsValue> {
        self.invalid_log_xp_group_order_probe(tree_id)
            .await
            .map_err(js_err)
    }

    #[cfg(feature = "regtest-e2e")]
    #[wasm_bindgen(js_name = testChopMutation)]
    pub async fn test_chop_mutation(
        &mut self,
        tree_id: u32,
        mutation: String,
    ) -> Result<(), JsValue> {
        let mutation = ChopMutation::parse(&mutation).map_err(js_err)?;
        self.rejected_chop_mutation_probe(tree_id, mutation)
            .await
            .map_err(js_err)
    }

    #[cfg(feature = "regtest-e2e")]
    #[wasm_bindgen(js_name = testSubmissionRecovery)]
    pub async fn test_submission_recovery(&mut self, tree_id: u32) -> Result<JsValue, JsValue> {
        let success = self
            .chop_inner_with_options(tree_id, ChopMutation::SubmissionFailure, None)
            .await
            .map_err(js_err)?;
        self.last_attempt = Some(AttemptView { tree_id, success });
        self.refresh().await
    }
}

impl WoodlandApp {
    fn sign_server_action(
        &self,
        server_url: &str,
        action: &str,
        timestamp_ms: f64,
        payload: &str,
    ) -> Result<(String, String, String)> {
        if server_url.is_empty() {
            return Err(anyhow!("server URL is empty"));
        }
        if !timestamp_ms.is_finite()
            || timestamp_ms < 0.0
            || timestamp_ms.fract() != 0.0
            || timestamp_ms > 9_007_199_254_740_991.0
        {
            return Err(anyhow!("server action timestamp is invalid"));
        }
        let timestamp_ms = timestamp_ms as u64;
        let player_asset = self.require_player_asset()?;
        let owner = self.keys.owner_pk();
        let message = player::server_action_message(
            self.world.genesis_txid,
            owner,
            player_asset,
            server_url,
            action,
            timestamp_ms,
            payload,
        );
        let signature = self
            .keys
            .secp
            .sign_schnorr_no_aux_rand(&message, &self.keys.keypair);
        Ok((
            owner.to_string(),
            player_asset.to_string(),
            signature.to_string(),
        ))
    }

    fn snapshot(&self) -> HarnessSnapshot {
        let now = crate::arkade::now_unix();
        let player_xp = self
            .player_state
            .as_ref()
            .map(|player| player.state.xp.value())
            .unwrap_or(0);
        let wallet_sats = self
            .wallet_records
            .iter()
            .fold(0_u64, |sum, record| sum.saturating_add(record.amount_sats))
            .saturating_add(
                self.player_state
                    .as_ref()
                    .map_or(0, |player| player.record.amount_sats),
            );
        let wallet_vtxos = self
            .wallet_records
            .iter()
            .map(|record| WalletVtxoView {
                outpoint: record.outpoint.to_string(),
                amount_sats: record.amount_sats,
                assets: record
                    .assets
                    .iter()
                    .map(|asset| AssetView {
                        id: asset.asset_id.to_string(),
                        amount: asset.amount,
                    })
                    .collect(),
            })
            .collect();
        let trees = self
            .trees
            .iter()
            .map(|tree| {
                let logs = tree.record.asset_amount(self.world.log_asset).unwrap_or(0);
                let xp_balance = tree.record.asset_amount(self.world.xp_asset).unwrap_or(0);
                let (next_roll, raw_drop) = tree.roll.advance(player_xp);
                let next_drop = raw_drop && tree.health.value() > 0 && logs > 0 && xp_balance > 0;
                let can_regrow = tree.health.value() == 0
                    && logs >= self.world.manifest.active_logs_per_tree
                    && xp_balance >= self.world.manifest.active_logs_per_tree;
                TreeView {
                    tree_id: tree.state.tree_id,
                    x: tree.state.x,
                    y: tree.state.y,
                    health: tree.health.value(),
                    log_reserve_remaining: logs,
                    xp_remaining: xp_balance,
                    value_sats: tree.record.amount_sats,
                    tree_outpoint: tree.record.outpoint.to_string(),
                    deployment_txid: tree.deployment_txid.to_string(),
                    last_attempt_txid: tree.last_attempt_txid.map(|txid| txid.to_string()),
                    next_roll_bucket: next_roll.bucket(),
                    next_drop,
                    expires_in_seconds: tree.record.expires_in(now),
                    respawn_at: can_regrow
                        .then(|| {
                            tree::respawn_at(
                                tree.state,
                                tree.record.outpoint,
                                tree.record.created_at,
                            )
                        })
                        .transpose()
                        .ok()
                        .flatten(),
                    respawn_in_seconds: can_regrow
                        .then(|| {
                            tree::respawn_at(
                                tree.state,
                                tree.record.outpoint,
                                tree.record.created_at,
                            )
                            .map(|deadline| (deadline - now).max(0))
                        })
                        .transpose()
                        .ok()
                        .flatten(),
                }
            })
            .collect();
        let player_level = player::level_from_xp(player_xp);
        let player_state_expires_in_seconds = self
            .player_state
            .as_ref()
            .and_then(|player| player.record.expires_in(now));
        let player_rollover_margin_seconds = self
            .player_state
            .as_ref()
            .map(|player| player.record.rollover_margin_seconds());
        let activation_sats = self.params.dust_sats;
        let clean_activation_funding = self
            .wallet_records
            .iter()
            .any(|record| record.assets.is_empty() && record.amount_sats == activation_sats);
        let activation_ready = self.player_state.is_none() && clean_activation_funding;
        let activation_blocked_reason = None;
        HarnessSnapshot {
            address: self.address(),
            server: self.rest.base().to_string(),
            emulator: self.emulator.base().to_string(),
            emulator_version: self.emulator_params.version.clone(),
            emulator_signer: self.emulator_params.signer_pk.to_string(),
            dust_sats: self.params.dust_sats,
            funding_required_sats: if self.player_state.is_none() && !clean_activation_funding {
                activation_sats.saturating_sub(wallet_sats)
            } else {
                0
            },
            wallet_sats,
            funding_ready: self.player_state.is_some() && self.pending_chop.is_none(),
            activation_ready,
            activation_blocked_reason,
            player_active: self.player_state.is_some(),
            player_xp,
            player_level,
            player_next_level_xp: player::xp_for_level(player_level.saturating_add(1)),
            season_xp_remaining: self.trees.iter().fold(0_u64, |total, tree| {
                total.saturating_add(tree.record.asset_amount(self.world.xp_asset).unwrap_or(0))
            }),
            player_state_expires_in_seconds,
            player_rollover_margin_seconds,
            log_drop_basis_points: tree::log_drop_basis_points(player_xp),
            player_logs: self
                .player_state
                .as_ref()
                .and_then(|player| player.record.asset_amount(self.world.log_asset))
                .unwrap_or(0),
            player_state_outpoint: self
                .player_state
                .as_ref()
                .map(|player| player.record.outpoint.to_string()),
            full_tree_value_sats: tree::full_tree_value_sats(self.params.dust_sats)
                .expect("validated full tree value"),
            wallet_vtxos,
            map_width: self.world.manifest.map_width,
            map_height: self.world.manifest.map_height,
            player_asset: self.profile.player_asset.clone(),
            tree_asset: self.world.tree_asset.to_string(),
            log_asset: self.world.log_asset.to_string(),
            xp_asset: self.world.xp_asset.to_string(),
            genesis_txid: self.world.genesis_txid.to_string(),
            covenant_script: self.world.contract.vtxo.script_pubkey().to_hex_string(),
            pending_chop_txid: self
                .pending_chop
                .as_ref()
                .map(|pending| pending.expected_txid.clone()),
            last_attempt: self.last_attempt,
            trees,
        }
    }

    async fn sync(&mut self) -> Result<()> {
        let wallet = player_vtxo(&self.keys, &self.params)?;
        let wallet_script = wallet.script_pubkey().to_hex_string();
        let tree_script = self.world.contract.vtxo.script_pubkey().to_hex_string();
        let player_contract = self.player_contract()?;
        let player_asset = self.player_asset()?;
        let tree_records = wait_for_complete_tree_records(
            &self.rest,
            &tree_script,
            &self.world,
            self.world.manifest.dust_sats,
        )
        .await?;
        let wallet_records = self.rest.get_vtxos(&wallet_script, "spendableOnly").await?;
        let player_state_record = match player_asset {
            Some(player_asset) => {
                let records = self
                    .rest
                    .get_vtxos(
                        &player_contract.vtxo.script_pubkey().to_hex_string(),
                        "spendableOnly",
                    )
                    .await?;
                select_player_state_record(&records, &player_contract, player_asset)?
            }
            None => None,
        };

        let mut txids: Vec<_> = tree_records
            .iter()
            .map(|record| record.outpoint.txid)
            .collect();
        if let Some(record) = &player_state_record {
            txids.push(record.outpoint.txid);
        }
        let transactions = wait_for_virtual_txs(&self.rest, &txids).await?;
        let mut discovered = Vec::with_capacity(tree_records.len());
        let mut seen_states = std::collections::HashMap::new();
        for record in tree_records {
            let previous_tx = transactions
                .get(&record.outpoint.txid)
                .cloned()
                .ok_or_else(|| anyhow!("missing current tree transaction"))?;
            record.validate_creating_transaction(&previous_tx)?;
            let state = tree::tree_state_from_tx(&previous_tx)?
                .ok_or_else(|| anyhow!("current tree transaction has no state packet"))?;
            let roll = tree::tree_roll_from_tx(&previous_tx)?
                .ok_or_else(|| anyhow!("current tree transaction has no roll packet"))?;
            let health = tree::tree_health_from_tx(&previous_tx)?
                .ok_or_else(|| anyhow!("current tree transaction has no health packet"))?;
            let declared = self
                .world
                .declared_trees
                .iter()
                .find(|tree| tree.state == state)
                .copied()
                .ok_or_else(|| anyhow!("current tree has an unknown world identity"))?;
            let is_deployment = record.outpoint.txid == declared.deployment_txid;
            let expected_vout = if is_deployment {
                0
            } else {
                match previous_tx.input.len() {
                    1 => u32::from(crate::protocol::RENEWAL_STATE_OUTPUT_INDEX),
                    crate::protocol::CHOP_INPUT_COUNT => {
                        u32::from(crate::protocol::TREE_OUTPUT_INDEX)
                    }
                    _ => return Err(anyhow!("current tree transaction has an invalid shape")),
                }
            };
            if record.outpoint.vout != expected_vout
                || (is_deployment && roll != tree::TreeRoll::initial(state))
                || (is_deployment && health.value() != self.world.manifest.active_logs_per_tree)
            {
                return Err(anyhow!("current tree record has an invalid lineage"));
            }
            if let Some(competing) = seen_states.insert(state, record.outpoint) {
                return Err(anyhow!(
                    "tree {} exposes competing spendable lineages {competing} and {}",
                    state.tree_id,
                    record.outpoint
                ));
            }
            discovered.push(LiveTree {
                state,
                roll,
                health,
                deployment_txid: declared.deployment_txid,
                last_attempt_txid: (!is_deployment
                    && previous_tx.input.len() == crate::protocol::CHOP_INPUT_COUNT)
                    .then_some(record.outpoint.txid),
                record,
                previous_tx,
            });
        }
        if seen_states.len() != self.world.declared_trees.len() {
            return Err(anyhow!("not all world trees are currently discoverable"));
        }
        discovered.sort_by_key(|tree| {
            self.world
                .declared_trees
                .iter()
                .position(|declared| declared.state == tree.state)
                .unwrap_or(usize::MAX)
        });
        self.trees = discovered;

        self.player_state = match player_state_record {
            Some(record) => {
                let previous_tx = transactions
                    .get(&record.outpoint.txid)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing current player-state transaction"))?;
                record.validate_creating_transaction(&previous_tx)?;
                let state = player::player_state_from_tx(&previous_tx)?
                    .ok_or_else(|| anyhow!("current player VTXO has no state packets"))?;
                player::validate_player_state_record(
                    &record,
                    &player_contract,
                    player_asset.expect("state selection requires PLAYER_ID"),
                )?;

                let expected_identity =
                    player::derive_player_identity(self.keys.owner_pk(), self.world.genesis_txid);
                let xp_balance = record.asset_amount(self.world.xp_asset).unwrap_or(0);
                if state.identity != expected_identity || state.xp.value() != xp_balance {
                    return Err(anyhow!(
                        "current player state has invalid identity or XP backing"
                    ));
                }
                Some(LivePlayerState {
                    contract: player_contract,
                    state,
                    record,
                    previous_tx,
                })
            }
            None => None,
        };
        self.wallet_records = wallet_records;
        self.reconcile_pending_chop()?;
        Ok(())
    }

    fn player_contract(&self) -> Result<PlayerContract> {
        player::build_player_contract(
            &self.keys.secp,
            self.keys.owner_pk(),
            self.params.signer_pk,
            self.emulator_params.signer_pk,
            self.world.rollover_signer,
            self.params.unilateral_exit_delay,
            self.params.network,
            self.world.tree_asset,
            self.world.log_asset,
            self.world.xp_asset,
            self.params.dust_sats,
            &self.world.contract.vtxo.script_pubkey(),
        )
    }

    fn player_asset(&self) -> Result<Option<AssetId>> {
        self.profile
            .player_asset
            .as_deref()
            .map(|asset| {
                asset
                    .parse::<AssetId>()
                    .context("local player profile has an invalid PLAYER_ID")
            })
            .transpose()
    }

    fn require_player_asset(&self) -> Result<AssetId> {
        self.player_asset()?
            .ok_or_else(|| anyhow!("player activation has not issued a PLAYER_ID"))
    }

    fn persist_pending_chop(&mut self, pending: PendingChop) -> Result<()> {
        let json = pending.to_json()?;
        browser_storage()?
            .set_item(&self.pending_storage_key, &json)
            .map_err(|error| anyhow!("persist pending chop journal: {error:?}"))?;
        self.pending_chop = Some(pending);
        Ok(())
    }

    fn clear_pending_chop(&mut self) -> Result<()> {
        browser_storage()?
            .remove_item(&self.pending_storage_key)
            .map_err(|error| anyhow!("clear pending chop journal: {error:?}"))?;
        self.pending_chop = None;
        Ok(())
    }

    fn reconcile_pending_chop(&mut self) -> Result<()> {
        let Some(pending) = self.pending_chop.clone() else {
            return Ok(());
        };
        let txid = pending.txid()?;
        let expected_state = OutPoint {
            txid,
            vout: u32::from(crate::protocol::PLAYER_STATE_OUTPUT_INDEX),
        };
        let expected_tree = OutPoint {
            txid,
            vout: u32::from(crate::protocol::TREE_OUTPUT_INDEX),
        };
        let accepted = self
            .player_state
            .as_ref()
            .is_some_and(|state| state.record.outpoint == expected_state)
            && self.trees.iter().any(|tree| {
                tree.state.tree_id == pending.tree_id && tree.record.outpoint == expected_tree
            });
        if accepted {
            self.last_attempt = Some(AttemptView {
                tree_id: pending.tree_id,
                success: pending.success,
            });
            self.clear_pending_chop()?;
            return Ok(());
        }
        let selected_state = pending.player_state_input()?;
        let selected_tree = pending.tree_input()?;
        let state_conflicted = self.player_state.as_ref().is_some_and(|state| {
            state.record.outpoint != selected_state && state.record.outpoint != expected_state
        });
        let tree_conflicted = self
            .trees
            .iter()
            .find(|tree| tree.state.tree_id == pending.tree_id)
            .is_some_and(|tree| {
                tree.record.outpoint != selected_tree && tree.record.outpoint != expected_tree
            });
        if state_conflicted || tree_conflicted {
            self.clear_pending_chop()?;
        }
        Ok(())
    }

    async fn resume_pending_chop_inner(&mut self) -> Result<bool> {
        let pending_before_sync = self.pending_chop.clone();
        self.sync().await?;
        let Some(pending) = self.pending_chop.clone() else {
            let Some(previously_pending) = pending_before_sync else {
                return Ok(false);
            };
            let expected_txid = previously_pending.txid()?;
            let accepted = self
                .player_state
                .as_ref()
                .is_some_and(|state| state.record.outpoint.txid == expected_txid)
                && self
                    .trees
                    .iter()
                    .find(|tree| tree.state.tree_id == previously_pending.tree_id)
                    .is_some_and(|tree| tree.record.outpoint.txid == expected_txid);
            return Ok(accepted);
        };
        let contract = self.player_contract()?;
        let (expected_ark, expected_checkpoints) = pending.decode_psbts()?;
        let (returned_ark, returned_checkpoints) = self
            .emulator
            .submit_tx(&expected_ark, &expected_checkpoints)
            .await
            .context("resume pending chop through emulator")?;
        player::verify_player_chop_response(
            &self.keys,
            &contract,
            &self.world.contract,
            &expected_ark,
            &expected_checkpoints,
            &returned_ark,
            returned_checkpoints,
        )?;

        let txid = pending.txid()?;
        wait_for_vtxo(
            &self.rest,
            &contract.vtxo.script_pubkey().to_hex_string(),
            OutPoint {
                txid,
                vout: u32::from(crate::protocol::PLAYER_STATE_OUTPUT_INDEX),
            },
        )
        .await?;
        wait_for_vtxo(
            &self.rest,
            &self.world.contract.vtxo.script_pubkey().to_hex_string(),
            OutPoint {
                txid,
                vout: u32::from(crate::protocol::TREE_OUTPUT_INDEX),
            },
        )
        .await?;
        self.sync().await?;
        if self.pending_chop.is_some() {
            return Err(anyhow!(
                "pending chop {txid} was submitted but did not reconcile"
            ));
        }
        Ok(true)
    }

    async fn activate_inner(&mut self) -> Result<()> {
        self.sync().await?;
        if self.player_state.is_some() {
            return Ok(());
        }
        let funding = self
            .wallet_records
            .iter()
            .filter(|record| {
                record.assets.is_empty() && record.amount_sats == self.params.dust_sats
            })
            .cloned()
            .collect::<Vec<_>>();
        let funding = match funding.as_slice() {
            [record] => record.clone(),
            [] => return Err(anyhow!("deposit one exact dust-sized VTXO first")),
            _ => return Err(anyhow!("multiple activation VTXOs are available")),
        };
        let wallet = player_vtxo(&self.keys, &self.params)?;
        let contract = self.player_contract()?;
        let inputs = [txbuild::vtxo_input(&funding, &wallet)?];
        let mut activation = build_offchain_transactions(
            &[SendReceiver::bitcoin(
                contract.vtxo.to_ark_address(),
                Amount::from_sat(self.params.dust_sats),
            )],
            &wallet.to_ark_address(),
            &inputs,
            &self.info,
        )
        .map_err(|error| anyhow!("build permissionless player activation: {error}"))?;
        ark_core::asset::packet::add_asset_packet_to_psbt(
            &mut activation.ark_tx,
            &Packet {
                groups: vec![AssetGroup {
                    asset_id: None,
                    control_asset: None,
                    metadata: Some(vec![
                        ("game".to_owned(), crate::world::GAME_ID.to_owned()),
                        (
                            "protocol".to_owned(),
                            crate::world::PROTOCOL_VERSION.to_string(),
                        ),
                        ("asset".to_owned(), "PLAYER_ID".to_owned()),
                        ("owner".to_owned(), self.keys.owner_pk().to_string()),
                    ]),
                    inputs: Vec::new(),
                    outputs: vec![AssetOutput {
                        output_index: crate::protocol::ACTIVATION_STATE_OUTPUT_INDEX,
                        amount: 1,
                    }],
                }],
            },
        )
        .map_err(|error| anyhow!("attach PLAYER_ID issuance packet: {error}"))?;
        player::attach_player_state_packets(
            &mut activation.ark_tx,
            PlayerState {
                identity: player::derive_player_identity(
                    self.keys.owner_pk(),
                    self.world.genesis_txid,
                ),
                position: player::PlayerPosition {
                    x: crate::world::PLAYER_SPAWN_X,
                    y: crate::world::PLAYER_SPAWN_Y,
                },
                xp: player::PlayerXp::new(0),
            },
        )?;
        if activation.ark_tx.unsigned_tx.output.len() != crate::protocol::ACTIVATION_OUTPUT_COUNT {
            return Err(anyhow!("activation transaction has an invalid shape"));
        }
        let txid = activation.ark_tx.unsigned_tx.compute_txid();
        let player_asset = AssetId {
            txid,
            group_index: crate::protocol::PLAYER_ID_ASSET_GROUP_INDEX as u16,
        };
        if let Some(expected) = self.player_asset()? {
            if expected != player_asset {
                return Err(anyhow!(
                    "activation retry does not reproduce the pending PLAYER_ID"
                ));
            }
        }
        self.profile.player_asset = Some(player_asset.to_string());
        txbuild::run_tx(
            &self.keys,
            &self.rest,
            activation.ark_tx,
            activation.checkpoint_txs,
        )
        .await
        .context("submit permissionless player activation")?;
        wait_for_vtxo(
            &self.rest,
            &contract.vtxo.script_pubkey().to_hex_string(),
            OutPoint {
                txid,
                vout: u32::from(crate::protocol::ACTIVATION_STATE_OUTPUT_INDEX),
            },
        )
        .await?;
        self.sync().await?;
        if self.player_state.is_none() {
            return Err(anyhow!("activated player state was not discovered"));
        }
        Ok(())
    }
    async fn renew_player_inner(&mut self) -> Result<()> {
        self.sync().await?;
        let state = self
            .player_state
            .clone()
            .ok_or_else(|| anyhow!("activate the player before renewal"))?;
        let old_expires_at = state
            .record
            .expires_at
            .ok_or_else(|| anyhow!("player state has no indexed expiry"))?;
        let player_asset = self.require_player_asset()?;
        let prepared = crate::renewal::prepare_player(
            &state.record,
            &state.previous_tx,
            &state.contract,
            player_asset,
        )?;
        let cosigner = Keys::generate()?;
        let prepared = crate::renewal::bind(
            &self.keys,
            prepared,
            &state.previous_tx,
            cosigner.keypair.public_key(),
        )?;
        let approved = crate::renewal::approve(
            &self.keys,
            &self.emulator,
            self.emulator_params.signer_pk,
            prepared,
        )
        .await?;
        let services = crate::batch::BatchServices::connect(
            &self.world.manifest.arkade_service_url,
            self.emulator.clone(),
            self.params.clone(),
        )
        .await?;
        let outcome = crate::batch::join_batch_with_intent(
            &services,
            &self.keys,
            &cosigner,
            self.emulator_params.signer_pk,
            &approved,
        )
        .await?;
        let renewed = wait_for_vtxo(
            &self.rest,
            &state.contract.vtxo.script_pubkey().to_hex_string(),
            outcome.outpoint,
        )
        .await?;
        let new_expires_at = renewed
            .expires_at
            .ok_or_else(|| anyhow!("renewed player state has no indexed expiry"))?;
        if new_expires_at <= old_expires_at {
            return Err(anyhow!(
                "player renewal did not extend expiry ({old_expires_at} -> {new_expires_at})"
            ));
        }
        self.sync().await?;
        Ok(())
    }

    async fn invalid_xp_transition_probe(&mut self, tree_id: u32) -> Result<()> {
        self.rejected_chop_mutation_probe(tree_id, ChopMutation::InvertXp)
            .await
    }

    async fn invalid_asset_group_order_probe(&mut self, tree_id: u32) -> Result<()> {
        self.rejected_chop_mutation_probe(tree_id, ChopMutation::SwapWorldGroups)
            .await
    }

    async fn invalid_xp_group_probe(&mut self, tree_id: u32) -> Result<()> {
        self.rejected_chop_mutation_probe(tree_id, ChopMutation::ReplaceXpGroup)
            .await
    }

    async fn invalid_log_xp_group_order_probe(&mut self, tree_id: u32) -> Result<()> {
        self.rejected_chop_mutation_probe(tree_id, ChopMutation::SwapLogXpGroups)
            .await
    }

    async fn rejected_chop_mutation_probe(
        &mut self,
        tree_id: u32,
        mutation: ChopMutation,
    ) -> Result<()> {
        match self.chop_inner_with_options(tree_id, mutation, None).await {
            Err(error) => {
                let message = format!("{error:#}").to_ascii_lowercase();
                let emulator_rejected = error
                    .chain()
                    .filter_map(|cause| cause.downcast_ref::<crate::arkade::HttpFailure>())
                    .any(|failure| failure.status_code() == Some(500));
                if message.contains("submit chop to emulator") && emulator_rejected {
                    Ok(())
                } else {
                    Err(error.context(format!(
                        "chop mutation {mutation:?} failed for an unexpected reason"
                    )))
                }
            }
            Ok(_) => Err(anyhow!(
                "emulator accepted invalid chop mutation {mutation:?}"
            )),
        }
    }

    async fn chop_inner(&mut self, tree_id: u32) -> Result<bool> {
        self.chop_inner_with_options(tree_id, ChopMutation::None, None)
            .await
    }

    async fn chop_inner_with_options(
        &mut self,
        tree_id: u32,
        mutation: ChopMutation,
        expected: Option<ExpectedChop>,
    ) -> Result<bool> {
        self.sync().await?;
        if let Some(pending) = &self.pending_chop {
            return Err(anyhow!(
                "pending chop {} must reconcile before another swing",
                pending.expected_txid
            ));
        }
        let state = self
            .player_state
            .clone()
            .ok_or_else(|| anyhow!("activate the player before chopping"))?;
        let player_asset = self.require_player_asset()?;
        let tree_index = self
            .trees
            .iter()
            .position(|tree| tree.state.tree_id == tree_id)
            .ok_or_else(|| anyhow!("tree {tree_id} is not part of this world"))?;
        let tree = self.trees[tree_index].clone();
        let player_logs_before = state.record.asset_amount(self.world.log_asset).unwrap_or(0);
        let player_xp_balance_before = state.record.asset_amount(self.world.xp_asset).unwrap_or(0);
        if state.state.xp.value() != player_xp_balance_before {
            return Err(anyhow!("player XP counter is not backed by the XP asset"));
        }
        require_asset_amount(&state.record, player_asset, 1, "PLAYER_ID")?;
        let tree_logs_before = require_nonzero_asset(
            &tree.record,
            self.world.log_asset,
            "tree has no LOG reserve",
        )?;
        let tree_xp_balance_before =
            require_nonzero_asset(&tree.record, self.world.xp_asset, "tree has no XP")?;
        if tree.health.value() == 0 {
            return Err(anyhow!("tree is a stump awaiting regrowth"));
        }
        require_asset_amount(&tree.record, self.world.tree_asset, 1, "tree marker")?;
        let tree_value = tree::full_tree_value_sats(self.params.dust_sats)?;
        if tree.record.amount_sats != tree_value {
            return Err(anyhow!("tree does not retain its fixed value"));
        }
        let now = crate::arkade::now_unix();
        for (record, label) in [(&state.record, "player state"), (&tree.record, "tree")] {
            record
                .ensure_live(now, crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS)
                .with_context(|| format!("{label} input"))?;
        }
        let (next_tree_roll, success) = tree.roll.advance(state.state.xp.value());
        if let Some(expected) = expected {
            if tree.record.outpoint.to_string() != expected.tree_outpoint
                || state.record.outpoint.to_string() != expected.player_state_outpoint
                || success != expected.drop
            {
                return Err(anyhow!(
                    "chop precondition changed; refresh before selecting another action"
                ));
            }
        }
        let reward = u64::from(success);
        let log_reward = if matches!(mutation, ChopMutation::WrongLogDelta) {
            1 - reward
        } else {
            reward
        };
        let xp_reward = if matches!(mutation, ChopMutation::WrongXpDelta) {
            1 - reward
        } else {
            reward
        };
        let tree_logs_after = tree_logs_before - log_reward;
        let player_logs_after = player_logs_before
            .checked_add(log_reward)
            .ok_or_else(|| anyhow!("player LOG balance overflow"))?;
        let tree_xp_balance_after = tree_xp_balance_before - xp_reward;
        let player_xp_balance_after = player_xp_balance_before
            .checked_add(xp_reward)
            .ok_or_else(|| anyhow!("player XP balance overflow"))?;
        let next_xp = if success {
            state.state.xp.increment()?
        } else {
            state.state.xp
        };
        let next_state = PlayerState {
            xp: next_xp,
            ..state.state
        };

        let inputs = [
            player::player_state_vtxo_input(&state.record, &state.contract, player_asset)?,
            tree_vtxo_input(&tree.record, &self.world.contract)?,
        ];
        let mut chop = build_offchain_transactions(
            &[
                SendReceiver::bitcoin(
                    state.contract.vtxo.to_ark_address(),
                    Amount::from_sat(self.params.dust_sats),
                ),
                SendReceiver::bitcoin(
                    self.world.contract.vtxo.to_ark_address(),
                    Amount::from_sat(tree_value),
                ),
            ],
            &state.contract.vtxo.to_ark_address(),
            &inputs,
            &self.info,
        )
        .map_err(|error| anyhow!("build chop transaction: {error}"))?;
        if chop.ark_tx.unsigned_tx.output.len()
            != crate::protocol::CHOP_OUTPUT_COUNT_BEFORE_EXTENSION
        {
            return Err(anyhow!("chop builder produced an unexpected change output"));
        }

        let mut log_inputs = Vec::new();
        if player_logs_before > 0 {
            log_inputs.push((
                crate::protocol::PLAYER_STATE_INPUT_INDEX as u16,
                player_logs_before,
            ));
        }
        log_inputs.push((crate::protocol::TREE_INPUT_INDEX as u16, tree_logs_before));
        let mut log_outputs = Vec::new();
        if player_logs_after > 0 {
            log_outputs.push((
                crate::protocol::PLAYER_STATE_OUTPUT_INDEX,
                player_logs_after,
            ));
        }
        if tree_logs_after > 0 {
            log_outputs.push((crate::protocol::TREE_OUTPUT_INDEX, tree_logs_after));
        }
        let mut xp_inputs = Vec::new();
        if player_xp_balance_before > 0 {
            xp_inputs.push((
                crate::protocol::PLAYER_STATE_INPUT_INDEX as u16,
                player_xp_balance_before,
            ));
        }
        xp_inputs.push((
            crate::protocol::TREE_INPUT_INDEX as u16,
            tree_xp_balance_before,
        ));
        let mut xp_outputs = Vec::new();
        if player_xp_balance_after > 0 {
            xp_outputs.push((
                crate::protocol::PLAYER_STATE_OUTPUT_INDEX,
                player_xp_balance_after,
            ));
        }
        if tree_xp_balance_after > 0 {
            xp_outputs.push((crate::protocol::TREE_OUTPUT_INDEX, tree_xp_balance_after));
        }
        let mut groups = vec![
            transfer_group(
                player_asset,
                vec![(crate::protocol::PLAYER_STATE_INPUT_INDEX as u16, 1)],
                vec![(crate::protocol::PLAYER_STATE_OUTPUT_INDEX, 1)],
            ),
            transfer_group(
                self.world.tree_asset,
                vec![(crate::protocol::TREE_INPUT_INDEX as u16, 1)],
                vec![(crate::protocol::TREE_OUTPUT_INDEX, 1)],
            ),
            transfer_group(self.world.log_asset, log_inputs, log_outputs),
            transfer_group(self.world.xp_asset, xp_inputs, xp_outputs),
        ];
        if matches!(mutation, ChopMutation::PlayerMarkerMetadata) {
            groups[crate::protocol::PLAYER_ID_ASSET_GROUP_INDEX].metadata =
                Some(vec![("forged".to_owned(), "metadata".to_owned())]);
        }
        if matches!(mutation, ChopMutation::AssetMetadata) {
            groups[crate::protocol::LOG_ASSET_GROUP_INDEX].metadata =
                Some(vec![("forged".to_owned(), "metadata".to_owned())]);
        }
        if matches!(mutation, ChopMutation::AssetControl) {
            groups[crate::protocol::LOG_ASSET_GROUP_INDEX].control_asset =
                Some(AssetRef::ById(self.world.tree_asset));
        }
        if matches!(mutation, ChopMutation::SwapWorldGroups) {
            groups.swap(
                crate::protocol::TREE_ASSET_GROUP_INDEX,
                crate::protocol::LOG_ASSET_GROUP_INDEX,
            );
        }
        if matches!(mutation, ChopMutation::SwapLogXpGroups) {
            groups.swap(
                crate::protocol::LOG_ASSET_GROUP_INDEX,
                crate::protocol::XP_ASSET_GROUP_INDEX,
            );
        }
        if matches!(mutation, ChopMutation::ReplaceXpGroup) {
            groups[crate::protocol::XP_ASSET_GROUP_INDEX].asset_id = Some(AssetId {
                txid: self.world.tree_asset.txid,
                group_index: u16::MAX,
            });
        }
        if matches!(mutation, ChopMutation::DoubleTreeMarker) {
            groups[crate::protocol::TREE_ASSET_GROUP_INDEX].outputs[0].amount = 2;
        }
        ark_core::asset::packet::add_asset_packet_to_psbt(&mut chop.ark_tx, &Packet { groups })
            .map_err(|error| anyhow!("attach chop asset packet: {error}"))?;
        player::attach_player_chop_context(
            &mut chop.ark_tx,
            &chop.checkpoint_txs,
            &state.contract,
            &self.world.contract,
            [&state.previous_tx, &tree.previous_tx],
            next_state,
            next_tree_roll,
        )?;
        if matches!(mutation, ChopMutation::InvertXp) {
            let wrong_xp = if success {
                state.state.xp
            } else {
                state.state.xp.increment()?
            };
            replace_extension_packet(
                &mut chop.ark_tx,
                crate::protocol::PLAYER_XP_PACKET_TYPE,
                &wrong_xp.encode(),
            )?;
        }
        if matches!(mutation, ChopMutation::NonCanonicalXp) {
            let mut negative_zero = [0_u8; 9];
            negative_zero[8] = 0x80;
            replace_extension_packet(
                &mut chop.ark_tx,
                crate::protocol::PLAYER_XP_PACKET_TYPE,
                &negative_zero,
            )?;
        }
        if matches!(mutation, ChopMutation::NonCanonicalHealth) {
            let mut negative_zero = [0_u8; 9];
            negative_zero[8] = 0x80;
            replace_extension_packet(
                &mut chop.ark_tx,
                crate::protocol::TREE_HEALTH_PACKET_TYPE,
                &negative_zero,
            )?;
        }
        if matches!(mutation, ChopMutation::WrongRoll) {
            replace_extension_packet(
                &mut chop.ark_tx,
                crate::protocol::TREE_ROLL_PACKET_TYPE,
                &next_tree_roll.next().encode(),
            )?;
        }
        if matches!(mutation, ChopMutation::WrongPlayerPosition) {
            let mut position = next_state.position;
            position.x = (position.x + 1) % self.world.manifest.map_width;
            replace_extension_packet(
                &mut chop.ark_tx,
                crate::protocol::PLAYER_POSITION_PACKET_TYPE,
                &position.encode(),
            )?;
        }
        if chop.ark_tx.unsigned_tx.output.len() != crate::protocol::CHOP_OUTPUT_COUNT {
            return Err(anyhow!("chop transaction has an invalid output count"));
        }
        match mutation {
            ChopMutation::ExtraOutput => {
                chop.ark_tx.unsigned_tx.output.push(TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new(),
                });
                chop.ark_tx.outputs.push(Default::default());
            }
            ChopMutation::WrongAnchor => {
                chop.ark_tx.unsigned_tx.output
                    [crate::protocol::CHOP_ANCHOR_OUTPUT_INDEX as usize]
                    .script_pubkey = ScriptBuf::new();
            }
            ChopMutation::FundExtension => {
                let state_index = crate::protocol::PLAYER_STATE_OUTPUT_INDEX as usize;
                let extension_index = crate::protocol::CHOP_EXTENSION_OUTPUT_INDEX as usize;
                let state_sats = chop.ark_tx.unsigned_tx.output[state_index]
                    .value
                    .to_sat()
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("player state cannot fund extension mutation"))?;
                chop.ark_tx.unsigned_tx.output[state_index].value = Amount::from_sat(state_sats);
                chop.ark_tx.unsigned_tx.output[extension_index].value = Amount::from_sat(1);
            }
            _ => {}
        }
        sign_ark_transaction(
            |_, message| Ok(self.keys.sign_msg(&message)),
            &mut chop.ark_tx,
            crate::protocol::PLAYER_STATE_INPUT_INDEX,
        )
        .map_err(|error| anyhow!("sign player Ark input: {error}"))?;
        sign_checkpoint_transaction(
            |_, message| Ok(self.keys.sign_msg(&message)),
            &mut chop.checkpoint_txs[crate::protocol::PLAYER_STATE_INPUT_INDEX],
        )
        .map_err(|error| anyhow!("sign player checkpoint: {error}"))?;

        let expected_ark = chop.ark_tx.clone();
        let expected_checkpoints = chop.checkpoint_txs.clone();
        let chop_tx = expected_ark.unsigned_tx.clone();
        let chop_txid = chop_tx.compute_txid();
        let recoverable_submission = matches!(
            mutation,
            ChopMutation::None | ChopMutation::SubmissionFailure
        );
        if recoverable_submission {
            self.persist_pending_chop(PendingChop::new(
                tree_id,
                success,
                state.record.outpoint,
                tree.record.outpoint,
                &expected_ark,
                &expected_checkpoints,
            ))?;
        }
        let submission = if matches!(mutation, ChopMutation::SubmissionFailure) {
            Err(anyhow!("simulated emulator internal error"))
        } else {
            self.emulator
                .submit_tx(&expected_ark, &expected_checkpoints)
                .await
                .context("submit chop to emulator")
        };
        let (returned_ark, returned_checkpoints) = match submission {
            Ok(response) => response,
            Err(submission_error) => {
                let definitive_rejection = submission_error.chain().any(|failure| {
                    failure
                        .downcast_ref::<crate::arkade::HttpFailure>()
                        .and_then(crate::arkade::HttpFailure::status_code)
                        .is_some_and(|status| {
                            (400..500).contains(&status) && ![408, 409, 425, 429].contains(&status)
                        })
                });
                if recoverable_submission && definitive_rejection {
                    self.clear_pending_chop()?;
                }
                if recoverable_submission && !definitive_rejection {
                    let submission_detail = format!("{submission_error:#}");
                    sleep_ms(500).await;
                    return match self.resume_pending_chop_inner().await {
                        Ok(true) => Ok(success),
                        Ok(false) => Err(anyhow!(
                            "chop conflicted while recovering from submission failure: {submission_detail}"
                        )),
                        Err(recovery_error) => Err(recovery_error.context(format!(
                            "recover exact pending chop after submission failure: {submission_detail}"
                        ))),
                    };
                }
                return Err(submission_error);
            }
        };
        player::verify_player_chop_response(
            &self.keys,
            &state.contract,
            &self.world.contract,
            &expected_ark,
            &expected_checkpoints,
            &returned_ark,
            returned_checkpoints,
        )?;
        let state_record = wait_for_vtxo(
            &self.rest,
            &state.contract.vtxo.script_pubkey().to_hex_string(),
            OutPoint {
                txid: chop_txid,
                vout: u32::from(crate::protocol::PLAYER_STATE_OUTPUT_INDEX),
            },
        )
        .await?;
        let tree_record = wait_for_vtxo(
            &self.rest,
            &self.world.contract.vtxo.script_pubkey().to_hex_string(),
            OutPoint {
                txid: chop_txid,
                vout: u32::from(crate::protocol::TREE_OUTPUT_INDEX),
            },
        )
        .await?;
        PlayerChopTransition {
            previous_state: state.state,
            next_state: player::player_state_from_tx(&chop_tx)?
                .ok_or_else(|| anyhow!("chop transaction omitted player state"))?,
            previous_tree_state: tree.state,
            next_tree_state: tree::tree_state_from_tx(&chop_tx)?
                .ok_or_else(|| anyhow!("chop transaction omitted tree state"))?,
            previous_tree_roll: tree.roll,
            next_tree_roll: tree::tree_roll_from_tx(&chop_tx)?
                .ok_or_else(|| anyhow!("chop transaction omitted tree roll"))?,
            previous_tree_health: tree.health,
            next_tree_health: tree::tree_health_from_tx(&chop_tx)?
                .ok_or_else(|| anyhow!("chop transaction omitted tree health"))?,
            success,
            state_logs_before: player_logs_before,
            state_logs_after: state_record.asset_amount(self.world.log_asset).unwrap_or(0),
            state_xp_balance_before: player_xp_balance_before,
            state_xp_balance_after: state_record.asset_amount(self.world.xp_asset).unwrap_or(0),
            tree_markers_before: 1,
            tree_markers_after: tree_record.asset_amount(self.world.tree_asset).unwrap_or(0),
            tree_logs_before,
            tree_logs_after: tree_record.asset_amount(self.world.log_asset).unwrap_or(0),
            tree_xp_balance_before,
            tree_xp_balance_after: tree_record.asset_amount(self.world.xp_asset).unwrap_or(0),
            state_value_before: state.record.amount_sats,
            state_value_after: state_record.amount_sats,
            tree_value_before: tree.record.amount_sats,
            tree_value_after: tree_record.amount_sats,
            dust_sats: self.params.dust_sats,
        }
        .validate()?;
        self.trees[tree_index] = LiveTree {
            state: tree.state,
            roll: next_tree_roll,
            health: tree::tree_health_from_tx(&chop_tx)?
                .ok_or_else(|| anyhow!("accepted chop omitted tree health"))?,
            deployment_txid: tree.deployment_txid,
            record: tree_record,
            previous_tx: chop_tx.clone(),
            last_attempt_txid: Some(chop_txid),
        };
        self.player_state = Some(LivePlayerState {
            contract: state.contract,
            state: next_state,
            record: state_record,
            previous_tx: chop_tx,
        });
        if matches!(mutation, ChopMutation::None) {
            self.clear_pending_chop()?;
        }
        Ok(success)
    }
}

async fn wait_for_complete_tree_records(
    rest: &ArkadeRest,
    script: &str,
    world: &World,
    dust_sats: u64,
) -> Result<Vec<VtxoRecord>> {
    let expected = world.declared_trees.len();
    for attempt in 0..INDEX_ATTEMPTS {
        let records = rest.get_vtxos(script, "spendableOnly").await?;
        let exposed = records
            .iter()
            .filter(|record| record.asset_amount(world.tree_asset).is_some())
            .count();
        if exposed >= expected {
            return select_tree_records(records, world, dust_sats);
        }
        if attempt + 1 < INDEX_ATTEMPTS {
            sleep_ms(INDEX_POLL_MS).await;
        }
    }
    Err(anyhow!(
        "woodland.sh index did not expose all {expected} trees"
    ))
}

fn select_tree_records(
    records: Vec<VtxoRecord>,
    world: &World,
    dust_sats: u64,
) -> Result<Vec<VtxoRecord>> {
    let mut trees = Vec::new();
    for record in records {
        let tree_entries = record
            .assets
            .iter()
            .filter(|asset| asset.asset_id == world.tree_asset)
            .count();
        if tree_entries == 0 {
            continue;
        }
        let log_entries = record
            .assets
            .iter()
            .filter(|asset| asset.asset_id == world.log_asset)
            .count();
        let xp_entries = record
            .assets
            .iter()
            .filter(|asset| asset.asset_id == world.xp_asset)
            .count();
        let logs = record.asset_amount(world.log_asset).unwrap_or(0);
        let xp_balance = record.asset_amount(world.xp_asset).unwrap_or(0);
        if record.amount_sats != tree::full_tree_value_sats(dust_sats)?
            || tree_entries != 1
            || log_entries > 1
            || xp_entries > 1
            || record.asset_amount(world.tree_asset) != Some(1)
            || logs > world.manifest.log_reserve_per_tree
            || xp_balance > world.manifest.xp_per_tree
            || logs != xp_balance
            || !record.assets.iter().all(|asset| {
                asset.asset_id == world.tree_asset
                    || asset.asset_id == world.log_asset
                    || asset.asset_id == world.xp_asset
            })
        {
            return Err(anyhow!("indexed tree record violates the world manifest"));
        }
        trees.push(record);
    }
    if trees.len() < world.declared_trees.len() {
        return Err(anyhow!(
            "woodland.sh exposes {} of {} trees",
            trees.len(),
            world.declared_trees.len()
        ));
    }
    Ok(trees)
}

fn select_player_state_record(
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
        _ => Err(anyhow!("PLAYER_ID has multiple recursive state VTXOs")),
    }
}

async fn wait_for_virtual_txs(
    rest: &ArkadeRest,
    txids: &[Txid],
) -> Result<std::collections::HashMap<Txid, Transaction>> {
    let mut last_error = None;
    for _ in 0..INDEX_ATTEMPTS {
        match rest.get_virtual_txs(txids).await {
            Ok(transactions) => return Ok(transactions),
            Err(error) => last_error = Some(error),
        }
        sleep_ms(INDEX_POLL_MS).await;
    }
    Err(last_error
        .unwrap_or_else(|| anyhow!("virtual transaction lookup failed"))
        .context("indexer did not expose creating virtual transactions"))
}

fn player_vtxo(keys: &Keys, params: &ServerParams) -> Result<ark_core::Vtxo> {
    txbuild::player_vtxo(keys, params)
}

fn transfer_group(
    asset_id: AssetId,
    inputs: Vec<(u16, u64)>,
    outputs: Vec<(u16, u64)>,
) -> AssetGroup {
    AssetGroup {
        asset_id: Some(asset_id),
        control_asset: None,
        metadata: None,
        inputs: inputs
            .into_iter()
            .map(|(input_index, amount)| AssetInput {
                input_index,
                amount,
            })
            .collect(),
        outputs: outputs
            .into_iter()
            .map(|(output_index, amount)| AssetOutput {
                output_index,
                amount,
            })
            .collect(),
    }
}

fn replace_extension_packet(
    psbt: &mut bitcoin::Psbt,
    packet_type: u8,
    replacement: &[u8],
) -> Result<()> {
    let output_index = psbt
        .unsigned_tx
        .output
        .iter()
        .position(|output| ark_core::extension::is_extension(&output.script_pubkey))
        .ok_or_else(|| anyhow!("transaction has no extension output"))?;
    let payload = ark_core::extension::extension_payload(
        &psbt.unsigned_tx.output[output_index].script_pubkey,
    )
    .ok_or_else(|| anyhow!("transaction extension payload is invalid"))?;
    let packets = ark_core::extension::iter_packets(payload).context("parse extension packets")?;
    let mut encoded = ark_core::extension::MAGIC_BYTES.to_vec();
    let mut replaced = false;
    for (current_type, current_payload) in packets {
        let current_payload = if current_type == packet_type {
            replaced = true;
            replacement
        } else {
            current_payload
        };
        encoded.push(current_type);
        ark_core::extension::encode_uvarint(&mut encoded, current_payload.len() as u64);
        encoded.extend_from_slice(current_payload);
    }
    if !replaced {
        return Err(anyhow!("extension packet {packet_type} is missing"));
    }
    psbt.unsigned_tx.output[output_index].script_pubkey = op_return_script(&encoded);
    Ok(())
}

fn op_return_script(data: &[u8]) -> ScriptBuf {
    let mut script = vec![bitcoin::opcodes::all::OP_RETURN.to_u8()];
    let len = data.len();
    if len <= 75 {
        script.push(len as u8);
    } else if len <= 0xff {
        script.extend_from_slice(&[0x4c, len as u8]);
    } else if len <= 0xffff {
        script.push(0x4d);
        script.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        script.push(0x4e);
        script.extend_from_slice(&(len as u32).to_le_bytes());
    }
    script.extend_from_slice(data);
    ScriptBuf::from_bytes(script)
}

fn tree_vtxo_input(record: &VtxoRecord, contract: &TreeContract) -> Result<VtxoInput> {
    if record.script != contract.vtxo.script_pubkey() {
        return Err(anyhow!("tree record does not match the covenant script"));
    }
    let control_block = contract
        .vtxo
        .get_spend_info(contract.chop_spend_script.clone())
        .map_err(|error| anyhow!("tree spend info: {error}"))?;
    let assets = record
        .assets
        .iter()
        .map(|asset| {
            if asset.amount == 0 {
                return Err(anyhow!(
                    "indexed tree record has zero asset {}",
                    asset.asset_id
                ));
            }
            Ok(asset.clone())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(VtxoInput::new(
        contract.chop_spend_script.clone(),
        None,
        control_block,
        contract.vtxo.tapscripts(),
        contract.vtxo.script_pubkey(),
        Amount::from_sat(record.amount_sats),
        record.outpoint,
        assets,
    ))
}

async fn wait_for_vtxo(rest: &ArkadeRest, script: &str, outpoint: OutPoint) -> Result<VtxoRecord> {
    for _ in 0..INDEX_ATTEMPTS {
        let records = rest.get_vtxos(script, "spendableOnly").await?;
        if let Some(record) = records
            .into_iter()
            .find(|record| record.outpoint == outpoint)
        {
            return Ok(record);
        }
        sleep_ms(INDEX_POLL_MS).await;
    }
    Err(anyhow!("indexer did not expose VTXO {outpoint}"))
}

fn require_nonzero_asset(record: &VtxoRecord, asset_id: AssetId, message: &str) -> Result<u64> {
    record
        .asset_amount(asset_id)
        .filter(|amount| *amount > 0)
        .ok_or_else(|| anyhow!(message.to_string()))
}

fn require_asset_amount(
    record: &VtxoRecord,
    asset_id: AssetId,
    expected: u64,
    label: &str,
) -> Result<()> {
    let actual = record.asset_amount(asset_id).unwrap_or(0);
    if actual != expected {
        return Err(anyhow!("{label} amount is {actual}, expected {expected}"));
    }
    Ok(())
}

fn browser_storage() -> Result<web_sys::Storage> {
    web_sys::window()
        .ok_or_else(|| anyhow!("no browser window"))?
        .local_storage()
        .map_err(|error| anyhow!("open browser storage: {error:?}"))?
        .ok_or_else(|| anyhow!("browser storage is unavailable"))
}

fn load_pending_chop(storage_key: &str) -> Result<Option<PendingChop>> {
    browser_storage()?
        .get_item(storage_key)
        .map_err(|error| anyhow!("read pending chop journal: {error:?}"))?
        .map(|json| PendingChop::from_json(&json))
        .transpose()
}

fn js_err(error: anyhow::Error) -> JsValue {
    JsValue::from_str(&format!("{error:#}"))
}

async fn sleep_ms(ms: u64) {
    use wasm_bindgen::JsCast;
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .expect("window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                resolve.unchecked_ref(),
                ms as i32,
            )
            .expect("setTimeout");
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}
