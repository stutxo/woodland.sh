//! Immutable shared-world manifest used by setup and browser players.

use crate::arkade::{EmulatorParams, ServerParams};
use crate::tree::{self, TreeContract, TreeState};
use crate::txbuild;
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use bitcoin::secp256k1::{Secp256k1, Verification};
use bitcoin::Txid;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::str::FromStr;

pub(crate) const GAME_ID: &str = "woodland.sh";
pub(crate) const PROTOCOL_VERSION: u32 = 1;
pub(crate) const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub(crate) const PROTOCOL_DUST_SATS: u64 = 330;
pub(crate) const ACTIVE_LOGS_PER_TREE: u64 = 5;
pub(crate) const LOG_RESERVE_PER_TREE: u64 = 10;
pub(crate) const XP_PER_TREE: u64 = 10;
pub(crate) const TREE_COUNT: usize = 2_100;
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
pub(crate) struct ManifestTree {
    pub state: TreeState,
    pub deployment_txid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorldManifest {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub game_id: String,
    pub network: String,
    pub arkade_service_url: String,
    pub emulator_url: String,
    pub operator_signer: String,
    pub emulator_signer: String,
    pub maintenance_signer: String,
    pub rollover_signer: String,
    pub unilateral_exit_sequence: u32,
    pub dust_sats: u64,
    pub map_width: u16,
    pub map_height: u16,
    pub tree_asset: String,
    pub log_asset: String,
    pub xp_asset: String,
    pub active_logs_per_tree: u64,
    pub log_reserve_per_tree: u64,
    pub xp_per_tree: u64,
    pub player_level_curve: String,
    pub max_player_level: u64,
    pub base_log_drop_basis_points: u64,
    pub level_log_drop_bonus_basis_points: u64,
    pub level_log_drop_xp_thresholds: [u64; 5],
    pub max_level_log_drop_basis_points: u64,
    pub respawn_min_seconds: i64,
    pub respawn_max_seconds: i64,
    pub tree_script: String,
    pub tree_chop_arkade_script: String,
    pub tree_regrow_arkade_script: String,
    pub tree_renewal_arkade_script: String,
    pub genesis_txid: String,
    pub trees: Vec<ManifestTree>,
}

#[derive(Clone, Copy)]
pub(crate) struct ValidatedTree {
    pub state: TreeState,
    pub deployment_txid: Txid,
}

pub(crate) struct ValidatedWorld {
    pub tree_asset: AssetId,
    pub log_asset: AssetId,
    pub xp_asset: AssetId,
    pub genesis_txid: Txid,
    pub trees: Vec<ValidatedTree>,
    /// Used only by the native maintenance flow.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub maintenance_signer: bitcoin::XOnlyPublicKey,
    pub rollover_signer: bitcoin::XOnlyPublicKey,
    pub contract: TreeContract,
}

impl WorldManifest {
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        params: &ServerParams,
        emulator: &EmulatorParams,
        arkade_service_url: &str,
        emulator_url: &str,
        maintenance_signer: bitcoin::XOnlyPublicKey,
        rollover_signer: bitcoin::XOnlyPublicKey,
        tree_asset: AssetId,
        log_asset: AssetId,
        xp_asset: AssetId,
        contract: &TreeContract,
        genesis_txid: Txid,
        deployments: &[(TreeState, Txid)],
    ) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            protocol_version: PROTOCOL_VERSION,
            game_id: GAME_ID.to_string(),
            network: params.network.to_string(),
            arkade_service_url: arkade_service_url.trim_end_matches('/').to_owned(),
            emulator_url: emulator_url.trim_end_matches('/').to_owned(),
            operator_signer: params.signer_pk.to_string(),
            emulator_signer: emulator.signer_pk.to_string(),
            rollover_signer: rollover_signer.to_string(),
            maintenance_signer: maintenance_signer.to_string(),
            unilateral_exit_sequence: params.unilateral_exit_delay.to_consensus_u32(),
            dust_sats: params.dust_sats,
            map_width: MAP_WIDTH,
            map_height: MAP_HEIGHT,
            tree_asset: tree_asset.to_string(),
            log_asset: log_asset.to_string(),
            xp_asset: xp_asset.to_string(),
            active_logs_per_tree: ACTIVE_LOGS_PER_TREE,
            log_reserve_per_tree: LOG_RESERVE_PER_TREE,
            xp_per_tree: XP_PER_TREE,
            player_level_curve: crate::player::PLAYER_LEVEL_CURVE.to_string(),
            max_player_level: crate::player::MAX_PLAYER_LEVEL,
            base_log_drop_basis_points: tree::BASE_LOG_DROP_BASIS_POINTS,
            level_log_drop_bonus_basis_points: tree::LEVEL_LOG_DROP_BONUS_BASIS_POINTS,
            level_log_drop_xp_thresholds: tree::LEVEL_LOG_DROP_XP_THRESHOLDS,
            max_level_log_drop_basis_points: tree::MAX_LEVEL_LOG_DROP_BASIS_POINTS,
            respawn_min_seconds: tree::RESPAWN_MIN_SECS,
            respawn_max_seconds: tree::RESPAWN_MAX_SECS,
            tree_script: contract.vtxo.script_pubkey().to_hex_string(),
            tree_chop_arkade_script: contract.chop_arkade_script.to_hex_string(),
            tree_regrow_arkade_script: contract.regrow_arkade_script.to_hex_string(),
            tree_renewal_arkade_script: contract.renewal_arkade_script.to_hex_string(),
            genesis_txid: genesis_txid.to_string(),
            trees: deployments
                .iter()
                .map(|(state, deployment_txid)| ManifestTree {
                    state: *state,
                    deployment_txid: deployment_txid.to_string(),
                })
                .collect(),
        }
    }

    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parse woodland.sh world manifest")
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
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported woodland.sh world manifest schema {}; deploy schema {MANIFEST_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        let maintenance_signer: bitcoin::XOnlyPublicKey = self
            .maintenance_signer
            .parse()
            .context("parse world maintenance signer")?;
        let rollover_signer: bitcoin::XOnlyPublicKey = self
            .rollover_signer
            .parse()
            .context("parse world rollover signer")?;
        if self.protocol_version != PROTOCOL_VERSION
            || self.game_id != GAME_ID
            || self.network != params.network.to_string()
            || self.operator_signer != params.signer_pk.to_string()
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
            maintenance_signer,
            rollover_signer,
        ]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
            != 4
        {
            return Err(anyhow!("world service signers must be distinct"));
        }
        if self.map_width != MAP_WIDTH
            || self.map_height != MAP_HEIGHT
            || self.dust_sats != PROTOCOL_DUST_SATS
            || self.active_logs_per_tree != ACTIVE_LOGS_PER_TREE
            || self.active_logs_per_tree != tree::LOGS_PER_TREE
            || self.log_reserve_per_tree != LOG_RESERVE_PER_TREE
            || self.xp_per_tree != XP_PER_TREE
            || self.player_level_curve != crate::player::PLAYER_LEVEL_CURVE
            || self.max_player_level != crate::player::MAX_PLAYER_LEVEL
            || self.base_log_drop_basis_points != tree::BASE_LOG_DROP_BASIS_POINTS
            || self.level_log_drop_bonus_basis_points != tree::LEVEL_LOG_DROP_BONUS_BASIS_POINTS
            || self.level_log_drop_xp_thresholds != tree::LEVEL_LOG_DROP_XP_THRESHOLDS
            || self.max_level_log_drop_basis_points != tree::MAX_LEVEL_LOG_DROP_BASIS_POINTS
            || self.respawn_min_seconds != tree::RESPAWN_MIN_SECS
            || self.respawn_max_seconds != tree::RESPAWN_MAX_SECS
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
        let genesis_txid =
            Txid::from_str(&self.genesis_txid).context("parse world genesis transaction ID")?;
        if tree_asset.txid != genesis_txid
            || log_asset.txid != genesis_txid
            || xp_asset.txid != genesis_txid
            || tree_asset.group_index != 0
            || log_asset.group_index != 1
            || xp_asset.group_index != 2
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
            maintenance_signer,
            rollover_signer,
            params.unilateral_exit_delay,
            params.network,
            tree_asset,
            log_asset,
            xp_asset,
            params.dust_sats,
        )?;
        if self.tree_script != contract.vtxo.script_pubkey().to_hex_string()
            || self.tree_chop_arkade_script != contract.chop_arkade_script.to_hex_string()
            || self.tree_regrow_arkade_script != contract.regrow_arkade_script.to_hex_string()
            || self.tree_renewal_arkade_script != contract.renewal_arkade_script.to_hex_string()
        {
            return Err(anyhow!("world manifest covenant script mismatch"));
        }

        Ok(ValidatedWorld {
            tree_asset,
            log_asset,
            xp_asset,
            genesis_txid,
            trees,
            maintenance_signer,
            rollover_signer,
            contract,
        })
    }
}
fn expected_asset_metadata(label: &str) -> Vec<u8> {
    let entries = [
        ("game", GAME_ID.to_string()),
        ("protocol", PROTOCOL_VERSION.to_string()),
        ("asset", label.to_string()),
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

impl ValidatedWorld {
    pub async fn verify_indexed_assets(&self, rest: &crate::arkade::ArkadeRest) -> Result<()> {
        for (asset_id, label, maximum_supply, allow_zero) in [
            (self.tree_asset, "TREE", self.trees.len() as u64, false),
            (
                self.log_asset,
                "LOG",
                LOG_RESERVE_PER_TREE * self.trees.len() as u64,
                true,
            ),
            (
                self.xp_asset,
                "XP",
                XP_PER_TREE * self.trees.len() as u64,
                true,
            ),
        ] {
            let details = rest
                .get_asset_details(asset_id)
                .await
                .with_context(|| format!("verify indexed {label} asset"))?;
            if details.control_asset.is_some()
                || (!allow_zero && details.supply == 0)
                || details.supply > maximum_supply
                || details.metadata != expected_asset_metadata(label)
            {
                return Err(anyhow!(
                    "indexed {label} asset does not match the fixed-supply world genesis"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Keypair, SecretKey};
    use bitcoin::{Address, Network, PublicKey, Sequence};

    fn fixture() -> (
        Secp256k1<bitcoin::secp256k1::All>,
        ServerParams,
        EmulatorParams,
        WorldManifest,
    ) {
        let secp = Secp256k1::new();
        let operator_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let emulator_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let renewal_secret = SecretKey::from_slice(&[5; 32]).unwrap();
        let rollover_secret = SecretKey::from_slice(&[7; 32]).unwrap();
        let operator_keypair = Keypair::from_secret_key(&secp, &operator_secret);
        let operator = operator_keypair.x_only_public_key().0;
        let emulator = Keypair::from_secret_key(&secp, &emulator_secret)
            .x_only_public_key()
            .0;
        let renewal = Keypair::from_secret_key(&secp, &renewal_secret)
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
        let contract = tree::build_tree_contract(
            &secp,
            operator,
            emulator,
            renewal,
            rollover,
            params.unilateral_exit_delay,
            params.network,
            tree_asset,
            log_asset,
            xp_asset,
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
            "http://127.0.0.1:7070",
            "http://127.0.0.1:7073",
            renewal,
            rollover,
            tree_asset,
            log_asset,
            xp_asset,
            &contract,
            genesis_txid,
            &deployments,
        );
        (secp, params, emulator_params, manifest)
    }

    #[test]
    fn manifest_round_trips_exactly_declared_trees() {
        let (secp, params, emulator, manifest) = fixture();
        let json = manifest.to_json().unwrap();
        assert!(json.contains("\"xpAsset\""));
        assert!(json.contains("\"xpPerTree\""));
        let parsed = WorldManifest::from_json(&json).unwrap();
        let world = parsed.validate(&secp, &params, &emulator).unwrap();
        assert_eq!(world.trees.len(), TREE_COUNT);
        assert_eq!(world.trees[0].state, tree_states()[0]);
        assert_eq!(world.xp_asset.to_string(), parsed.xp_asset);
        assert_eq!(parsed.arkade_service_url, "http://127.0.0.1:7070");
        assert_eq!(parsed.emulator_url, "http://127.0.0.1:7073");
        assert_eq!(parsed.game_id, GAME_ID);
        assert_eq!(parsed.protocol_version, PROTOCOL_VERSION);
        assert_eq!(parsed.player_level_curve, crate::player::PLAYER_LEVEL_CURVE);
        assert_eq!(parsed.max_player_level, crate::player::MAX_PLAYER_LEVEL);
        assert_eq!(
            parsed.level_log_drop_xp_thresholds,
            tree::LEVEL_LOG_DROP_XP_THRESHOLDS
        );
        assert_eq!(
            parsed.max_level_log_drop_basis_points,
            tree::MAX_LEVEL_LOG_DROP_BASIS_POINTS
        );
        assert_eq!(parsed.respawn_min_seconds, tree::RESPAWN_MIN_SECS);
        assert_eq!(parsed.respawn_max_seconds, tree::RESPAWN_MAX_SECS);
        assert_eq!(
            world.contract.regrow_arkade_script.to_hex_string(),
            parsed.tree_regrow_arkade_script
        );
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

        let mut regrow = manifest.clone();
        regrow.tree_regrow_arkade_script.push_str("00");
        assert!(regrow.validate(&secp, &params, &emulator).is_err());

        let mut chop = manifest;
        chop.tree_chop_arkade_script.push_str("00");
        assert!(chop.validate(&secp, &params, &emulator).is_err());
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
            |manifest: &mut WorldManifest| manifest.max_player_level += 1,
            |manifest: &mut WorldManifest| manifest.log_reserve_per_tree += 1,
            |manifest: &mut WorldManifest| manifest.xp_per_tree += 1,
            |manifest: &mut WorldManifest| manifest.respawn_min_seconds -= 1,
            |manifest: &mut WorldManifest| manifest.respawn_max_seconds += 1,
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
    }
}
