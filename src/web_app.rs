//! Browser/WASM application for direct interaction with woodland.sh covenants.

use crate::arkade::{ArkadeRest, EmulatorParams, EmulatorRest, ServerParams, VtxoRecord};
use crate::chop::{require_asset_amount, ChopMutation, ExpectedChop, PendingChop};
use crate::keys::Keys;
use crate::player::{self, PlayerChopTransition, PlayerContract, PlayerState};
use crate::tree::{self, TreeContract, TreeHealth, TreeState};
use crate::txbuild;
use crate::world::{ValidatedTree, ValidatedWorld, WorldManifest};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::packet::{AssetGroup, AssetOutput, Packet};
use ark_core::asset::AssetId;
use ark_core::send::{build_offchain_transactions, SendReceiver};
use ark_core::Asset;
use bitcoin::{Amount, OutPoint, Transaction, Txid};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

const INDEX_ATTEMPTS: usize = 80;
const INDEX_POLL_MS: u64 = 250;
/// A submitted swing whose response was lost may still land; poll the
/// indexer for the journaled outpoints for this long before resubmitting,
/// because a resubmission that races the original can trip the service's
/// concurrent-spend protection.
const RESUME_RECONCILE_ATTEMPTS: u32 = 15;
const RESUME_RECONCILE_DELAY_MS: u64 = 2_000;

fn is_definitive_submission_rejection(error: &anyhow::Error) -> bool {
    error.chain().any(|failure| {
        failure
            .downcast_ref::<crate::arkade::HttpFailure>()
            .and_then(crate::arkade::HttpFailure::status_code)
            .is_some_and(|status| {
                (400..500).contains(&status) && ![408, 409, 425, 429].contains(&status)
            })
    })
}

// Spendable-only queries can briefly expose both sides of a just-committed
// transition. Retry that snapshot, but keep persistent forks fail-closed.
#[derive(Debug)]
struct TransientIndexSnapshot(String);

impl std::fmt::Display for TransientIndexSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TransientIndexSnapshot {}

fn transient_index_snapshot(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(TransientIndexSnapshot(detail.into()))
}

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
    health: TreeHealth,
    deployment_txid: Txid,
    record: VtxoRecord,
    previous_tx: Option<Transaction>,
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
struct AppSnapshot {
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
    player_luck_credit: Option<u64>,
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

#[derive(Clone, Copy)]
struct TreeViewport {
    min_x: u16,
    min_y: u16,
    max_x: u16,
    max_y: u16,
}

impl TreeViewport {
    fn contains(self, state: TreeState) -> bool {
        state.x >= self.min_x
            && state.x <= self.max_x
            && state.y >= self.min_y
            && state.y <= self.max_y
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
    /// Drop prediction exists only for the adversarial e2e build; a
    /// production snapshot exposes observed state, never future outcomes.
    #[cfg(feature = "regtest-e2e")]
    next_roll_bucket: u64,
    #[cfg(feature = "regtest-e2e")]
    next_drop: bool,
    expires_in_seconds: Option<i64>,
    /// True when the tree's local LOG/XP reserve is permanently exhausted.
    /// A funded stump is instead permissionlessly regrowable in one batch.
    depleted: bool,
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
    tree_viewport: TreeViewport,
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
        let pending_storage_key = format!("woodland.sh:web:v2:pending:{server}:{genesis_txid}");
        let pending_chop = load_pending_chop(&pending_storage_key).map_err(js_err)?;
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
        let tree_viewport = TreeViewport {
            min_x: 0,
            min_y: 0,
            max_x: manifest.map_width.saturating_sub(1).min(64),
            max_y: manifest.map_height.saturating_sub(1).min(64),
        };
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
            tree_viewport,
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

    #[wasm_bindgen(js_name = setTreeViewport)]
    pub fn set_tree_viewport(
        &mut self,
        min_x: u16,
        min_y: u16,
        max_x: u16,
        max_y: u16,
    ) -> Result<(), JsValue> {
        if min_x > max_x
            || min_y > max_y
            || max_x >= self.world.manifest.map_width
            || max_y >= self.world.manifest.map_height
        {
            return Err(JsValue::from_str("tree viewport is outside the world map"));
        }
        self.tree_viewport = TreeViewport {
            min_x,
            min_y,
            max_x,
            max_y,
        };
        Ok(())
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

    #[wasm_bindgen(js_name = withdrawLog)]
    pub async fn withdraw_log(&mut self, amount: f64) -> Result<JsValue, JsValue> {
        if amount.fract() != 0.0 || !(1.0..=u64::MAX as f64).contains(&amount) {
            return Err(JsValue::from_str(
                "withdraw amount must be a positive integer",
            ));
        }
        self.withdraw_log_inner(amount as u64)
            .await
            .map_err(js_err)?;
        self.refresh().await
    }
    pub async fn refresh(&mut self) -> Result<JsValue, JsValue> {
        self.sync_player().await.map_err(js_err)?;
        serde_wasm_bindgen::to_value(&self.snapshot())
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    #[wasm_bindgen(js_name = refreshWorld)]
    pub async fn refresh_world(&mut self) -> Result<JsValue, JsValue> {
        self.sync().await.map_err(js_err)?;
        serde_wasm_bindgen::to_value(&self.snapshot())
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    #[wasm_bindgen(js_name = refreshPlayer)]
    pub async fn refresh_player(&mut self) -> Result<JsValue, JsValue> {
        self.sync_player().await.map_err(js_err)?;
        serde_wasm_bindgen::to_value(&self.snapshot())
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    pub async fn chop(&mut self, tree_id: u32) -> Result<JsValue, JsValue> {
        let success = self.chop_inner(tree_id).await.map_err(js_err)?;
        self.last_attempt = Some(AttemptView { tree_id, success });
        self.refresh().await
    }

    /// Permissionlessly regrow a funded stump in one fresh Ark batch.
    pub async fn regrow(&mut self, tree_id: u32) -> Result<JsValue, JsValue> {
        self.regrow_tree_inner(tree_id).await.map_err(js_err)?;
        self.refresh().await
    }

    #[cfg(feature = "regtest-e2e")]
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

    fn snapshot(&self) -> AppSnapshot {
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
        #[cfg(feature = "regtest-e2e")]
        let next_luck = self
            .player_state
            .as_ref()
            .map(|player| player.state.luck.advance(player_xp));
        let trees = self
            .trees
            .iter()
            .filter(|tree| self.tree_viewport.contains(tree.state))
            .map(|tree| {
                let logs = tree.record.asset_amount(self.world.log_asset).unwrap_or(0);
                let xp_balance = tree.record.asset_amount(self.world.xp_asset).unwrap_or(0);
                #[cfg(feature = "regtest-e2e")]
                let (next_roll_bucket, raw_drop) = next_luck
                    .map(|(luck, drop)| (luck.roll.bucket(), drop))
                    .unwrap_or((0, false));
                #[cfg(feature = "regtest-e2e")]
                let next_drop = raw_drop && tree.health.value() > 0 && logs > 0 && xp_balance > 0;
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
                    #[cfg(feature = "regtest-e2e")]
                    next_roll_bucket,
                    #[cfg(feature = "regtest-e2e")]
                    next_drop,
                    expires_in_seconds: tree.record.expires_in(now),
                    // One permissionless batch regrows a funded stump.
                    // Zero local reserve is terminal.
                    depleted: logs == 0,
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
        AppSnapshot {
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
            player_luck_credit: self
                .player_state
                .as_ref()
                .map(|player| player.state.luck.credit.value()),
            season_xp_remaining: self.trees.iter().fold(0_u64, |total, tree| {
                total.saturating_add(tree.record.asset_amount(self.world.xp_asset).unwrap_or(0))
            }),
            player_state_expires_in_seconds,
            player_rollover_margin_seconds,
            log_drop_basis_points: player::log_drop_basis_points(player_xp),
            player_logs: self
                .player_state
                .as_ref()
                .and_then(|player| player.record.asset_amount(self.world.log_asset))
                .unwrap_or(0),
            player_state_outpoint: self
                .player_state
                .as_ref()
                .map(|player| player.record.outpoint.to_string()),
            full_tree_value_sats: self.params.dust_sats,
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
        let mut initialized_trees = !self.trees.is_empty();
        let mut last_transient = None;
        for attempt in 0..INDEX_ATTEMPTS {
            match self.sync_once().await {
                Ok(()) if initialized_trees => return Ok(()),
                Ok(()) => {
                    initialized_trees = true;
                    continue;
                }
                Err(error) if error.downcast_ref::<TransientIndexSnapshot>().is_some() => {
                    last_transient = Some(error);
                }
                Err(error) => return Err(error),
            }
            if attempt + 1 < INDEX_ATTEMPTS {
                txbuild::sleep_ms(INDEX_POLL_MS).await;
            }
        }
        Err(last_transient
            .unwrap_or_else(|| transient_index_snapshot("index snapshot did not converge"))
            .context("index did not converge to one spendable lineage"))
    }

    async fn sync_once(&mut self) -> Result<()> {
        let wallet = player_vtxo(&self.keys, &self.params)?;
        let wallet_script = wallet.script_pubkey().to_hex_string();
        let player_contract = self.player_contract()?;
        let player_asset = self.player_asset()?;
        let initializing_trees = self.trees.is_empty();
        let trees_to_refresh = self
            .trees
            .iter()
            .filter(|tree| self.tree_viewport.contains(tree.state))
            .cloned()
            .collect::<Vec<_>>();
        let expected_tree_count = if initializing_trees {
            self.world.declared_trees.len()
        } else {
            trees_to_refresh.len()
        };
        let tree_records = if initializing_trees {
            load_current_tree_records(&self.rest, &self.world, &[]).await?
        } else if trees_to_refresh.is_empty() {
            Vec::new()
        } else {
            load_current_tree_records(&self.rest, &self.world, &trees_to_refresh).await?
        };
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

        let declared_by_state = self
            .world
            .declared_trees
            .iter()
            .map(|tree| (tree.state, *tree))
            .collect::<std::collections::HashMap<_, _>>();
        let declared_by_deployment = self
            .world
            .declared_trees
            .iter()
            .map(|tree| (tree.deployment_txid, *tree))
            .collect::<std::collections::HashMap<_, _>>();
        let mut cached_by_outpoint = trees_to_refresh
            .into_iter()
            .map(|tree| (tree.record.outpoint, tree))
            .collect::<std::collections::HashMap<_, _>>();
        let mut discovered = Vec::with_capacity(tree_records.len());
        let mut uncached_records = Vec::new();
        let mut seen_states = std::collections::HashMap::new();
        for record in tree_records {
            if let Some(mut cached) = cached_by_outpoint.remove(&record.outpoint) {
                if let Some(previous_tx) = cached.previous_tx.as_ref() {
                    record.validate_creating_transaction(previous_tx)?;
                } else {
                    validate_initial_tree_record(
                        &record,
                        &self.world,
                        cached.state,
                        cached.deployment_txid,
                    )?;
                }
                if let Some(competing) = seen_states.insert(cached.state, record.outpoint) {
                    return Err(transient_index_snapshot(format!(
                        "tree {} exposes competing spendable lineages {competing} and {}",
                        cached.state.tree_id, record.outpoint
                    )));
                }
                cached.record = record;
                discovered.push(cached);
            } else if let Some(declared) = declared_by_deployment.get(&record.outpoint.txid) {
                validate_initial_tree_record(
                    &record,
                    &self.world,
                    declared.state,
                    declared.deployment_txid,
                )?;
                if let Some(competing) = seen_states.insert(declared.state, record.outpoint) {
                    return Err(transient_index_snapshot(format!(
                        "tree {} exposes competing spendable lineages {competing} and {}",
                        declared.state.tree_id, record.outpoint
                    )));
                }
                discovered.push(LiveTree {
                    state: declared.state,
                    health: tree::TreeHealth::new(self.world.manifest.active_logs_per_tree)?,
                    deployment_txid: declared.deployment_txid,
                    record,
                    previous_tx: None,
                    last_attempt_txid: None,
                });
            } else {
                uncached_records.push(record);
            }
        }

        let cached_player = self.player_state.clone().filter(|cached| {
            player_state_record
                .as_ref()
                .is_some_and(|record| record.outpoint == cached.record.outpoint)
        });
        let mut txids = uncached_records
            .iter()
            .map(|record| record.outpoint.txid)
            .collect::<Vec<_>>();
        if cached_player.is_none() {
            if let Some(record) = &player_state_record {
                txids.push(record.outpoint.txid);
            }
        }
        let transactions = wait_for_virtual_txs(&self.rest, &txids).await?;
        for record in uncached_records {
            let previous_tx = transactions
                .get(&record.outpoint.txid)
                .cloned()
                .ok_or_else(|| anyhow!("missing current tree transaction"))?;
            record.validate_creating_transaction(&previous_tx)?;
            let state = tree::tree_state_from_tx(&previous_tx)?
                .ok_or_else(|| anyhow!("current tree transaction has no state packet"))?;
            let health = tree::tree_health_from_tx(&previous_tx)?
                .ok_or_else(|| anyhow!("current tree transaction has no health packet"))?;
            let declared = declared_by_state
                .get(&state)
                .copied()
                .ok_or_else(|| anyhow!("current tree has an unknown world identity"))?;
            let is_deployment = record.outpoint.txid == declared.deployment_txid;
            let transition = tree::classify_transition(&previous_tx, is_deployment)?;
            let expected_vout = transition.output_index();
            if record.outpoint.vout != expected_vout
                || (is_deployment && health.value() != self.world.manifest.active_logs_per_tree)
            {
                return Err(anyhow!("current tree record has an invalid lineage"));
            }
            validate_tree_local_state(&record, &self.world, health)?;
            if let Some(competing) = seen_states.insert(state, record.outpoint) {
                return Err(transient_index_snapshot(format!(
                    "tree {} exposes competing spendable lineages {competing} and {}",
                    state.tree_id, record.outpoint
                )));
            }
            discovered.push(LiveTree {
                state,
                health,
                deployment_txid: declared.deployment_txid,
                last_attempt_txid: (transition == tree::TreeTransition::Chop)
                    .then_some(record.outpoint.txid),
                record,
                previous_tx: Some(previous_tx),
            });
        }
        if seen_states.len() != expected_tree_count {
            return Err(transient_index_snapshot(
                "not all viewport trees are currently discoverable",
            ));
        }
        discovered.sort_by_key(|tree| tree.state.tree_id);
        if initializing_trees {
            self.trees = discovered;
        } else {
            let mut updates = discovered
                .into_iter()
                .map(|tree| (tree.state, tree))
                .collect::<std::collections::HashMap<_, _>>();
            for tree in &mut self.trees {
                if let Some(update) = updates.remove(&tree.state) {
                    *tree = update;
                }
            }
            if !updates.is_empty() {
                return Err(anyhow!("viewport contains an unknown tree identity"));
            }
        }

        self.player_state = match player_state_record {
            Some(record) => {
                if let Some(mut cached) = cached_player {
                    record.validate_creating_transaction(&cached.previous_tx)?;
                    cached.record = record;
                    Some(cached)
                } else {
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

                    let expected_identity = player::derive_player_identity(
                        self.keys.owner_pk(),
                        self.world.genesis_txid,
                    );
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
            }
            None => None,
        };
        self.wallet_records = wallet_records;
        self.reconcile_pending_chop()?;
        Ok(())
    }

    async fn sync_tree(&mut self, tree_id: u32) -> Result<()> {
        let tree_index = self
            .trees
            .iter()
            .position(|tree| tree.state.tree_id == tree_id)
            .ok_or_else(|| anyhow!("tree {tree_id} is not part of this world"))?;
        let current = self.trees[tree_index].clone();
        let mut records =
            load_current_tree_records(&self.rest, &self.world, std::slice::from_ref(&current))
                .await?;
        let record = records
            .pop()
            .ok_or_else(|| anyhow!("selected tree is not indexed"))?;
        if record.outpoint == current.record.outpoint {
            if let Some(previous_tx) = current.previous_tx.as_ref() {
                record.validate_creating_transaction(previous_tx)?;
            } else {
                validate_initial_tree_record(
                    &record,
                    &self.world,
                    current.state,
                    current.deployment_txid,
                )?;
            }
            validate_tree_local_state(&record, &self.world, current.health)?;
            self.trees[tree_index].record = record;
            return Ok(());
        }
        let transactions = wait_for_virtual_txs(&self.rest, &[record.outpoint.txid]).await?;
        let previous_tx = transactions
            .get(&record.outpoint.txid)
            .cloned()
            .ok_or_else(|| anyhow!("missing selected tree transaction"))?;
        record.validate_creating_transaction(&previous_tx)?;
        let state = tree::tree_state_from_tx(&previous_tx)?
            .ok_or_else(|| anyhow!("selected tree transaction has no state packet"))?;
        if state != current.state {
            return Err(anyhow!("selected tree successor changed identity"));
        }
        let health = tree::tree_health_from_tx(&previous_tx)?
            .ok_or_else(|| anyhow!("selected tree transaction has no health packet"))?;
        let transition = tree::classify_transition(&previous_tx, false)?;
        if record.outpoint.vout != transition.output_index() {
            return Err(anyhow!("selected tree successor has an invalid output"));
        }
        validate_tree_local_state(&record, &self.world, health)?;
        self.trees[tree_index] = LiveTree {
            state,
            health,
            deployment_txid: current.deployment_txid,
            last_attempt_txid: (transition == tree::TreeTransition::Chop)
                .then_some(record.outpoint.txid),
            record,
            previous_tx: Some(previous_tx),
        };
        Ok(())
    }

    async fn sync_player(&mut self) -> Result<()> {
        let wallet = player_vtxo(&self.keys, &self.params)?;
        let wallet_records = self
            .rest
            .get_vtxos(&wallet.script_pubkey().to_hex_string(), "spendableOnly")
            .await?;
        let player_contract = self.player_contract()?;
        let player_asset = self.player_asset()?;
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
        self.player_state = match player_state_record {
            Some(record) => {
                if let Some(mut cached) = self
                    .player_state
                    .clone()
                    .filter(|cached| cached.record.outpoint == record.outpoint)
                {
                    record.validate_creating_transaction(&cached.previous_tx)?;
                    cached.record = record;
                    Some(cached)
                } else {
                    let transactions =
                        wait_for_virtual_txs(&self.rest, &[record.outpoint.txid]).await?;
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
                    let expected_identity = player::derive_player_identity(
                        self.keys.owner_pk(),
                        self.world.genesis_txid,
                    );
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
            }
            None => None,
        };
        self.wallet_records = wallet_records;
        self.reconcile_pending_chop()
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

    fn profile_storage_key(&self) -> Result<String> {
        let origin = web_sys::window()
            .ok_or_else(|| anyhow!("no browser window"))?
            .location()
            .origin()
            .map_err(|error| anyhow!("read browser location origin: {error:?}"))?;
        Ok(format!(
            "woodland.sh:web:v2:profile:{origin}:{}",
            self.world.genesis_txid
        ))
    }

    fn persist_profile(&self) -> Result<()> {
        let json = serde_json::to_string(&self.profile).context("serialize player profile")?;
        browser_storage()?
            .set_item(&self.profile_storage_key()?, &json)
            .map_err(|error| anyhow!("persist player profile: {error:?}"))?;
        Ok(())
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
        for attempt in 0..RESUME_RECONCILE_ATTEMPTS {
            let pending_before_sync = self.pending_chop.clone();
            self.sync().await?;
            if self.pending_chop.is_none() {
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
            }
            if attempt + 1 < RESUME_RECONCILE_ATTEMPTS {
                txbuild::sleep_ms(RESUME_RECONCILE_DELAY_MS).await;
            }
        }
        let Some(pending) = self.pending_chop.clone() else {
            return Ok(false);
        };
        let contract = self.player_contract()?;
        let (expected_ark, expected_checkpoints) = pending.decode_psbts()?;
        let submission = self
            .emulator
            .submit_tx(&expected_ark, &expected_checkpoints)
            .await
            .context("resume pending chop through emulator");
        let (returned_ark, returned_checkpoints) = match submission {
            Ok(response) => response,
            Err(error) if is_definitive_submission_rejection(&error) => {
                let expected_txid = pending.txid()?;
                self.sync()
                    .await
                    .context("reconcile pending chop after definitive rejection")?;
                let accepted = self
                    .player_state
                    .as_ref()
                    .is_some_and(|state| state.record.outpoint.txid == expected_txid)
                    && self
                        .trees
                        .iter()
                        .find(|tree| tree.state.tree_id == pending.tree_id)
                        .is_some_and(|tree| tree.record.outpoint.txid == expected_txid);
                if self.pending_chop.is_some() {
                    self.clear_pending_chop()?;
                }
                return Ok(accepted);
            }
            Err(error) => return Err(error),
        };
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
        self.sync_player().await?;
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
        let identity =
            player::derive_player_identity(self.keys.owner_pk(), self.world.genesis_txid);
        player::attach_player_state_packets(
            &mut activation.ark_tx,
            PlayerState {
                identity,
                position: player::PlayerPosition {
                    x: crate::world::PLAYER_SPAWN_X,
                    y: crate::world::PLAYER_SPAWN_Y,
                },
                luck: player::PlayerLuck::initial(identity),
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
        // Persist the derived PLAYER_ID before submission: the JavaScript
        // side only writes the profile after the action settles, so a crash
        // in between would otherwise strand the activated state.
        self.persist_profile()?;
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
        self.sync_player().await?;
        if self.player_state.is_none() {
            return Err(anyhow!("activated player state was not discovered"));
        }
        Ok(())
    }
    async fn withdraw_log_inner(&mut self, amount: u64) -> Result<()> {
        self.sync_player().await?;
        let state = self
            .player_state
            .clone()
            .ok_or_else(|| anyhow!("activate the player before withdrawing"))?;
        let player_asset = self.require_player_asset()?;
        let wallet = player_vtxo(&self.keys, &self.params)?;
        let funding = self
            .wallet_records
            .iter()
            .filter(|record| {
                record.assets.is_empty() && record.amount_sats == self.params.dust_sats
            })
            .cloned()
            .collect::<Vec<_>>();
        let [funding] = funding.as_slice() else {
            return Err(anyhow!(
                "keep exactly one {}-sat wallet VTXO for withdrawal funding",
                self.params.dust_sats
            ));
        };
        let destination =
            bitcoin::Address::from_script(&wallet.script_pubkey(), self.params.network)
                .map_err(|error| anyhow!("derive wallet address: {error}"))?;
        let funding_previous_tx = self
            .rest
            .get_virtual_txs(&[funding.outpoint.txid])
            .await?
            .remove(&funding.outpoint.txid)
            .ok_or_else(|| anyhow!("indexer omitted the funding transaction"))?;
        let prepared = crate::chop::prepare_withdraw(
            &self.keys,
            &self.info,
            &state.contract,
            player_asset,
            &state.record,
            &state.previous_tx,
            state.state,
            funding,
            &funding_previous_tx,
            &wallet,
            amount,
            destination,
        )?;
        let txid = prepared.ark_tx.unsigned_tx.compute_txid();
        let (returned_ark, _) = self
            .emulator
            .submit_tx(&prepared.ark_tx, &prepared.checkpoint_txs)
            .await
            .context("submit withdraw to emulator")?;
        if returned_ark.unsigned_tx != prepared.ark_tx.unsigned_tx {
            return Err(anyhow!("emulator changed the submitted withdraw"));
        }
        wait_for_vtxo(
            &self.rest,
            &state.contract.vtxo.script_pubkey().to_hex_string(),
            OutPoint {
                txid,
                vout: u32::from(crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX),
            },
        )
        .await?;
        self.sync_player().await?;
        Ok(())
    }

    async fn renew_player_inner(&mut self) -> Result<()> {
        self.sync_player().await?;
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
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )?;
        let services = crate::batch::BatchServices::connect(
            &self.world.manifest.arkade_service_url,
            self.emulator.clone(),
            self.params.clone(),
            self.world.manifest.pins(self.params.network)?,
        )
        .await?;
        let outcome = services
            .settle_renewal(
                &self.keys,
                self.emulator_params.signer_pk,
                prepared,
                &state.previous_tx,
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
        self.sync_player().await?;
        Ok(())
    }

    async fn regrow_tree_inner(&mut self, tree_id: u32) -> Result<()> {
        self.sync_tree(tree_id).await?;
        let tree = self
            .trees
            .iter()
            .find(|tree| tree.state.tree_id == tree_id)
            .cloned()
            .ok_or_else(|| anyhow!("tree {tree_id} is not part of this world"))?;
        if tree.health.value() != 0 {
            return Err(anyhow!("tree {tree_id} is not a stump"));
        }
        if tree.record.asset_amount(self.world.log_asset).unwrap_or(0) == 0 {
            return Err(anyhow!("tree {tree_id} has exhausted its local reserve"));
        }
        let previous_tx = tree
            .previous_tx
            .as_ref()
            .ok_or_else(|| anyhow!("tree stump has no indexed creating transaction"))?;
        let prepared = crate::renewal::prepare_tree(
            &tree.record,
            previous_tx,
            &self.world.contract,
            self.world.tree_asset,
            self.world.log_asset,
            self.world.xp_asset,
            self.params.dust_sats,
            0,
        )?;
        let services = crate::batch::BatchServices::connect(
            &self.world.manifest.arkade_service_url,
            self.emulator.clone(),
            self.params.clone(),
            self.world.manifest.pins(self.params.network)?,
        )
        .await?;
        let outcome = services
            .settle_renewal(
                &self.keys,
                self.emulator_params.signer_pk,
                prepared,
                previous_tx,
            )
            .await?;
        wait_for_vtxo(
            &self.rest,
            &self.world.contract.vtxo.script_pubkey().to_hex_string(),
            outcome.outpoint,
        )
        .await?;
        self.sync_tree(tree_id).await?;
        let regrown = self
            .trees
            .iter()
            .find(|tree| tree.state.tree_id == tree_id)
            .ok_or_else(|| anyhow!("regrown tree disappeared from the world"))?;
        if regrown.record.outpoint != outcome.outpoint
            || regrown.health.value() != tree::LOGS_PER_TREE
        {
            return Err(anyhow!("regrown tree state is invalid"));
        }
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
        self.sync_tree(tree_id).await?;
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
        let mut tree = self.trees[tree_index].clone();
        if tree.previous_tx.is_none() {
            let transactions =
                wait_for_virtual_txs(&self.rest, &[tree.record.outpoint.txid]).await?;
            let previous_tx = transactions
                .get(&tree.record.outpoint.txid)
                .cloned()
                .ok_or_else(|| anyhow!("missing selected tree transaction"))?;
            tree.record.validate_creating_transaction(&previous_tx)?;
            tree.previous_tx = Some(previous_tx);
        }
        if let Some(expected) = expected {
            let (_, success) = state.state.luck.advance(state.state.xp.value());
            if tree.record.outpoint.to_string() != expected.tree_outpoint
                || state.record.outpoint.to_string() != expected.player_state_outpoint
                || success != expected.drop
            {
                return Err(anyhow!(
                    "chop precondition changed; refresh before selecting another action"
                ));
            }
        }
        let prepared = crate::chop::prepare_chop(
            &self.keys,
            &self.info,
            &crate::chop::ChopWorld {
                contract: &self.world.contract,
                tree_asset: self.world.tree_asset,
                log_asset: self.world.log_asset,
                xp_asset: self.world.xp_asset,
                dust_sats: self.params.dust_sats,
                map_width: self.world.manifest.map_width,
            },
            &crate::chop::PlayerChopState {
                record: &state.record,
                previous_tx: &state.previous_tx,
                contract: &state.contract,
                state: state.state,
                player_asset,
            },
            &crate::chop::TreeChopState {
                record: &tree.record,
                previous_tx: tree
                    .previous_tx
                    .as_ref()
                    .expect("selected tree transaction was loaded"),
                health: tree.health,
            },
            mutation,
        )?;
        let success = prepared.success;
        let player_logs_before = state.record.asset_amount(self.world.log_asset).unwrap_or(0);
        let player_xp_balance_before = state.record.asset_amount(self.world.xp_asset).unwrap_or(0);
        let tree_logs_before = tree.record.asset_amount(self.world.log_asset).unwrap_or(0);
        let tree_xp_balance_before = tree.record.asset_amount(self.world.xp_asset).unwrap_or(0);

        let expected_ark = prepared.ark_tx.clone();
        let expected_checkpoints = prepared.checkpoint_txs.clone();
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
                let definitive_rejection = is_definitive_submission_rejection(&submission_error);
                if recoverable_submission && definitive_rejection {
                    self.clear_pending_chop()?;
                }
                if recoverable_submission && !definitive_rejection {
                    let submission_detail = format!("{submission_error:#}");
                    txbuild::sleep_ms(500).await;
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
        let next_state = player::player_state_from_tx(&chop_tx)?
            .ok_or_else(|| anyhow!("chop transaction omitted player state"))?;
        PlayerChopTransition {
            previous_state: state.state,
            next_state,
            previous_tree_state: tree.state,
            next_tree_state: tree::tree_state_from_tx(&chop_tx)?
                .ok_or_else(|| anyhow!("chop transaction omitted tree state"))?,
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
        let accepted_health = tree::tree_health_from_tx(&chop_tx)?
            .ok_or_else(|| anyhow!("accepted chop omitted tree health"))?;
        validate_tree_local_state(&tree_record, &self.world, accepted_health)?;
        self.trees[tree_index] = LiveTree {
            state: tree.state,
            health: accepted_health,
            deployment_txid: tree.deployment_txid,
            record: tree_record,
            previous_tx: Some(chop_tx.clone()),
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

fn validate_initial_tree_record(
    record: &VtxoRecord,
    world: &World,
    state: TreeState,
    deployment_txid: Txid,
) -> Result<()> {
    if record.outpoint
        != (OutPoint {
            txid: deployment_txid,
            vout: 0,
        })
        || record.script != world.contract.vtxo.script_pubkey()
        || record.amount_sats != world.manifest.dust_sats
        || record.assets.len() != 3
    {
        return Err(anyhow!(
            "initial tree {} record does not match its deployment",
            state.tree_id
        ));
    }
    require_asset_amount(record, world.tree_asset, 1, "tree marker")?;
    require_asset_amount(
        record,
        world.log_asset,
        world.manifest.log_reserve_per_tree,
        "tree LOG reserve",
    )?;
    require_asset_amount(
        record,
        world.xp_asset,
        world.manifest.xp_per_tree,
        "tree XP reserve",
    )
}

fn validate_tree_local_state(record: &VtxoRecord, world: &World, health: TreeHealth) -> Result<()> {
    if record.script != world.contract.vtxo.script_pubkey()
        || record.amount_sats != world.manifest.dust_sats
        || record.asset_amount(world.tree_asset).unwrap_or(0) != 1
        || record.assets.iter().any(|asset| {
            ![world.tree_asset, world.log_asset, world.xp_asset].contains(&asset.asset_id)
        })
    {
        return Err(anyhow!("tree record has invalid backing or foreign assets"));
    }
    let logs = record.asset_amount(world.log_asset).unwrap_or(0);
    let xp = record.asset_amount(world.xp_asset).unwrap_or(0);
    if logs > world.manifest.log_reserve_per_tree
        || xp > world.manifest.xp_per_tree
        || logs != xp
        || health.value() > logs
    {
        return Err(anyhow!("tree record has invalid local reserves"));
    }
    Ok(())
}

async fn load_current_tree_records(
    rest: &ArkadeRest,
    world: &World,
    cached: &[LiveTree],
) -> Result<Vec<VtxoRecord>> {
    if cached.is_empty() {
        let value = world.manifest.dust_sats;
        return Ok(world
            .declared_trees
            .iter()
            .map(|tree| VtxoRecord {
                outpoint: OutPoint {
                    txid: tree.deployment_txid,
                    vout: 0,
                },
                script: world.contract.vtxo.script_pubkey(),
                amount_sats: value,
                assets: vec![
                    Asset {
                        asset_id: world.tree_asset,
                        amount: 1,
                    },
                    Asset {
                        asset_id: world.log_asset,
                        amount: world.manifest.log_reserve_per_tree,
                    },
                    Asset {
                        asset_id: world.xp_asset,
                        amount: world.manifest.xp_per_tree,
                    },
                ],
                created_at: None,
                expires_at: None,
                is_preconfirmed: false,
                is_swept: false,
                spent_by: None,
                settled_by: None,
                is_unrolled: false,
                is_spent: false,
            })
            .collect());
    }
    let expected = cached.len();
    let mut lineage_states = cached
        .iter()
        .map(|tree| (tree.record.outpoint, tree.state))
        .collect::<std::collections::HashMap<_, _>>();
    let outpoints = cached
        .iter()
        .map(|tree| tree.record.outpoint)
        .collect::<Vec<_>>();
    let mut current = rest.get_vtxos_by_outpoints(&outpoints).await?;
    if current.len() != expected {
        return Err(transient_index_snapshot(format!(
            "index returned {} of {expected} exact tree lineages",
            current.len()
        )));
    }
    let mut visited = current
        .iter()
        .map(|record| record.outpoint)
        .collect::<std::collections::HashSet<_>>();
    let tree_script = world.contract.vtxo.script_pubkey();
    loop {
        if current
            .iter()
            .any(|record| record.is_swept || record.is_unrolled)
        {
            return Err(anyhow!(
                "a tree lineage is no longer cooperatively spendable"
            ));
        }
        let spent = current
            .iter()
            .filter(|record| record.is_spent)
            .cloned()
            .collect::<Vec<_>>();
        if spent.is_empty() {
            return select_tree_records(current, world, world.manifest.dust_sats, expected);
        }
        let candidates = rest
            .get_vtxo_successor_candidates(&spent, &tree_script, world.tree_asset)
            .await
            .map_err(|error| {
                if error.to_string().contains("successor is not indexed yet") {
                    transient_index_snapshot(error.to_string())
                } else {
                    error
                }
            })?;
        let mut successors = Vec::with_capacity(candidates.len());
        for (record, transaction) in candidates {
            let state = tree::tree_state_from_tx(&transaction)?
                .ok_or_else(|| anyhow!("tree successor has no identity packet"))?;
            successors.push((state, record));
        }
        for record in &mut current {
            if !record.is_spent {
                continue;
            }
            let state = lineage_states
                .remove(&record.outpoint)
                .ok_or_else(|| anyhow!("tree lineage lost its identity"))?;
            let direct_txid = record.spent_by;
            let mut matching = successors
                .iter()
                .enumerate()
                .filter(|(_, (candidate_state, candidate))| {
                    *candidate_state == state
                        && direct_txid.is_none_or(|txid| candidate.outpoint.txid == txid)
                })
                .map(|(index, _)| index);
            let index = matching
                .next()
                .ok_or_else(|| transient_index_snapshot("tree successor is not indexed yet"))?;
            if matching.next().is_some() {
                return Err(anyhow!("tree has multiple indexed successors"));
            }
            drop(matching);
            let (_, successor) = successors.swap_remove(index);
            if !visited.insert(successor.outpoint) {
                return Err(anyhow!("tree lineage contains a cycle"));
            }
            lineage_states.insert(successor.outpoint, state);
            *record = successor;
        }
    }
}

fn select_tree_records(
    records: Vec<VtxoRecord>,
    world: &World,
    dust_sats: u64,
    expected: usize,
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
        if record.amount_sats != dust_sats
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
    if trees.len() != expected {
        return Err(anyhow!(
            "woodland.sh exposes {} of {expected} requested trees",
            trees.len()
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
        _ => Err(transient_index_snapshot(
            "PLAYER_ID has multiple recursive state VTXOs",
        )),
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
        txbuild::sleep_ms(INDEX_POLL_MS).await;
    }
    Err(last_error
        .unwrap_or_else(|| anyhow!("virtual transaction lookup failed"))
        .context("indexer did not expose creating virtual transactions"))
}

fn player_vtxo(keys: &Keys, params: &ServerParams) -> Result<ark_core::Vtxo> {
    txbuild::player_vtxo(keys, params)
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
        txbuild::sleep_ms(INDEX_POLL_MS).await;
    }
    Err(anyhow!("indexer did not expose VTXO {outpoint}"))
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
