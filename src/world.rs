//! Immutable shared-world manifest used by setup and browser players.

use crate::arkade::{EmulatorParams, ServerParams};
#[cfg(not(target_arch = "wasm32"))]
use crate::keys::Keys;
use crate::tree::{self, TreeContract, TreeState};
use crate::txbuild;
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use bitcoin::hashes::{sha256, Hash, HashEngine};
use bitcoin::secp256k1::{Message, Secp256k1, Verification};
#[cfg(not(target_arch = "wasm32"))]
use bitcoin::OutPoint;
use bitcoin::Txid;
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::str::FromStr;

pub(crate) const GAME_ID: &str = "woodland.sh";
pub(crate) const PROTOCOL_VERSION: u32 = 4;
pub(crate) const RULESET_ID: &str = "woodland.sh/forest/v4";
/// No-vault world: every fixed-issuance resource starts on 420 recursive
/// trees, and funded stumps regrow in one permissionless renewal batch.
pub(crate) const MANIFEST_SCHEMA_VERSION: u32 = 4;
pub(crate) const PROTOCOL_DUST_SATS: u64 = 330;
pub(crate) const ACTIVE_LOGS_PER_TREE: u64 = 10;
pub(crate) const LOG_RESERVE_PER_TREE: u64 = 50_000;
pub(crate) const XP_PER_TREE: u64 = 50_000;
pub(crate) const STONE_RESERVE_PER_TREE: u64 = 50_000;
pub(crate) const IRON_ORE_RESERVE_PER_TREE: u64 = 50_000;
pub(crate) const TREE_COUNT: usize = 420;
/// Genesis issuance caps, distributed completely across the initial trees.
pub(crate) const LOG_SUPPLY: u64 = 21_000_000;
pub(crate) const XP_SUPPLY: u64 = 21_000_000;
pub(crate) const STONE_SUPPLY: u64 = 21_000_000;
pub(crate) const IRON_ORE_SUPPLY: u64 = 21_000_000;
pub(crate) const MAP_WIDTH: u16 = 425;
pub(crate) const MAP_HEIGHT: u16 = 425;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const PLAYER_SPAWN_X: u16 = 3;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const PLAYER_SPAWN_Y: u16 = 17;
const TREE_ID_START: u32 = 417;
const TREE_LAYOUT_SEED: u64 = 0x574f_4f44_4c41_4e44;
const INITIAL_TREE_STATES: [TreeState; 10] = [
    TreeState {
        tree_id: 417,
        x: 7,
        y: 13,
    },
    TreeState {
        tree_id: 418,
        x: 12,
        y: 4,
    },
    TreeState {
        tree_id: 419,
        x: 22,
        y: 2,
    },
    TreeState {
        tree_id: 420,
        x: 36,
        y: 3,
    },
    TreeState {
        tree_id: 421,
        x: 41,
        y: 8,
    },
    TreeState {
        tree_id: 422,
        x: 29,
        y: 7,
    },
    TreeState {
        tree_id: 423,
        x: 17,
        y: 9,
    },
    TreeState {
        tree_id: 424,
        x: 34,
        y: 12,
    },
    TreeState {
        tree_id: 425,
        x: 20,
        y: 15,
    },
    TreeState {
        tree_id: 426,
        x: 40,
        y: 16,
    },
];

fn layout_sample(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub(crate) fn tree_states() -> Vec<TreeState> {
    let mut states = INITIAL_TREE_STATES.to_vec();
    let mut occupied = states
        .iter()
        .map(|state| (state.x, state.y))
        .collect::<HashSet<_>>();
    let cell_count = u64::from(MAP_WIDTH) * u64::from(MAP_HEIGHT);
    let mut nonce = 0_u64;
    while states.len() < TREE_COUNT {
        let cell = layout_sample(TREE_LAYOUT_SEED.wrapping_add(nonce)) % cell_count;
        nonce += 1;
        let x = (cell % u64::from(MAP_WIDTH)) as u16;
        let y = (cell / u64::from(MAP_WIDTH)) as u16;
        let spawn_distance =
            u32::from(x.abs_diff(PLAYER_SPAWN_X)) + u32::from(y.abs_diff(PLAYER_SPAWN_Y));
        if spawn_distance <= 4 || !occupied.insert((x, y)) {
            continue;
        }
        states.push(TreeState {
            tree_id: TREE_ID_START + states.len() as u32,
            x,
            y,
        });
    }
    states
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestTree {
    pub state: TreeState,
    pub deployment_txid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorldManifest {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub game_id: String,
    pub ruleset_id: String,
    pub network: String,
    pub arkade_service_url: String,
    pub emulator_url: String,
    pub operator_signer: String,
    pub forfeit_pubkey: String,
    pub forfeit_address: String,
    pub emulator_signer: String,
    pub deployer_signer: String,
    pub rollover_signer: String,
    pub unilateral_exit_sequence: u32,
    pub dust_sats: u64,
    pub map_width: u16,
    pub map_height: u16,
    pub tree_asset: String,
    pub log_asset: String,
    pub xp_asset: String,
    pub stone_asset: String,
    pub iron_ore_asset: String,
    pub active_logs_per_tree: u64,
    pub log_reserve_per_tree: u64,
    pub xp_per_tree: u64,
    pub stone_reserve_per_tree: u64,
    pub iron_ore_reserve_per_tree: u64,
    /// User-facing Woodcutting XP represented by each soulbound XP asset unit.
    pub woodcutting_xp_per_log: u64,
    pub player_level_curve: String,
    pub max_player_level: u64,
    pub base_log_drop_basis_points: u64,
    pub level_log_drop_bonus_basis_points: u64,
    pub level_log_drop_xp_thresholds: [u64; 5],
    pub max_level_log_drop_basis_points: u64,
    pub max_log_drop_basis_points: u64,
    pub stone_drop_basis_points: u64,
    pub iron_ore_drop_basis_points: u64,
    pub iron_ore_unlock_level: u64,
    pub axe_recipes: [crate::player::AxeRecipe; 3],
    pub luck_window_basis_points: u64,
    pub initial_luck_credit: u64,
    pub tree_script: String,
    pub tree_chop_arkade_script: String,
    pub tree_regrowth_arkade_script: String,
    pub tree_maintenance_arkade_script: String,
    pub genesis_txid: String,
    pub trees: Vec<ManifestTree>,
    pub manifest_signature: String,
}

#[derive(Clone, Copy)]
pub struct ValidatedTree {
    pub state: TreeState,
    pub deployment_txid: Txid,
}

/// An exact lineage endpoint which the indexer has not exposed yet. These
/// gaps are safe to defer during upkeep, unlike malformed or forked lineages.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PendingTreeLineage {
    Deployment { tree_id: u32, outpoint: OutPoint },
    Successor { tree_id: u32, outpoint: OutPoint },
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Display for PendingTreeLineage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deployment { tree_id, outpoint } => {
                write!(
                    formatter,
                    "tree {tree_id} deployment {outpoint} is not indexed yet"
                )
            }
            Self::Successor { tree_id, outpoint } => {
                write!(
                    formatter,
                    "tree {tree_id} successor of {outpoint} is not indexed yet"
                )
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct TreeLineageRecords {
    pub current: Vec<crate::arkade::VtxoRecord>,
    pub pending: Vec<PendingTreeLineage>,
}

#[cfg(not(target_arch = "wasm32"))]
struct TreeLineageScan {
    records: TreeLineageRecords,
    states: std::collections::HashMap<OutPoint, TreeState>,
    visited: HashSet<OutPoint>,
}

#[cfg(not(target_arch = "wasm32"))]
impl TreeLineageScan {
    fn new(declared: &[ValidatedTree], current: Vec<crate::arkade::VtxoRecord>) -> Result<Self> {
        let mut states = std::collections::HashMap::with_capacity(declared.len());
        for tree in declared {
            let outpoint = OutPoint {
                txid: tree.deployment_txid,
                vout: 0,
            };
            if states.insert(outpoint, tree.state).is_some() {
                return Err(anyhow!("duplicate declared tree lineage {outpoint}"));
            }
        }
        let mut visited = HashSet::with_capacity(current.len());
        for record in &current {
            if !states.contains_key(&record.outpoint) || !visited.insert(record.outpoint) {
                return Err(anyhow!(
                    "unexpected or duplicate exact tree lineage {}",
                    record.outpoint
                ));
            }
        }
        let mut pending = Vec::new();
        for tree in declared {
            let outpoint = OutPoint {
                txid: tree.deployment_txid,
                vout: 0,
            };
            if !visited.contains(&outpoint) {
                states.remove(&outpoint);
                pending.push(PendingTreeLineage::Deployment {
                    tree_id: tree.state.tree_id,
                    outpoint,
                });
            }
        }
        Ok(Self {
            records: TreeLineageRecords { current, pending },
            states,
            visited,
        })
    }

    fn advance(
        &mut self,
        mut successors: Vec<(TreeState, crate::arkade::VtxoRecord)>,
    ) -> Result<()> {
        let mut next = Vec::with_capacity(self.records.current.len());
        for record in self.records.current.drain(..) {
            if !record.is_spent {
                next.push(record);
                continue;
            }
            let state = self
                .states
                .remove(&record.outpoint)
                .ok_or_else(|| anyhow!("tree lineage lost its identity"))?;
            let mut matching = successors
                .iter()
                .enumerate()
                .filter(|(_, (candidate_state, candidate))| {
                    *candidate_state == state
                        && record
                            .spent_by
                            .is_none_or(|txid| candidate.outpoint.txid == txid)
                })
                .map(|(index, _)| index);
            let index = matching.next();
            if matching.next().is_some() {
                return Err(anyhow!(
                    "tree {} has multiple indexed successors",
                    state.tree_id
                ));
            }
            let Some(index) = index else {
                if record.spent_by.is_some_and(|txid| {
                    successors
                        .iter()
                        .any(|(_, candidate)| candidate.outpoint.txid == txid)
                }) {
                    return Err(anyhow!(
                        "tree {} successor has the wrong identity",
                        state.tree_id
                    ));
                }
                self.records.pending.push(PendingTreeLineage::Successor {
                    tree_id: state.tree_id,
                    outpoint: record.outpoint,
                });
                continue;
            };
            let (_, successor) = successors.swap_remove(index);
            if !self.visited.insert(successor.outpoint) {
                return Err(anyhow!("tree {} lineage contains a cycle", state.tree_id));
            }
            self.states.insert(successor.outpoint, state);
            next.push(successor);
        }
        self.records.current = next;
        Ok(())
    }
}

/// Manifest-pinned Arkade service identity. Batch validation and forfeit
/// signing use these values instead of endpoint-supplied parameters, so a
/// spoofed or redirected Arkade endpoint cannot substitute its own operator
/// key or forfeit payout.
#[derive(Clone, Debug)]
pub struct WorldPins {
    pub operator_signer: bitcoin::XOnlyPublicKey,
    pub forfeit_pk: bitcoin::PublicKey,
    pub forfeit_address: bitcoin::Address,
}

pub struct ValidatedWorld {
    pub tree_asset: AssetId,
    pub log_asset: AssetId,
    pub xp_asset: AssetId,
    pub stone_asset: AssetId,
    pub iron_ore_asset: AssetId,
    pub genesis_txid: Txid,
    pub trees: Vec<ValidatedTree>,
    pub deployer_signer: bitcoin::XOnlyPublicKey,
    /// Used by the native and browser batch flows; the browser reads the
    /// pins through the manifest instead.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub pins: WorldPins,
    pub rollover_signer: bitcoin::XOnlyPublicKey,
    pub contract: TreeContract,
}

impl WorldManifest {
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        params: &ServerParams,
        emulator: &EmulatorParams,
        deployer_keys: &Keys,
        arkade_service_url: &str,
        emulator_url: &str,
        rollover_signer: bitcoin::XOnlyPublicKey,
        tree_asset: AssetId,
        log_asset: AssetId,
        xp_asset: AssetId,
        stone_asset: AssetId,
        iron_ore_asset: AssetId,
        contract: &TreeContract,
        genesis_txid: Txid,
        deployments: &[(TreeState, Txid)],
    ) -> Result<Self> {
        let mut manifest = Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            protocol_version: PROTOCOL_VERSION,
            game_id: GAME_ID.to_string(),
            ruleset_id: RULESET_ID.to_string(),
            network: params.network.to_string(),
            arkade_service_url: arkade_service_url.trim_end_matches('/').to_owned(),
            emulator_url: emulator_url.trim_end_matches('/').to_owned(),
            operator_signer: params.signer_pk.to_string(),
            forfeit_pubkey: params.forfeit_pk.to_string(),
            forfeit_address: params.forfeit_address.to_string(),
            emulator_signer: emulator.signer_pk.to_string(),
            deployer_signer: deployer_keys.owner_pk().to_string(),
            rollover_signer: rollover_signer.to_string(),
            unilateral_exit_sequence: params.unilateral_exit_delay.to_consensus_u32(),
            dust_sats: params.dust_sats,
            map_width: MAP_WIDTH,
            map_height: MAP_HEIGHT,
            tree_asset: tree_asset.to_string(),
            log_asset: log_asset.to_string(),
            xp_asset: xp_asset.to_string(),
            stone_asset: stone_asset.to_string(),
            iron_ore_asset: iron_ore_asset.to_string(),
            active_logs_per_tree: ACTIVE_LOGS_PER_TREE,
            log_reserve_per_tree: LOG_RESERVE_PER_TREE,
            xp_per_tree: XP_PER_TREE,
            stone_reserve_per_tree: STONE_RESERVE_PER_TREE,
            iron_ore_reserve_per_tree: IRON_ORE_RESERVE_PER_TREE,
            woodcutting_xp_per_log: crate::player::WOODCUTTING_XP_PER_LOG,
            player_level_curve: crate::player::PLAYER_LEVEL_CURVE.to_string(),
            max_player_level: crate::player::MAX_PLAYER_LEVEL,
            base_log_drop_basis_points: crate::player::BASE_LOG_DROP_BASIS_POINTS,
            level_log_drop_bonus_basis_points: crate::player::LEVEL_LOG_DROP_BONUS_BASIS_POINTS,
            level_log_drop_xp_thresholds: crate::player::LEVEL_LOG_DROP_XP_THRESHOLDS,
            max_level_log_drop_basis_points: crate::player::MAX_LEVEL_LOG_DROP_BASIS_POINTS,
            max_log_drop_basis_points: crate::player::MAX_LOG_DROP_BASIS_POINTS,
            stone_drop_basis_points: crate::player::STONE_DROP_BASIS_POINTS,
            iron_ore_drop_basis_points: crate::player::IRON_ORE_DROP_BASIS_POINTS,
            iron_ore_unlock_level: crate::player::IRON_ORE_UNLOCK_LEVEL,
            axe_recipes: crate::player::AXE_RECIPES,
            luck_window_basis_points: crate::player::LUCK_WINDOW_BASIS_POINTS,
            initial_luck_credit: crate::player::INITIAL_LUCK_CREDIT,
            tree_script: contract.vtxo.script_pubkey().to_hex_string(),
            tree_chop_arkade_script: contract.chop_arkade_script.to_hex_string(),
            tree_regrowth_arkade_script: contract.regrowth_arkade_script.to_hex_string(),
            tree_maintenance_arkade_script: contract.maintenance_arkade_script.to_hex_string(),
            genesis_txid: genesis_txid.to_string(),
            trees: deployments
                .iter()
                .map(|(state, deployment_txid)| ManifestTree {
                    state: *state,
                    deployment_txid: deployment_txid.to_string(),
                })
                .collect(),
            manifest_signature: String::new(),
        };
        manifest.sign(deployer_keys)?;
        Ok(manifest)
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn sign(&mut self, deployer_keys: &Keys) -> Result<()> {
        if self.deployer_signer != deployer_keys.owner_pk().to_string() {
            return Err(anyhow!(
                "manifest deployer signer does not match its signing key"
            ));
        }
        let message = self.signature_message()?;
        let signatures = deployer_keys.sign_msg(&message);
        let [(signature, signer)] = signatures.as_slice() else {
            return Err(anyhow!("manifest signer returned an invalid signature set"));
        };
        if *signer != deployer_keys.owner_pk() {
            return Err(anyhow!("manifest signer returned the wrong public key"));
        }
        self.manifest_signature = signature.to_string();
        Ok(())
    }

    pub fn verify_authenticity<C: Verification>(
        &self,
        secp: &Secp256k1<C>,
    ) -> Result<bitcoin::XOnlyPublicKey> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION
            || self.protocol_version != PROTOCOL_VERSION
            || self.game_id != GAME_ID
            || self.ruleset_id != RULESET_ID
        {
            return Err(anyhow!("world manifest has an unsupported signed ruleset"));
        }
        let deployer_signer = self
            .deployer_signer
            .parse::<bitcoin::XOnlyPublicKey>()
            .context("parse manifest deployer signer")?;
        let signature = self
            .manifest_signature
            .parse::<bitcoin::secp256k1::schnorr::Signature>()
            .context("parse manifest signature")?;
        if signature.to_string() != self.manifest_signature {
            return Err(anyhow!("manifest signature is not canonically encoded"));
        }
        secp.verify_schnorr(&signature, &self.signature_message()?, &deployer_signer)
            .context("verify manifest deployer signature")?;
        Ok(deployer_signer)
    }

    fn signature_message(&self) -> Result<Message> {
        let mut unsigned = self.clone();
        unsigned.manifest_signature.clear();
        let value = serde_json::to_value(unsigned).context("encode manifest signing value")?;
        let mut canonical = Vec::new();
        write_canonical_json(&value, &mut canonical)?;
        let mut engine = sha256::Hash::engine();
        engine.input(b"woodland.sh/world-manifest/v4\0");
        engine.input(&canonical);
        Ok(Message::from_digest(
            sha256::Hash::from_engine(engine).to_byte_array(),
        ))
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let manifest: Self =
            serde_json::from_str(json).context("parse woodland.sh world manifest")?;
        manifest.verify_authenticity(&Secp256k1::verification_only())?;
        Ok(manifest)
    }

    /// Parse the pinned service identity, checking the forfeit address
    /// against the expected network. Meaningful only after [`Self::validate`]
    /// has bound the manifest to the live services.
    pub fn pins(&self, network: bitcoin::Network) -> Result<WorldPins> {
        let operator_signer = self
            .operator_signer
            .parse()
            .context("parse world operator signer")?;
        let forfeit_pk = self
            .forfeit_pubkey
            .parse()
            .context("parse world forfeit pubkey")?;
        let forfeit_address = self
            .forfeit_address
            .parse::<bitcoin::Address<_>>()
            .context("parse world forfeit address")?
            .require_network(network)
            .context("world forfeit address network mismatch")?;
        Ok(WorldPins {
            operator_signer,
            forfeit_pk,
            forfeit_address,
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("serialize woodland.sh world manifest")
    }

    pub fn validate<C: Verification>(
        &self,
        secp: &Secp256k1<C>,
        params: &ServerParams,
        emulator: &EmulatorParams,
    ) -> Result<ValidatedWorld> {
        let deployer_signer = self.verify_authenticity(secp)?;
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported woodland.sh world manifest schema {}; deploy schema {MANIFEST_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        let rollover_signer: bitcoin::XOnlyPublicKey = self
            .rollover_signer
            .parse()
            .context("parse world rollover signer")?;
        if self.protocol_version != PROTOCOL_VERSION
            || self.game_id != GAME_ID
            || self.ruleset_id != RULESET_ID
            || self.network != params.network.to_string()
            || self.operator_signer != params.signer_pk.to_string()
            || self.forfeit_pubkey != params.forfeit_pk.to_string()
            || self.forfeit_address != params.forfeit_address.to_string()
            || self.emulator_signer != emulator.signer_pk.to_string()
            || self.unilateral_exit_sequence != params.unilateral_exit_delay.to_consensus_u32()
            || self.dust_sats != params.dust_sats
            || self.arkade_service_url.is_empty()
            || self.emulator_url.is_empty()
            || (params.network == bitcoin::Network::Bitcoin
                && (!self.arkade_service_url.starts_with("https://")
                    || !self.emulator_url.starts_with("https://")))
        {
            return Err(anyhow!(
                "woodland.sh world manifest does not match the running services"
            ));
        }
        if [
            params.signer_pk,
            emulator.signer_pk,
            rollover_signer,
            deployer_signer,
        ]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
            != 4
        {
            return Err(anyhow!(
                "world deployer and service signers must be distinct"
            ));
        }
        if self.map_width != MAP_WIDTH
            || self.map_height != MAP_HEIGHT
            || self.dust_sats != PROTOCOL_DUST_SATS
            || self.active_logs_per_tree != ACTIVE_LOGS_PER_TREE
            || self.active_logs_per_tree != tree::LOGS_PER_TREE
            || self.log_reserve_per_tree != LOG_RESERVE_PER_TREE
            || self.xp_per_tree != XP_PER_TREE
            || self.stone_reserve_per_tree != STONE_RESERVE_PER_TREE
            || self.iron_ore_reserve_per_tree != IRON_ORE_RESERVE_PER_TREE
            || self.woodcutting_xp_per_log != crate::player::WOODCUTTING_XP_PER_LOG
            || self.player_level_curve != crate::player::PLAYER_LEVEL_CURVE
            || self.max_player_level != crate::player::MAX_PLAYER_LEVEL
            || self.base_log_drop_basis_points != crate::player::BASE_LOG_DROP_BASIS_POINTS
            || self.level_log_drop_bonus_basis_points
                != crate::player::LEVEL_LOG_DROP_BONUS_BASIS_POINTS
            || self.level_log_drop_xp_thresholds != crate::player::LEVEL_LOG_DROP_XP_THRESHOLDS
            || self.max_level_log_drop_basis_points
                != crate::player::MAX_LEVEL_LOG_DROP_BASIS_POINTS
            || self.max_log_drop_basis_points != crate::player::MAX_LOG_DROP_BASIS_POINTS
            || self.stone_drop_basis_points != crate::player::STONE_DROP_BASIS_POINTS
            || self.iron_ore_drop_basis_points != crate::player::IRON_ORE_DROP_BASIS_POINTS
            || self.iron_ore_unlock_level != crate::player::IRON_ORE_UNLOCK_LEVEL
            || self.axe_recipes != crate::player::AXE_RECIPES
            || self.luck_window_basis_points != crate::player::LUCK_WINDOW_BASIS_POINTS
            || self.initial_luck_credit != crate::player::INITIAL_LUCK_CREDIT
            || self.trees.len() != TREE_COUNT
        {
            return Err(anyhow!("woodland.sh world manifest shape is invalid"));
        }

        let tree_asset = txbuild::parse_asset_id_pub(&self.tree_asset)
            .ok_or_else(|| anyhow!("invalid TREE asset ID in world manifest"))?;
        let log_asset = txbuild::parse_asset_id_pub(&self.log_asset)
            .ok_or_else(|| anyhow!("invalid LOG asset ID in world manifest"))?;
        let xp_asset = txbuild::parse_asset_id_pub(&self.xp_asset)
            .ok_or_else(|| anyhow!("invalid XP asset ID in world manifest"))?;
        let stone_asset = txbuild::parse_asset_id_pub(&self.stone_asset)
            .ok_or_else(|| anyhow!("invalid STONE asset ID in world manifest"))?;
        let iron_ore_asset = txbuild::parse_asset_id_pub(&self.iron_ore_asset)
            .ok_or_else(|| anyhow!("invalid IRON ORE asset ID in world manifest"))?;
        let genesis_txid =
            Txid::from_str(&self.genesis_txid).context("parse world genesis transaction ID")?;
        if tree_asset.txid != genesis_txid
            || log_asset.txid != genesis_txid
            || xp_asset.txid != genesis_txid
            || stone_asset.txid != genesis_txid
            || iron_ore_asset.txid != genesis_txid
            || tree_asset.group_index != 0
            || log_asset.group_index != 1
            || xp_asset.group_index != 2
            || stone_asset.group_index != 3
            || iron_ore_asset.group_index != 4
        {
            return Err(anyhow!(
                "world asset IDs do not match the canonical genesis"
            ));
        }

        let expected_states: HashSet<_> = tree_states().into_iter().collect();
        let mut states = HashSet::new();
        let mut tree_ids = HashSet::new();
        let mut coordinates = HashSet::new();
        let mut deployment_txids = HashSet::new();
        let mut trees = Vec::with_capacity(self.trees.len());
        for tree in &self.trees {
            let deployment_txid = Txid::from_str(&tree.deployment_txid)
                .context("parse tree deployment transaction ID")?;
            if tree.state.x >= self.map_width
                || tree.state.y >= self.map_height
                || !states.insert(tree.state)
                || !tree_ids.insert(tree.state.tree_id)
                || !coordinates.insert((tree.state.x, tree.state.y))
                || !deployment_txids.insert(deployment_txid)
                || deployment_txid == genesis_txid
            {
                return Err(anyhow!("world manifest contains an invalid tree entry"));
            }
            trees.push(ValidatedTree {
                state: tree.state,
                deployment_txid,
            });
        }
        if states != expected_states {
            return Err(anyhow!("world manifest tree layout mismatch"));
        }

        let contract = tree::build_tree_contract(
            secp,
            params.signer_pk,
            emulator.signer_pk,
            rollover_signer,
            params.unilateral_exit_delay,
            params.network,
            tree_asset,
            log_asset,
            xp_asset,
            stone_asset,
            iron_ore_asset,
            params.dust_sats,
        )?;
        if self.tree_script != contract.vtxo.script_pubkey().to_hex_string()
            || self.tree_chop_arkade_script != contract.chop_arkade_script.to_hex_string()
            || self.tree_regrowth_arkade_script != contract.regrowth_arkade_script.to_hex_string()
            || self.tree_maintenance_arkade_script
                != contract.maintenance_arkade_script.to_hex_string()
        {
            return Err(anyhow!("world manifest covenant script mismatch"));
        }

        Ok(ValidatedWorld {
            tree_asset,
            log_asset,
            xp_asset,
            stone_asset,
            iron_ore_asset,
            genesis_txid,
            trees,
            deployer_signer,
            pins: self.pins(params.network)?,
            rollover_signer,
            contract,
        })
    }
}
fn write_canonical_json(value: &serde_json::Value, output: &mut Vec<u8>) -> Result<()> {
    match value {
        serde_json::Value::Null => output.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => {
            output.extend_from_slice(if *value { b"true" } else { b"false" })
        }
        serde_json::Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::String(value) => output.extend_from_slice(
            serde_json::to_string(value)
                .context("encode canonical JSON string")?
                .as_bytes(),
        ),
        serde_json::Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        serde_json::Value::Object(values) => {
            output.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    serde_json::to_string(key)
                        .context("encode canonical JSON key")?
                        .as_bytes(),
                );
                output.push(b':');
                write_canonical_json(
                    values
                        .get(key)
                        .expect("canonical JSON key came from this object"),
                    output,
                )?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}
pub(crate) fn asset_metadata_entries(
    label: &str,
    deployer_signer: bitcoin::XOnlyPublicKey,
    rollover_signer: bitcoin::XOnlyPublicKey,
) -> Vec<(String, String)> {
    vec![
        ("game".to_string(), GAME_ID.to_string()),
        ("protocol".to_string(), PROTOCOL_VERSION.to_string()),
        ("ruleset".to_string(), RULESET_ID.to_string()),
        ("asset".to_string(), label.to_string()),
        ("deployer".to_string(), deployer_signer.to_string()),
        ("rollover".to_string(), rollover_signer.to_string()),
    ]
}

fn expected_asset_metadata(
    label: &str,
    deployer_signer: bitcoin::XOnlyPublicKey,
    rollover_signer: bitcoin::XOnlyPublicKey,
) -> Vec<u8> {
    let entries = asset_metadata_entries(label, deployer_signer, rollover_signer);
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

fn indexed_supply_is_valid(current: u64, genesis: u64, burnable: bool) -> bool {
    if burnable {
        current <= genesis
    } else {
        current == genesis
    }
}

impl ValidatedWorld {
    /// Follow every declared tree to its current record, failing closed on
    /// indexing gaps as well as invalid lineages. Strict gameplay callers must
    /// not treat a temporarily absent successor as an absent tree.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn load_tree_lineage_records(
        &self,
        rest: &crate::arkade::ArkadeRest,
        declared: &[ValidatedTree],
    ) -> Result<Vec<crate::arkade::VtxoRecord>> {
        let records = self
            .load_tree_lineage_records_partial(rest, declared)
            .await?;
        if let Some(pending) = records.pending.first() {
            return Err(anyhow!(
                "{pending} ({} pending tree lineage(s))",
                records.pending.len()
            ));
        }
        Ok(records.current)
    }

    /// Resolve independent tree lineages for operator maintenance. Only absent
    /// exact deployment/successor records are returned as `pending`; transport,
    /// decoding, identity, fork, and cycle errors still fail the entire call.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) async fn load_tree_lineage_records_partial(
        &self,
        rest: &crate::arkade::ArkadeRest,
        declared: &[ValidatedTree],
    ) -> Result<TreeLineageRecords> {
        let outpoints = declared
            .iter()
            .map(|tree| OutPoint {
                txid: tree.deployment_txid,
                vout: 0,
            })
            .collect::<Vec<_>>();
        let mut scan =
            TreeLineageScan::new(declared, rest.get_vtxos_by_outpoints(&outpoints).await?)?;
        let tree_script = self.contract.vtxo.script_pubkey();
        loop {
            for record in &scan.records.current {
                if record.script != tree_script || record.asset_amount(self.tree_asset) != Some(1) {
                    return Err(anyhow!("invalid tree lineage record {}", record.outpoint));
                }
            }
            let spent = scan
                .records
                .current
                .iter()
                .filter(|record| record.is_spent)
                .cloned()
                .collect::<Vec<_>>();
            if spent.is_empty() {
                return Ok(scan.records);
            }
            let candidates = rest
                .get_vtxo_successor_candidates(&spent, &tree_script, self.tree_asset)
                .await?;
            let mut successors = Vec::with_capacity(candidates.len());
            for (record, transaction) in candidates {
                record.validate_creating_transaction(&transaction)?;
                let state = tree::tree_state_from_tx(&transaction)?
                    .ok_or_else(|| anyhow!("tree successor has no identity packet"))?;
                successors.push((state, record));
            }
            scan.advance(successors)?;
        }
    }

    pub async fn verify_indexed_assets(&self, rest: &crate::arkade::ArkadeRest) -> Result<()> {
        stream::iter([
            (self.tree_asset, "TREE", self.trees.len() as u64, false),
            (self.log_asset, "LOG", LOG_SUPPLY, true),
            (self.xp_asset, "XP", XP_SUPPLY, false),
            (self.stone_asset, "STONE", STONE_SUPPLY, true),
            (self.iron_ore_asset, "IRON ORE", IRON_ORE_SUPPLY, true),
        ])
        .map(|(asset_id, label, genesis_supply, burnable)| async move {
            let details = rest
                .get_asset_details(asset_id)
                .await
                .with_context(|| format!("verify indexed {label} asset"))?;
            if details.control_asset.is_some()
                || !indexed_supply_is_valid(details.supply, genesis_supply, burnable)
                || details.metadata
                    != expected_asset_metadata(label, self.deployer_signer, self.rollover_signer)
            {
                return Err(anyhow!(
                    "indexed {label} asset does not match the fixed-issuance world genesis"
                ));
            }
            Ok(())
        })
        // Fetch all five assets together, retaining the original error order.
        .buffered(5)
        .try_for_each(|()| async { Ok(()) })
        .await
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Keypair, SecretKey};
    use bitcoin::{Address, Network, PublicKey, Sequence};

    pub(crate) fn fixture() -> (
        Secp256k1<bitcoin::secp256k1::All>,
        ServerParams,
        EmulatorParams,
        WorldManifest,
    ) {
        let secp = Secp256k1::new();
        let operator_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let emulator_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let rollover_secret = SecretKey::from_slice(&[7; 32]).unwrap();
        let deployer_keys = Keys::from_hex(&"08".repeat(32)).unwrap();
        let operator_keypair = Keypair::from_secret_key(&secp, &operator_secret);
        let operator = operator_keypair.x_only_public_key().0;
        let emulator = Keypair::from_secret_key(&secp, &emulator_secret)
            .x_only_public_key()
            .0;
        let rollover = Keypair::from_secret_key(&secp, &rollover_secret)
            .x_only_public_key()
            .0;
        let params = ServerParams {
            version: "test".to_string(),
            signer_pk: operator,
            forfeit_pk: PublicKey::new(operator_keypair.public_key()),
            network: Network::Regtest,
            dust_sats: 330,
            vtxo_min_sats: 1,
            unilateral_exit_delay: Sequence::from_height(144),
            max_tx_weight: 40_000,
            max_op_return_outputs: 3,
            zero_offchain_fees: true,
            checkpoint_tapscript: bitcoin::ScriptBuf::new(),
            forfeit_address: Address::p2tr(&secp, operator, None, Network::Regtest),
        };
        let emulator_params = EmulatorParams {
            version: "test".to_string(),
            signer_pk: emulator,
        };
        let genesis_txid = Txid::from_byte_array([7; 32]);
        let tree_asset = AssetId {
            txid: genesis_txid,
            group_index: 0,
        };
        let log_asset = AssetId {
            txid: genesis_txid,
            group_index: 1,
        };
        let xp_asset = AssetId {
            txid: genesis_txid,
            group_index: 2,
        };
        let stone_asset = AssetId {
            txid: genesis_txid,
            group_index: 3,
        };
        let iron_ore_asset = AssetId {
            txid: genesis_txid,
            group_index: 4,
        };
        let contract = tree::build_tree_contract(
            &secp,
            operator,
            emulator,
            rollover,
            params.unilateral_exit_delay,
            params.network,
            tree_asset,
            log_asset,
            xp_asset,
            stone_asset,
            iron_ore_asset,
            params.dust_sats,
        )
        .unwrap();
        let deployments: Vec<_> = tree_states()
            .into_iter()
            .enumerate()
            .map(|(index, state)| {
                let mut txid = [0_u8; 32];
                txid[..8].copy_from_slice(&(index as u64 + 8).to_le_bytes());
                (state, Txid::from_byte_array(txid))
            })
            .collect();
        let manifest = WorldManifest::new(
            &params,
            &emulator_params,
            &deployer_keys,
            "http://127.0.0.1:7070",
            "http://127.0.0.1:7073",
            rollover,
            tree_asset,
            log_asset,
            xp_asset,
            stone_asset,
            iron_ore_asset,
            &contract,
            genesis_txid,
            &deployments,
        )
        .unwrap();
        (secp, params, emulator_params, manifest)
    }

    #[test]
    fn manifest_round_trips_exactly_declared_trees() {
        let (secp, params, emulator, manifest) = fixture();
        let json = manifest.to_json().unwrap();
        assert!(json.contains("\"manifestSignature\""));
        assert!(json.contains("\"xpAsset\""));
        assert!(json.contains("\"xpPerTree\""));
        assert!(json.contains("\"stoneAsset\""));
        assert!(json.contains("\"ironOreAsset\""));
        assert!(json.contains("\"axeRecipes\""));
        assert!(json.contains("\"woodcuttingXpPerLog\""));
        let parsed = WorldManifest::from_json(&json).unwrap();
        let world = parsed.validate(&secp, &params, &emulator).unwrap();
        assert_eq!(world.trees.len(), TREE_COUNT);
        assert_eq!(world.trees[0].state, tree_states()[0]);
        assert_eq!(world.stone_asset.to_string(), parsed.stone_asset);
        assert_eq!(world.iron_ore_asset.to_string(), parsed.iron_ore_asset);
        assert_eq!(world.xp_asset.to_string(), parsed.xp_asset);
        assert_eq!(parsed.arkade_service_url, "http://127.0.0.1:7070");
        assert_eq!(parsed.emulator_url, "http://127.0.0.1:7073");
        assert_eq!(parsed.game_id, GAME_ID);
        assert_eq!(parsed.protocol_version, PROTOCOL_VERSION);
        assert_eq!(parsed.player_level_curve, crate::player::PLAYER_LEVEL_CURVE);
        assert_eq!(parsed.max_player_level, crate::player::MAX_PLAYER_LEVEL);
        assert_eq!(
            parsed.woodcutting_xp_per_log,
            crate::player::WOODCUTTING_XP_PER_LOG
        );
        assert_eq!(
            parsed.level_log_drop_xp_thresholds,
            crate::player::LEVEL_LOG_DROP_XP_THRESHOLDS
        );
        assert_eq!(
            parsed.max_level_log_drop_basis_points,
            crate::player::MAX_LEVEL_LOG_DROP_BASIS_POINTS
        );
        assert_eq!(
            parsed.max_log_drop_basis_points,
            crate::player::MAX_LOG_DROP_BASIS_POINTS
        );
        assert_eq!(
            parsed.stone_drop_basis_points,
            crate::player::STONE_DROP_BASIS_POINTS
        );
        assert_eq!(
            parsed.iron_ore_drop_basis_points,
            crate::player::IRON_ORE_DROP_BASIS_POINTS
        );
        assert_eq!(
            parsed.iron_ore_unlock_level,
            crate::player::IRON_ORE_UNLOCK_LEVEL
        );
        assert_eq!(parsed.axe_recipes, crate::player::AXE_RECIPES);
        assert_eq!(parsed.stone_reserve_per_tree, STONE_RESERVE_PER_TREE);
        assert_eq!(parsed.iron_ore_reserve_per_tree, IRON_ORE_RESERVE_PER_TREE);
        assert_eq!(
            parsed.luck_window_basis_points,
            crate::player::LUCK_WINDOW_BASIS_POINTS
        );
        assert_eq!(
            parsed.initial_luck_credit,
            crate::player::INITIAL_LUCK_CREDIT
        );
        assert_eq!(
            world.contract.chop_arkade_script.to_hex_string(),
            parsed.tree_chop_arkade_script
        );
        assert_eq!(
            world.contract.regrowth_arkade_script.to_hex_string(),
            parsed.tree_regrowth_arkade_script
        );
        assert_eq!(
            world.contract.maintenance_arkade_script.to_hex_string(),
            parsed.tree_maintenance_arkade_script
        );
    }

    #[test]
    fn manifest_rejects_previous_protocol_assets_and_ruleset() {
        let (_, _, _, manifest) = fixture();
        for mutate in [
            |manifest: &mut WorldManifest| manifest.schema_version = 3,
            |manifest: &mut WorldManifest| manifest.protocol_version = 3,
            |manifest: &mut WorldManifest| manifest.ruleset_id = "woodland.sh/forest/v3".into(),
        ] {
            let mut legacy = manifest.clone();
            mutate(&mut legacy);
            let json = serde_json::to_string(&legacy).unwrap();
            let error = WorldManifest::from_json(&json).unwrap_err();
            assert!(error.to_string().contains("unsupported signed ruleset"));
        }
    }

    #[test]
    fn manifest_deserialization_rejects_unsigned_or_tampered_authority() {
        let (_, _, _, manifest) = fixture();

        let mut changed_url = manifest.clone();
        changed_url.arkade_service_url = "http://127.0.0.1:7999".to_string();
        let json = serde_json::to_string(&changed_url).unwrap();
        assert!(WorldManifest::from_json(&json).is_err());

        let mut unsigned = manifest;
        unsigned.manifest_signature.clear();
        let json = serde_json::to_string(&unsigned).unwrap();
        assert!(WorldManifest::from_json(&json).is_err());
    }

    #[test]
    fn manifest_rejects_missing_duplicate_or_moved_trees() {
        let (secp, params, emulator, manifest) = fixture();

        let mut missing = manifest.clone();
        missing.trees.pop();
        assert!(missing.validate(&secp, &params, &emulator).is_err());

        let mut duplicate = manifest.clone();
        duplicate.trees[1].state = duplicate.trees[0].state;
        assert!(duplicate.validate(&secp, &params, &emulator).is_err());

        let mut moved = manifest;
        moved.trees[0].state.x += 1;
        assert!(moved.validate(&secp, &params, &emulator).is_err());
    }

    #[test]
    fn manifest_rejects_changed_tree_leaf_commitments() {
        let (secp, params, emulator, manifest) = fixture();

        let mut regrowth = manifest.clone();
        regrowth.tree_regrowth_arkade_script.push_str("00");
        assert!(regrowth.validate(&secp, &params, &emulator).is_err());

        let mut maintenance = manifest.clone();
        maintenance.tree_maintenance_arkade_script.push_str("00");
        assert!(maintenance.validate(&secp, &params, &emulator).is_err());

        let mut chop = manifest;
        chop.tree_chop_arkade_script.push_str("00");
        assert!(chop.validate(&secp, &params, &emulator).is_err());
    }

    #[test]
    fn manifest_rejects_changed_service_identity() {
        let (secp, params, emulator, manifest) = fixture();
        for mutate in [
            |manifest: &mut WorldManifest| manifest.operator_signer.push('0'),
            |manifest: &mut WorldManifest| manifest.emulator_signer.push('0'),
            |manifest: &mut WorldManifest| manifest.forfeit_pubkey.push('0'),
            |manifest: &mut WorldManifest| {
                manifest.forfeit_address = Address::p2tr(
                    &Secp256k1::new(),
                    SecretKey::from_slice(&[9; 32])
                        .unwrap()
                        .public_key(&Secp256k1::new())
                        .x_only_public_key()
                        .0,
                    None,
                    Network::Regtest,
                )
                .to_string();
            },
        ] {
            let mut changed = manifest.clone();
            mutate(&mut changed);
            assert!(changed.validate(&secp, &params, &emulator).is_err());
        }
    }

    #[test]
    fn manifest_rejects_changed_gameplay_policy() {
        let (secp, params, emulator, manifest) = fixture();
        for mutate in [
            |manifest: &mut WorldManifest| manifest.protocol_version += 1,
            |manifest: &mut WorldManifest| manifest.base_log_drop_basis_points += 1,
            |manifest: &mut WorldManifest| manifest.level_log_drop_bonus_basis_points += 1,
            |manifest: &mut WorldManifest| manifest.level_log_drop_xp_thresholds[0] -= 1,
            |manifest: &mut WorldManifest| manifest.max_level_log_drop_basis_points += 1,
            |manifest: &mut WorldManifest| manifest.max_log_drop_basis_points += 1,
            |manifest: &mut WorldManifest| manifest.stone_drop_basis_points += 1,
            |manifest: &mut WorldManifest| manifest.iron_ore_drop_basis_points += 1,
            |manifest: &mut WorldManifest| manifest.iron_ore_unlock_level += 1,
            |manifest: &mut WorldManifest| manifest.axe_recipes[0].log_cost += 1,
            |manifest: &mut WorldManifest| manifest.luck_window_basis_points += 1,
            |manifest: &mut WorldManifest| manifest.initial_luck_credit += 1,
            |manifest: &mut WorldManifest| manifest.max_player_level += 1,
            |manifest: &mut WorldManifest| manifest.log_reserve_per_tree += 1,
            |manifest: &mut WorldManifest| manifest.xp_per_tree += 1,
            |manifest: &mut WorldManifest| manifest.stone_reserve_per_tree += 1,
            |manifest: &mut WorldManifest| manifest.iron_ore_reserve_per_tree += 1,
            |manifest: &mut WorldManifest| manifest.woodcutting_xp_per_log += 1,
        ] {
            let mut changed = manifest.clone();
            mutate(&mut changed);
            assert!(changed.validate(&secp, &params, &emulator).is_err());
        }
        let mut renamed = manifest;
        renamed.game_id = "not-woodland.sh".to_string();
        assert!(renamed.validate(&secp, &params, &emulator).is_err());
        let (_, _, _, mut manifest) = fixture();
        manifest.player_level_curve = "not-woodland-xp".to_string();
        assert!(manifest.validate(&secp, &params, &emulator).is_err());
        let (_, _, _, mut manifest) = fixture();
        manifest.xp_asset = manifest.log_asset.clone();
        assert!(manifest.validate(&secp, &params, &emulator).is_err());
        let (_, _, _, mut manifest) = fixture();
        manifest.stone_asset = manifest.log_asset.clone();
        assert!(manifest.validate(&secp, &params, &emulator).is_err());
        let (_, _, _, mut manifest) = fixture();
        manifest.iron_ore_asset = manifest.stone_asset.clone();
        assert!(manifest.validate(&secp, &params, &emulator).is_err());
    }

    fn lineage_record(byte: u8) -> crate::arkade::VtxoRecord {
        crate::arkade::VtxoRecord {
            outpoint: OutPoint {
                txid: Txid::from_byte_array([byte; 32]),
                vout: 0,
            },
            script: bitcoin::ScriptBuf::new(),
            amount_sats: PROTOCOL_DUST_SATS,
            assets: Vec::new(),
            created_at: Some(1),
            expires_at: Some(10_001),
            is_preconfirmed: false,
            is_swept: false,
            spent_by: None,
            settled_by: None,
            is_unrolled: false,
            is_spent: false,
        }
    }

    fn declared_lineage(byte: u8) -> ValidatedTree {
        ValidatedTree {
            state: TreeState {
                tree_id: u32::from(byte),
                x: 1,
                y: 1,
            },
            deployment_txid: Txid::from_byte_array([byte; 32]),
        }
    }

    #[test]
    fn pending_tree_does_not_block_other_live_or_advancing_lineages() {
        let declared = [
            declared_lineage(1),
            declared_lineage(2),
            declared_lineage(3),
        ];
        let due = lineage_record(1);
        let mut pending = lineage_record(2);
        pending.is_spent = true;
        pending.spent_by = Some(Txid::from_byte_array([4; 32]));
        let mut advancing = lineage_record(3);
        advancing.is_spent = true;
        advancing.spent_by = Some(Txid::from_byte_array([5; 32]));
        let successor = lineage_record(5);
        let mut scan =
            TreeLineageScan::new(&declared, vec![advancing, pending.clone(), due.clone()]).unwrap();
        scan.advance(vec![(declared[2].state, successor.clone())])
            .unwrap();
        let mut current = scan
            .records
            .current
            .iter()
            .map(|record| record.outpoint)
            .collect::<Vec<_>>();
        current.sort_unstable();
        let mut expected = vec![due.outpoint, successor.outpoint];
        expected.sort_unstable();
        assert_eq!(current, expected);
        assert_eq!(
            scan.records.pending,
            [PendingTreeLineage::Successor {
                tree_id: 2,
                outpoint: pending.outpoint,
            }]
        );
    }

    #[test]
    fn missing_deployment_is_pending_but_duplicate_records_are_invalid() {
        let declared = [declared_lineage(1), declared_lineage(2)];
        let live = lineage_record(1);
        let scan = TreeLineageScan::new(&declared, vec![live.clone()]).unwrap();
        assert_eq!(scan.records.current[0].outpoint, live.outpoint);
        assert_eq!(
            scan.records.pending,
            [PendingTreeLineage::Deployment {
                tree_id: 2,
                outpoint: lineage_record(2).outpoint,
            }]
        );
        assert!(TreeLineageScan::new(&declared, vec![live.clone(), live]).is_err());
        assert!(TreeLineageScan::new(&declared, vec![lineage_record(3)]).is_err());
    }

    #[test]
    fn pending_lineages_do_not_hide_forks_cycles_or_changed_identity() {
        let declared = [declared_lineage(1), declared_lineage(2)];
        let mut spent = lineage_record(1);
        spent.is_spent = true;
        spent.spent_by = Some(Txid::from_byte_array([3; 32]));
        let first = lineage_record(3);
        let mut fork = first.clone();
        fork.outpoint.vout = 1;
        let mut scan = TreeLineageScan::new(&declared, vec![spent.clone()]).unwrap();
        assert!(scan
            .advance(vec![
                (declared[0].state, first.clone()),
                (declared[0].state, fork),
            ])
            .is_err());

        let mut scan = TreeLineageScan::new(&declared, vec![spent.clone()]).unwrap();
        assert!(scan
            .advance(vec![(declared[1].state, first.clone())])
            .is_err());

        let mut scan = TreeLineageScan::new(&declared, vec![spent.clone()]).unwrap();
        let mut cyclic = first;
        cyclic.is_spent = true;
        cyclic.spent_by = Some(spent.outpoint.txid);
        scan.advance(vec![(declared[0].state, cyclic)]).unwrap();
        assert!(scan.advance(vec![(declared[0].state, spent)]).is_err());
    }

    #[test]
    fn indexed_supply_validation_distinguishes_burns_from_issuance() {
        assert!(indexed_supply_is_valid(LOG_SUPPLY - 1, LOG_SUPPLY, true));
        assert!(indexed_supply_is_valid(STONE_SUPPLY, STONE_SUPPLY, true));
        assert!(!indexed_supply_is_valid(
            IRON_ORE_SUPPLY + 1,
            IRON_ORE_SUPPLY,
            true,
        ));
        assert!(indexed_supply_is_valid(XP_SUPPLY, XP_SUPPLY, false));
        assert!(!indexed_supply_is_valid(XP_SUPPLY - 1, XP_SUPPLY, false));
    }
}
