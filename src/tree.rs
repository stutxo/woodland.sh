//! Recursive tree covenant primitives.
//!
//! Tree health is a numeric packet separate from the tree's fixed LOG reserve.
//! Successful chops reduce both health and the remaining LOG/XP inventory;
//! regrowth resets health without issuing assets.

use crate::protocol::{
    CHOP_ANCHOR_OUTPUT_INDEX, CHOP_ASSET_GROUP_COUNT, CHOP_EXTENSION_OUTPUT_INDEX,
    CHOP_INPUT_COUNT, CHOP_OUTPUT_COUNT, LOG_ASSET_GROUP_INDEX, PLAYER_IDENTITY_PACKET_TYPE,
    PLAYER_ID_ASSET_GROUP_INDEX, PLAYER_POSITION_PACKET_TYPE, PLAYER_STATE_INPUT_INDEX,
    PLAYER_STATE_OUTPUT_INDEX, REGROW_ANCHOR_OUTPUT_INDEX, REGROW_EXTENSION_OUTPUT_INDEX,
    REGROW_INPUT_COUNT, REGROW_OUTPUT_COUNT, REGROW_TREE_INPUT_INDEX, REGROW_TREE_OUTPUT_INDEX,
    RENEWAL_EXTENSION_OUTPUT_INDEX, RENEWAL_INPUT_COUNT, RENEWAL_OUTPUT_COUNT,
    RENEWAL_STATE_INPUT_INDEX, RENEWAL_STATE_OUTPUT_INDEX, TREE_ASSET_GROUP_INDEX,
    TREE_HEALTH_PACKET_TYPE, TREE_INPUT_INDEX, TREE_OUTPUT_INDEX, TREE_ROLL_PACKET_TYPE,
    TREE_STATE_PACKET_TYPE, XP_ASSET_GROUP_INDEX,
};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use ark_script::{op, ArkadeLeaf, ArkadeTapscript, ArkadeVtxoInput, ArkadeVtxoScript};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::opcodes::all::{
    OP_2DROP, OP_ADD, OP_CAT, OP_DROP, OP_DUP, OP_ELSE, OP_ENDIF, OP_EQUAL, OP_EQUALVERIFY,
    OP_FROMALTSTACK, OP_GREATERTHAN, OP_IF, OP_LESSTHAN, OP_MOD, OP_NIP, OP_OVER, OP_ROT,
    OP_SHA256, OP_SIZE, OP_TOALTSTACK, OP_VERIFY,
};
use bitcoin::script::witness_version::WitnessVersion;
use bitcoin::script::{Builder, PushBytesBuf};
use bitcoin::secp256k1::{Secp256k1, Verification};
use bitcoin::{Network, Psbt, ScriptBuf, Sequence, Transaction, XOnlyPublicKey};

pub const LOGS_PER_TREE: u64 = 5;
pub const CHOP_ROLL_BASIS_POINTS: u64 = 10_000;
pub const BASE_LOG_DROP_BASIS_POINTS: u64 = 1_000;
pub const LEVEL_LOG_DROP_BONUS_BASIS_POINTS: u64 = 100;
pub const LEVEL_LOG_DROP_XP_THRESHOLDS: [u64; 5] = [1_154, 4_470, 13_363, 37_224, 101_333];
pub const MAX_LEVEL_LOG_DROP_BASIS_POINTS: u64 = 1_500;
pub const RESPAWN_MIN_SECS: i64 = 20;
pub const RESPAWN_MAX_SECS: i64 = 40;

pub const fn log_drop_basis_points(player_xp: u64) -> u64 {
    let mut basis_points = BASE_LOG_DROP_BASIS_POINTS;
    let mut index = 0;
    while index < LEVEL_LOG_DROP_XP_THRESHOLDS.len() {
        if player_xp >= LEVEL_LOG_DROP_XP_THRESHOLDS[index] {
            basis_points += LEVEL_LOG_DROP_BONUS_BASIS_POINTS;
        }
        index += 1;
    }
    if basis_points > MAX_LEVEL_LOG_DROP_BASIS_POINTS {
        MAX_LEVEL_LOG_DROP_BASIS_POINTS
    } else {
        basis_points
    }
}

/// Every health state keeps the full tree value; LOG has no sats collateral.
pub fn full_tree_value_sats(dust_sats: u64) -> Result<u64> {
    let value_units = LOGS_PER_TREE
        .checked_add(1)
        .ok_or_else(|| anyhow!("tree value unit overflow"))?;
    dust_sats
        .checked_mul(value_units)
        .ok_or_else(|| anyhow!("tree value overflow"))
}

const TREE_STATE_MAGIC: &[u8; 2] = b"TR";
const TREE_STATE_VERSION: u8 = 1;
const TREE_STATE_LEN: usize = 11;
/// Immutable identity committed to every transaction in one tree's chain.
/// Mutable health, LOG reserve, and XP inventory are committed separately.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeState {
    pub tree_id: u32,
    pub x: u16,
    pub y: u16,
}

impl TreeState {
    pub fn encode(self) -> [u8; TREE_STATE_LEN] {
        let mut encoded = [0_u8; TREE_STATE_LEN];
        encoded[..2].copy_from_slice(TREE_STATE_MAGIC);
        encoded[2] = TREE_STATE_VERSION;
        encoded[3..7].copy_from_slice(&self.tree_id.to_le_bytes());
        encoded[7..9].copy_from_slice(&self.x.to_le_bytes());
        encoded[9..11].copy_from_slice(&self.y.to_le_bytes());
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() != TREE_STATE_LEN
            || &encoded[..2] != TREE_STATE_MAGIC
            || encoded[2] != TREE_STATE_VERSION
        {
            return Err(anyhow!("invalid tree state packet"));
        }
        Ok(Self {
            tree_id: u32::from_le_bytes(encoded[3..7].try_into().expect("fixed tree state")),
            x: u16::from_le_bytes(encoded[7..9].try_into().expect("fixed tree state")),
            y: u16::from_le_bytes(encoded[9..11].try_into().expect("fixed tree state")),
        })
    }
}

/// Publicly predictable but covenant-enforced entropy for one tree. Every
/// swing replaces this value with its SHA-256 digest; the digest's value modulo
/// 10,000 is compared with the player's XP-derived LOG-drop threshold.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct TreeRoll([u8; 32]);

impl TreeRoll {
    pub fn initial(state: TreeState) -> Self {
        let mut material = b"woodland.sh/tree-roll/v1".to_vec();
        material.extend_from_slice(&state.encode());
        Self(sha256::Hash::hash(&material).to_byte_array())
    }

    pub const fn encode(self) -> [u8; 32] {
        self.0
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        let bytes: [u8; 32] = encoded
            .try_into()
            .map_err(|_| anyhow!("invalid tree roll packet"))?;
        Ok(Self(bytes))
    }

    pub fn next(self) -> Self {
        Self(sha256::Hash::hash(&self.0).to_byte_array())
    }

    pub fn bucket(self) -> u64 {
        self.0.iter().rev().fold(0_u64, |value, byte| {
            (value * 256 + u64::from(*byte)) % CHOP_ROLL_BASIS_POINTS
        })
    }

    pub fn advance(self, player_xp: u64) -> (Self, bool) {
        let next = self.next();
        (next, next.bucket() < log_drop_basis_points(player_xp))
    }
}
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct TreeHealth(u64);

impl TreeHealth {
    pub fn new(value: u64) -> Result<Self> {
        if value > LOGS_PER_TREE {
            return Err(anyhow!("tree health exceeds {LOGS_PER_TREE}"));
        }
        Ok(Self(value))
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub fn encode(self) -> [u8; 9] {
        let mut encoded = [0_u8; 9];
        encoded[..8].copy_from_slice(&self.0.to_le_bytes());
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() != 9 || encoded[8] != 0 {
            return Err(anyhow!("invalid tree health packet"));
        }
        Self::new(u64::from_le_bytes(
            encoded[..8].try_into().expect("fixed tree health"),
        ))
    }
}

fn push_health_packet_value(builder: Builder, input_index: Option<usize>) -> Builder {
    let builder = builder.push_int(TREE_HEALTH_PACKET_TYPE.into());
    let builder = match input_index {
        Some(index) => builder
            .push_int(index as i64)
            .push_opcode(op::INSPECTINPUTPACKET),
        None => builder.push_opcode(op::INSPECTPACKET),
    };
    builder
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SIZE)
        .push_int(9)
        .push_opcode(OP_EQUALVERIFY)
        // Require the unique positive fixed-width encoding. Without this,
        // negative zero could numerically pass a stump transition and brick the
        // shared tree for canonical clients.
        .push_opcode(OP_DUP)
        .push_opcode(op::BIN2NUM)
        .push_opcode(OP_DUP)
        .push_int(9)
        .push_opcode(op::NUM2BIN)
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int(-1)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY)
}

fn push_tree_health_amounts(builder: Builder, input_index: usize) -> Builder {
    let builder = push_health_packet_value(builder, Some(input_index));
    push_health_packet_value(builder, None)
}

pub fn respawn_delay_secs(state: TreeState, outpoint: bitcoin::OutPoint) -> i64 {
    let mut material = b"woodland.sh/respawn-delay/v1".to_vec();
    material.extend_from_slice(&state.encode());
    material.extend_from_slice(&outpoint.txid.to_byte_array());
    material.extend_from_slice(&outpoint.vout.to_le_bytes());
    let digest = sha256::Hash::hash(&material).to_byte_array();
    let spread = (RESPAWN_MAX_SECS - RESPAWN_MIN_SECS + 1) as u64;
    let sample = u64::from_le_bytes(digest[..8].try_into().expect("fixed respawn digest"));
    RESPAWN_MIN_SECS + (sample % spread) as i64
}

pub fn respawn_at(
    state: TreeState,
    outpoint: bitcoin::OutPoint,
    created_at: Option<i64>,
) -> Result<i64> {
    created_at
        .ok_or_else(|| anyhow!("stump has no server creation time"))?
        .checked_add(respawn_delay_secs(state, outpoint))
        .ok_or_else(|| anyhow!("tree respawn deadline overflow"))
}

/// Host-side mirror of the asset invariants enforced by the Arkade Script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChopTransition {
    pub previous_state: TreeState,
    pub next_state: TreeState,
    pub previous_roll: TreeRoll,
    pub next_roll: TreeRoll,
    pub previous_health: TreeHealth,
    pub next_health: TreeHealth,
    pub player_xp_before: u64,
    pub state_xp_before: u64,
    pub state_xp_after: u64,
    pub success: bool,
    pub dust_sats: u64,
    pub tree_markers_before: u64,
    pub tree_markers_after: u64,
    pub tree_logs_before: u64,
    pub tree_logs_after: u64,
    pub tree_xp_balance_before: u64,
    pub tree_xp_balance_after: u64,
    pub player_logs_before: u64,
    pub player_logs_after: u64,
    pub player_xp_balance_before: u64,
    pub player_xp_balance_after: u64,
    pub player_value_before: u64,
    pub player_value_after: u64,
    pub tree_value_before: u64,
    pub tree_value_after: u64,
}

impl ChopTransition {
    pub fn validate(self) -> Result<()> {
        if self.next_state != self.previous_state {
            return Err(anyhow!("tree identity or position changed"));
        }
        if self.tree_markers_before != 1 || self.tree_markers_after != 1 {
            return Err(anyhow!("tree transition must preserve one TREE marker"));
        }
        if self.previous_health.value() == 0
            || self.tree_logs_before == 0
            || self.tree_xp_balance_before == 0
        {
            return Err(anyhow!("cannot chop a depleted tree"));
        }
        let (expected_roll, expected_success) = self.previous_roll.advance(self.player_xp_before);
        if self.next_roll != expected_roll || self.success != expected_success {
            return Err(anyhow!("tree chop roll is invalid"));
        }
        let reward = u64::from(self.success);
        if self.state_xp_before != self.player_xp_before
            || self.state_xp_before != self.player_xp_balance_before
            || self.state_xp_after != self.player_xp_balance_after
            || self.state_xp_after
                != self
                    .state_xp_before
                    .checked_add(reward)
                    .ok_or_else(|| anyhow!("player XP overflow"))?
        {
            return Err(anyhow!(
                "player XP counter does not match the conserved XP asset"
            ));
        }
        if self.next_health.value()
            != self
                .previous_health
                .value()
                .checked_sub(reward)
                .ok_or_else(|| anyhow!("tree health underflow"))?
        {
            return Err(anyhow!("tree health does not match the chop roll"));
        }
        if self.tree_logs_after
            != self
                .tree_logs_before
                .checked_sub(reward)
                .ok_or_else(|| anyhow!("tree LOG balance underflow"))?
            || self.player_logs_after
                != self
                    .player_logs_before
                    .checked_add(reward)
                    .ok_or_else(|| anyhow!("player LOG balance overflow"))?
        {
            return Err(anyhow!("LOG balances do not match the chop roll"));
        }
        if self.tree_xp_balance_after
            != self
                .tree_xp_balance_before
                .checked_sub(reward)
                .ok_or_else(|| anyhow!("tree XP balance underflow"))?
            || self.player_xp_balance_after
                != self
                    .player_xp_balance_before
                    .checked_add(reward)
                    .ok_or_else(|| anyhow!("player XP balance overflow"))?
        {
            return Err(anyhow!("XP asset balances do not match the chop roll"));
        }
        if self.dust_sats == 0
            || self.player_value_before != self.dust_sats
            || self.player_value_after != self.dust_sats
        {
            return Err(anyhow!("player state must preserve one dust value"));
        }
        let expected_tree_value = full_tree_value_sats(self.dust_sats)?;
        if self.tree_value_before != expected_tree_value
            || self.tree_value_after != expected_tree_value
        {
            return Err(anyhow!("tree value must remain fixed during chop"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegrowTransition {
    pub previous_state: TreeState,
    pub next_state: TreeState,
    pub previous_roll: TreeRoll,
    pub next_roll: TreeRoll,
    pub previous_health: TreeHealth,
    pub next_health: TreeHealth,
    pub dust_sats: u64,
    pub tree_markers_before: u64,
    pub tree_markers_after: u64,
    pub tree_logs_before: u64,
    pub tree_logs_after: u64,
    pub tree_xp_balance_before: u64,
    pub tree_xp_balance_after: u64,
    pub tree_value_before: u64,
    pub tree_value_after: u64,
}

impl RegrowTransition {
    pub fn validate(self) -> Result<()> {
        if self.previous_state != self.next_state {
            return Err(anyhow!("regrowth changed the tree identity or position"));
        }
        if self.previous_roll != self.next_roll {
            return Err(anyhow!("regrowth changed the tree chop roll"));
        }
        if self.previous_health.value() != 0 || self.next_health.value() != LOGS_PER_TREE {
            return Err(anyhow!(
                "regrowth must reset zero health to {LOGS_PER_TREE}"
            ));
        }
        if self.tree_markers_before != 1 || self.tree_markers_after != 1 {
            return Err(anyhow!("regrowth must preserve one TREE marker"));
        }
        if self.tree_logs_before != self.tree_logs_after
            || self.tree_xp_balance_before != self.tree_xp_balance_after
            || self.tree_logs_before < LOGS_PER_TREE
            || self.tree_xp_balance_before < LOGS_PER_TREE
        {
            return Err(anyhow!(
                "regrowth must preserve at least {LOGS_PER_TREE} LOG and XP"
            ));
        }
        let expected_value = full_tree_value_sats(self.dust_sats)?;
        if self.tree_value_before != expected_value || self.tree_value_after != expected_value {
            return Err(anyhow!("regrowth must preserve the fixed tree value"));
        }
        Ok(())
    }
}

/// The complete contract material needed to fund and later spend a tree VTXO.
#[derive(Clone, Debug)]
pub struct TreeContract {
    pub vtxo: ark_core::Vtxo,
    pub chop_spend_script: ScriptBuf,
    pub chop_arkade_script: ScriptBuf,
    pub regrow_spend_script: ScriptBuf,
    pub regrow_arkade_script: ScriptBuf,
    pub renewal_spend_script: ScriptBuf,
    pub renewal_arkade_script: ScriptBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeServiceKeys {
    pub operator: XOnlyPublicKey,
    pub emulator: XOnlyPublicKey,
    pub maintenance: XOnlyPublicKey,
}

/// Build the shared three-leaf tree contract: swing, maintained regrowth, and
/// exact-self-send batch renewal.
///
/// `maintenance_pk` gates timed regrowth. `rollover_pk` gates renewal so a
/// permissionless caller cannot continuously rotate the outpoint and starve
/// gameplay; public services may accept rollover requests from anyone.
///
/// The three usable tapleaves have separate authority:
///
/// - chop: operator + covenant-tweaked emulator;
/// - regrowth: maintenance + operator + covenant-tweaked emulator;
/// - renewal: rollover + operator + covenant-tweaked emulator.
///
/// The CSV exit is keyed to the NUMS point and is intentionally unusable as an
/// escape from the recursive covenant.
#[allow(clippy::too_many_arguments)]
pub fn build_tree_contract<C: Verification>(
    secp: &Secp256k1<C>,
    operator_pk: XOnlyPublicKey,
    emulator_pk: XOnlyPublicKey,
    maintenance_pk: XOnlyPublicKey,
    rollover_pk: XOnlyPublicKey,
    exit_delay: Sequence,
    network: Network,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    dust_sats: u64,
) -> Result<TreeContract> {
    if [tree_asset, log_asset, xp_asset]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("TREE, LOG, and XP asset IDs must differ"));
    }
    if [operator_pk, emulator_pk, maintenance_pk, rollover_pk]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 4
    {
        return Err(anyhow!("tree contract signers must be distinct"));
    }
    script_int(full_tree_value_sats(dust_sats)?, "full tree value")?;

    let chop_arkade_script = tree_covenant_script(tree_asset, log_asset, xp_asset, dust_sats)?;
    let regrow_arkade_script =
        tree_regrow_covenant_script(tree_asset, log_asset, xp_asset, dust_sats)?;
    let renewal_arkade_script = tree_renewal_covenant_script(tree_asset, log_asset, xp_asset)?;
    let leaf = |arkade_script: ScriptBuf, pubkeys: Vec<XOnlyPublicKey>| {
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script,
            tapscript: ArkadeTapscript::Multisig { pubkeys },
            introspectors: vec![emulator_pk],
        })
    };
    let processed = ArkadeVtxoScript::new(vec![
        leaf(chop_arkade_script.clone(), vec![operator_pk]),
        leaf(
            regrow_arkade_script.clone(),
            vec![operator_pk, maintenance_pk],
        ),
        leaf(
            renewal_arkade_script.clone(),
            vec![operator_pk, rollover_pk],
        ),
    ])
    .context("build tree Arkade tapleaves")?;
    let [chop_spend_script, regrow_spend_script, renewal_spend_script] =
        processed.scripts.as_slice()
    else {
        return Err(anyhow!(
            "tree contract must have chop, regrow, and renewal leaves"
        ));
    };
    let chop_spend_script = chop_spend_script.clone();
    let regrow_spend_script = regrow_spend_script.clone();
    let renewal_spend_script = renewal_spend_script.clone();
    let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY
        .parse()
        .context("parse Arkade NUMS key")?;
    let owner = nums.inner.x_only_public_key().0;
    // arkd requires a timelocked exit leaf on every batch VTXO. Shared trees
    // key it to the NUMS owner: it satisfies the server's exit-delay
    // accounting without giving anyone a unilateral path around the covenant.
    let scripts = processed
        .scripts
        .into_iter()
        .chain([ark_core::script::csv_sig_script(exit_delay, owner)])
        .collect();
    let vtxo = ark_core::Vtxo::new_with_custom_scripts(
        secp,
        operator_pk,
        owner,
        scripts,
        exit_delay,
        network,
    )
    .map_err(|error| anyhow!("build tree VTXO: {error}"))?;

    Ok(TreeContract {
        vtxo,
        chop_spend_script,
        chop_arkade_script,
        regrow_spend_script,
        regrow_arkade_script,
        renewal_spend_script,
        renewal_arkade_script,
    })
}

/// Build the tree half of the atomic PLAYER/TREE chop.
///
/// The tree transfers LOG and XP into owner-authorized player state and requires
/// the numeric XP packet to equal the player's conserved XP asset balance.
///
/// Canonical shape:
///
/// ```text
/// vin 0 player | vin 1 tree
/// vout 0 player | vout 1 tree | vout 2 extension | vout 3 anchor
/// groups 0..3 PLAYER_ID | TREE | LOG | XP
/// ```
///
/// This half owns the global game rule: tree identity, roll advancement,
/// success probability, health, reward movement, and fixed world-asset supply.
/// The personalized player half supplies the owner signature and pins this
/// exact tree contract.
pub fn tree_covenant_script(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    dust_sats: u64,
) -> Result<ScriptBuf> {
    if [tree_asset, log_asset, xp_asset]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("TREE, LOG, and XP asset IDs must differ"));
    }
    if dust_sats == 0 {
        return Err(anyhow!("tree dust must be non-zero"));
    }
    let player_value = script_int(dust_sats, "player value")?;
    let tree_value = script_int(full_tree_value_sats(dust_sats)?, "tree value")?;
    let anchor_program =
        witness_v1_program(&ark_core::anchor_output().script_pubkey, "Arkade anchor")?;
    let builder = Builder::new()
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(TREE_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(CHOP_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(CHOP_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(CHOP_ASSET_GROUP_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(TREE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_int(3)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(CHOP_EXTENSION_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(CHOP_ANCHOR_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        // The player input must execute an Arkade covenant, while its own
        // personalized leaf pins this exact shared tree.
        .push_int(PLAYER_STATE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINPUTARKADESCRIPTHASH)
        .push_opcode(OP_DROP);

    let mut builder =
        push_player_marker_group(builder, PLAYER_STATE_INPUT_INDEX, PLAYER_STATE_OUTPUT_INDEX);
    for (asset, group_index) in [
        (tree_asset, TREE_ASSET_GROUP_INDEX as i64),
        (log_asset, LOG_ASSET_GROUP_INDEX as i64),
        (xp_asset, XP_ASSET_GROUP_INDEX as i64),
    ] {
        builder = push_asset_group_index(builder, asset)
            .push_int(group_index)
            .push_opcode(OP_EQUALVERIFY);
    }
    for asset in [tree_asset, log_asset, xp_asset] {
        builder = push_transfer_group_shell(builder, asset);
    }

    let builder = push_equal_input_output_scripts(
        builder,
        PLAYER_STATE_INPUT_INDEX,
        PLAYER_STATE_OUTPUT_INDEX,
    );
    let builder = push_equal_input_output_scripts(builder, TREE_INPUT_INDEX, TREE_OUTPUT_INDEX)
        .push_int(i64::from(PLAYER_STATE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(player_value)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(PLAYER_STATE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINPUTVALUE)
        .push_int(player_value)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(TREE_OUTPUT_INDEX.into())
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(tree_value)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(TREE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINPUTVALUE)
        .push_int(tree_value)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(TREE_STATE_PACKET_TYPE.into())
        .push_int(TREE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(TREE_STATE_PACKET_TYPE.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY);

    let builder = crate::player::push_preserved_player_packet(
        builder,
        PLAYER_IDENTITY_PACKET_TYPE,
        PLAYER_STATE_INPUT_INDEX,
    );
    let builder = crate::player::push_preserved_player_packet(
        builder,
        PLAYER_POSITION_PACKET_TYPE,
        PLAYER_STATE_INPUT_INDEX,
    );

    // XP advances exactly when the deterministic tree roll succeeds.
    let builder = crate::player::push_player_xp_amounts(builder, PLAYER_STATE_INPUT_INDEX);
    let builder = push_advanced_tree_roll(builder, TREE_INPUT_INDEX, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_ROT)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_tree_health_amounts(builder, TREE_INPUT_INDEX)
        .push_opcode(OP_OVER)
        .push_int(0)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    let builder = push_advanced_tree_roll(builder, TREE_INPUT_INDEX, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_extension_and_anchor_shape(
        builder,
        CHOP_EXTENSION_OUTPUT_INDEX,
        CHOP_ANCHOR_OUTPUT_INDEX,
        &anchor_program,
    );

    let builder = push_canonical_player_asset_counts(builder, log_asset, xp_asset);

    // The TREE marker remains unique.
    let builder = push_input_asset_lookup(builder, TREE_INPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_output_asset_lookup(builder, TREE_OUTPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);

    // LOG moves from tree to player on a successful roll.
    let builder = push_input_asset_lookup(builder, TREE_INPUT_INDEX, log_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int(0)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    let builder = push_optional_output_asset_lookup(builder, TREE_OUTPUT_INDEX, log_asset)
        .push_opcode(OP_DROP);
    let builder = push_advanced_tree_roll(builder, TREE_INPUT_INDEX, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, log_asset)
        .push_opcode(OP_DROP);
    let builder = push_optional_output_asset_lookup(builder, PLAYER_STATE_OUTPUT_INDEX, log_asset)
        .push_opcode(OP_DROP);
    let builder = push_advanced_tree_roll(builder, TREE_INPUT_INDEX, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_ROT)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);

    // XP is conserved and moved into player state, where it backs the XP packet.
    let builder = push_input_asset_lookup(builder, TREE_INPUT_INDEX, xp_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int(0)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    let builder = push_optional_output_asset_lookup(builder, TREE_OUTPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    let builder = push_advanced_tree_roll(builder, TREE_INPUT_INDEX, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    let builder = crate::player::push_player_input_xp(builder, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_output_asset_lookup(builder, PLAYER_STATE_OUTPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    let builder = crate::player::push_player_output_xp(builder).push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    let builder = push_optional_output_asset_lookup(builder, PLAYER_STATE_OUTPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    Ok(
        push_advanced_tree_roll(builder, TREE_INPUT_INDEX, PLAYER_STATE_INPUT_INDEX)
            .push_opcode(OP_ROT)
            .push_opcode(OP_ADD)
            .push_opcode(OP_EQUAL)
            .into_script(),
    )
}

/// Maintenance-authorized respawn for one canonical stump. The maintenance
/// host checks its wall-clock deadline before signing.
///
/// A stump keeps one TREE marker, its remaining fixed-supply LOG and XP,
/// and the tree's fixed sats. This covenant preserves every asset amount and
/// resets only the health packet from zero to five.
///
/// Arkade Script has no wall-clock opcode here. The covenant proves only the
/// zero-to-five reset and complete state/asset preservation; the maintenance
/// signer waits for the host-computed deadline before authorizing it.
pub fn tree_regrow_covenant_script(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    dust_sats: u64,
) -> Result<ScriptBuf> {
    if [tree_asset, log_asset, xp_asset]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("TREE, LOG, and XP asset IDs must differ"));
    }
    let tree_value = script_int(full_tree_value_sats(dust_sats)?, "tree value")?;
    let anchor_program =
        witness_v1_program(&ark_core::anchor_output().script_pubkey, "Arkade anchor")?;
    let builder = Builder::new()
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(REGROW_TREE_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(REGROW_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(REGROW_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(3)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(REGROW_TREE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_int(3)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(REGROW_TREE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_int(3)
        .push_opcode(OP_EQUALVERIFY);
    let builder =
        push_equal_input_output_scripts(builder, REGROW_TREE_INPUT_INDEX, REGROW_TREE_OUTPUT_INDEX)
            .push_int(i64::from(REGROW_TREE_OUTPUT_INDEX))
            .push_opcode(op::INSPECTOUTPUTVALUE)
            .push_int(tree_value)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(REGROW_TREE_INPUT_INDEX as i64)
            .push_opcode(op::INSPECTINPUTVALUE)
            .push_int(tree_value)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(TREE_STATE_PACKET_TYPE.into())
            .push_int(REGROW_TREE_INPUT_INDEX as i64)
            .push_opcode(op::INSPECTINPUTPACKET)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(TREE_STATE_PACKET_TYPE.into())
            .push_opcode(op::INSPECTPACKET)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_EQUALVERIFY);
    let builder = push_preserved_tree_roll(builder, REGROW_TREE_INPUT_INDEX);
    let builder = push_tree_health_amounts(builder, REGROW_TREE_INPUT_INDEX)
        .push_int(LOGS_PER_TREE as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_extension_and_anchor_shape(
        builder,
        REGROW_EXTENSION_OUTPUT_INDEX,
        REGROW_ANCHOR_OUTPUT_INDEX,
        &anchor_program,
    )
    .push_int(i64::from(REGROW_EXTENSION_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(REGROW_ANCHOR_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY);

    let builder = push_canonical_asset_group(builder, tree_asset, 1, 1);
    let builder = push_canonical_asset_group(builder, log_asset, 1, 1);
    let builder = push_canonical_asset_group(builder, xp_asset, 1, 1);
    let builder = push_local_asset_group_inputs(builder, tree_asset, 1);
    let builder = push_local_asset_group_inputs(builder, log_asset, 1);
    let builder = push_local_asset_group_inputs(builder, xp_asset, 1);

    let builder = push_input_asset_lookup(builder, REGROW_TREE_INPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_output_asset_lookup(builder, REGROW_TREE_OUTPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);

    let builder = push_input_asset_lookup(builder, REGROW_TREE_INPUT_INDEX, log_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int((LOGS_PER_TREE - 1) as i64)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    let builder = push_output_asset_lookup(builder, REGROW_TREE_OUTPUT_INDEX, log_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY);

    let builder = push_input_asset_lookup(builder, REGROW_TREE_INPUT_INDEX, xp_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int((LOGS_PER_TREE - 1) as i64)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    Ok(
        push_output_asset_lookup(builder, REGROW_TREE_OUTPUT_INDEX, xp_asset)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_EQUAL)
            .into_script(),
    )
}

/// Covenant for the tree's batch-renewal leaf. It runs on a version-2 intent
/// proof and only permits an exact self-send: identical P2TR, value, TREE/LOG/XP
/// assets, and tree-state packets. Renewal changes nothing; it only re-enters the
/// VTXO into a fresh batch for a new expiry.
///
/// Canonical intent proof shape:
///
/// - input 0: fake BIP322 message input bound to the register message
/// - input 1: this tree VTXO spending the renewal leaf
/// - output 0: the same tree P2TR with the same value
/// - output 1: zero-value merged ARK extension (asset, state, emulator)
pub fn tree_renewal_covenant_script(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
) -> Result<ScriptBuf> {
    if [tree_asset, log_asset, xp_asset]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("TREE, LOG, and XP asset IDs must differ"));
    }
    let builder = push_renewal_shape(Builder::new())?;
    let builder = push_equal_input_output_scripts(
        builder,
        RENEWAL_STATE_INPUT_INDEX,
        RENEWAL_STATE_OUTPUT_INDEX,
    )
    .push_int(i64::from(RENEWAL_STATE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(RENEWAL_STATE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_opcode(OP_EQUALVERIFY);
    let builder = push_equal_state_packet(builder, TREE_STATE_PACKET_TYPE);
    let builder = push_equal_state_packet(builder, TREE_ROLL_PACKET_TYPE);
    let builder = push_equal_state_packet(builder, TREE_HEALTH_PACKET_TYPE);
    let builder = push_renewal_asset_shell(builder)?;
    let builder = push_transfer_group_shell(builder, tree_asset);
    let builder = push_optional_transfer_group_shell(builder, log_asset);
    let builder = push_optional_transfer_group_shell(builder, xp_asset);

    let builder = push_input_asset_lookup(builder, RENEWAL_STATE_INPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_output_asset_lookup(builder, RENEWAL_STATE_OUTPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);

    let builder = push_optional_input_asset_lookup(builder, RENEWAL_STATE_INPUT_INDEX, log_asset);
    let builder = push_optional_output_asset_lookup(builder, RENEWAL_STATE_OUTPUT_INDEX, log_asset);
    let builder = builder
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY);

    let builder = push_optional_input_asset_lookup(builder, RENEWAL_STATE_INPUT_INDEX, xp_asset);
    let builder = push_optional_output_asset_lookup(builder, RENEWAL_STATE_OUTPUT_INDEX, xp_asset);
    Ok(builder
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUAL)
        .into_script())
}

/// Shared proof shape for every woodland.sh renewal leaf: the spending transaction
/// must be a version-2 intent proof whose only real input is the renewed VTXO
/// and whose outputs are the state continuation plus the merged extension.
pub(crate) fn push_renewal_shape(builder: Builder) -> Result<Builder> {
    Ok(builder
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(RENEWAL_STATE_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTVERSION)
        .push_int(2)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(RENEWAL_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(RENEWAL_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY))
}

/// Require the renewed state packet to be carried byte-for-byte from the
/// previous transaction into this proof's merged extension.
pub(crate) fn push_equal_state_packet(builder: Builder, packet_type: u8) -> Builder {
    builder
        .push_int(packet_type.into())
        .push_int(RENEWAL_STATE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(packet_type.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
}

/// Verify SHA256(input roll) is the output roll and leave the deterministic
/// XP-adjusted success bit on the stack. XP is the numeric PLAYER packet.
pub(crate) fn push_advanced_tree_roll(
    builder: Builder,
    tree_input_index: usize,
    player_input_index: usize,
) -> Builder {
    let positive_sign = PushBytesBuf::try_from(vec![0]).expect("one-byte sign extension");
    let builder = builder
        .push_int(TREE_ROLL_PACKET_TYPE.into())
        .push_int(tree_input_index as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SIZE)
        .push_int(32)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SHA256)
        .push_opcode(OP_DUP)
        .push_int(TREE_ROLL_PACKET_TYPE.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
        // Arkade BigNums are little-endian signed integers. Appending zero
        // keeps every digest positive before taking the probability bucket.
        .push_slice(positive_sign)
        .push_opcode(OP_CAT)
        .push_opcode(op::BIN2NUM)
        .push_int(CHOP_ROLL_BASIS_POINTS as i64)
        .push_opcode(OP_MOD);
    // Add one so the strict comparison below is exactly `XP >= threshold`.
    let builder = crate::player::push_player_input_xp(builder, player_input_index)
        .push_int(1)
        .push_opcode(OP_ADD)
        .push_int(BASE_LOG_DROP_BASIS_POINTS as i64);
    let mut builder = builder;
    for threshold in LEVEL_LOG_DROP_XP_THRESHOLDS {
        builder = builder
            .push_opcode(OP_OVER)
            .push_int(threshold as i64)
            .push_opcode(OP_GREATERTHAN)
            .push_opcode(OP_IF)
            .push_int(LEVEL_LOG_DROP_BONUS_BASIS_POINTS as i64)
            .push_opcode(OP_ADD)
            .push_opcode(OP_ENDIF);
    }
    builder
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_DROP)
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_LESSTHAN)
}

fn push_preserved_tree_roll(builder: Builder, tree_input_index: usize) -> Builder {
    builder
        .push_int(TREE_ROLL_PACKET_TYPE.into())
        .push_int(tree_input_index as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SIZE)
        .push_int(32)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(TREE_ROLL_PACKET_TYPE.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
}

/// Pin the extension shape and prove assets cannot leak anywhere except the
/// state output: every packet asset group is assigned to the sole state input
/// and the sole state output, and the extension carries no assets.
pub(crate) fn push_renewal_asset_shell(builder: Builder) -> Result<Builder> {
    Ok(builder
        .push_int(i64::from(RENEWAL_EXTENSION_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(RENEWAL_EXTENSION_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(-1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DROP)
        .push_int(i64::from(RENEWAL_EXTENSION_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(RENEWAL_STATE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(i64::from(RENEWAL_STATE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_opcode(OP_EQUALVERIFY))
}

pub(crate) fn push_equal_input_output_scripts(
    builder: Builder,
    input_index: usize,
    output_index: u16,
) -> Builder {
    builder
        .push_int(i64::from(output_index))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(input_index as i64)
        .push_opcode(op::INSPECTINPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
}

fn push_extension_and_anchor_shape(
    builder: Builder,
    extension_output_index: u16,
    anchor_output_index: u16,
    anchor_program: &PushBytesBuf,
) -> Builder {
    builder
        // Packet inspection proves the sole non-witness output is this merged
        // extension output.
        .push_int(i64::from(extension_output_index))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(extension_output_index))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(-1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DROP)
        .push_int(i64::from(anchor_output_index))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(anchor_output_index))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_slice(anchor_program.clone())
        .push_opcode(OP_EQUALVERIFY)
}

/// Preserve the packet-local group-zero PLAYER_ID without knowing its
/// post-issuance AssetId. The group must be one uncontrolled, metadata-free
/// unit moving from the player state input to the player state output.
pub(crate) fn push_player_marker_group(
    builder: Builder,
    input_index: usize,
    output_index: u16,
) -> Builder {
    builder
        // Group zero has exactly one assignment on each side.
        .push_int(PLAYER_ID_ASSET_GROUP_INDEX as i64)
        .push_opcode(OP_DUP)
        .push_int(2)
        .push_opcode(op::INSPECTASSETGROUPNUM)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        // Transfers cannot add issuance authority or metadata.
        .push_opcode(OP_DUP)
        .push_opcode(op::INSPECTASSETGROUPCTRL)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_2DROP)
        .push_opcode(OP_DUP)
        .push_opcode(op::INSPECTASSETGROUPMETADATAHASH)
        .push_slice(PushBytesBuf::try_from(vec![0; 32]).expect("fixed-size metadata hash"))
        .push_opcode(OP_EQUALVERIFY)
        // Input assignment: local type, selected player-state vin, one unit.
        .push_opcode(OP_DUP)
        .push_int(0)
        .push_int(0)
        .push_opcode(op::INSPECTASSETGROUP)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(input_index as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        // Output assignment: local type, selected player-state vout, one unit.
        .push_int(0)
        .push_int(1)
        .push_opcode(op::INSPECTASSETGROUP)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(output_index))
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
}

/// Require player state to contain its mandatory PLAYER_ID plus at most one LOG
/// and one XP holding. Both reciprocal chop covenants use this exact sequence so
/// their view of canonical player inventory cannot drift.
pub(crate) fn push_canonical_player_asset_counts(
    builder: Builder,
    log_asset: AssetId,
    xp_asset: AssetId,
) -> Builder {
    let builder = push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, log_asset)
        .push_opcode(OP_NIP);
    let builder = push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, xp_asset)
        .push_opcode(OP_NIP)
        .push_opcode(OP_ADD)
        .push_int(1)
        .push_opcode(OP_ADD)
        .push_int(PLAYER_STATE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_output_asset_lookup(builder, PLAYER_STATE_OUTPUT_INDEX, log_asset)
        .push_opcode(OP_NIP);
    push_optional_output_asset_lookup(builder, PLAYER_STATE_OUTPUT_INDEX, xp_asset)
        .push_opcode(OP_NIP)
        .push_opcode(OP_ADD)
        .push_int(1)
        .push_opcode(OP_ADD)
        .push_int(i64::from(PLAYER_STATE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_opcode(OP_EQUALVERIFY)
}

/// Prove that group zero is the PLAYER_ID and every remaining renewal group
/// is one of the two optional player inventory assets.
pub(crate) fn push_player_renewal_group_set(
    builder: Builder,
    log_asset: AssetId,
    xp_asset: AssetId,
) -> Builder {
    let builder = push_optional_transfer_group_shell_counted(builder.push_int(1), log_asset)
        .push_opcode(OP_ADD);
    push_optional_transfer_group_shell_counted(builder, xp_asset)
        .push_opcode(OP_ADD)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_opcode(OP_EQUALVERIFY)
}

fn push_optional_transfer_group_shell_counted(builder: Builder, asset: AssetId) -> Builder {
    builder
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::FINDASSETGROUPBYASSETID)
        .push_opcode(OP_IF)
        .push_opcode(OP_DUP)
        .push_opcode(op::INSPECTASSETGROUPCTRL)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_2DROP)
        .push_opcode(op::INSPECTASSETGROUPMETADATAHASH)
        .push_slice(PushBytesBuf::try_from(vec![0; 32]).expect("fixed-size metadata hash"))
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DROP)
        .push_int(0)
        .push_opcode(OP_ENDIF)
}

pub(crate) fn push_transfer_group_shell(builder: Builder, asset: AssetId) -> Builder {
    push_asset_group_index(builder, asset)
        .push_opcode(OP_DUP)
        .push_opcode(op::INSPECTASSETGROUPCTRL)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_2DROP)
        .push_opcode(op::INSPECTASSETGROUPMETADATAHASH)
        .push_slice(PushBytesBuf::try_from(vec![0; 32]).expect("fixed-size metadata hash"))
        .push_opcode(OP_EQUALVERIFY)
}

pub(crate) fn push_optional_transfer_group_shell(builder: Builder, asset: AssetId) -> Builder {
    builder
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::FINDASSETGROUPBYASSETID)
        .push_opcode(OP_IF)
        .push_opcode(OP_DUP)
        .push_opcode(op::INSPECTASSETGROUPCTRL)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_2DROP)
        .push_opcode(op::INSPECTASSETGROUPMETADATAHASH)
        .push_slice(PushBytesBuf::try_from(vec![0; 32]).expect("fixed-size metadata hash"))
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DROP)
        .push_opcode(OP_ENDIF)
}

fn push_canonical_asset_group(
    builder: Builder,
    asset: AssetId,
    expected_inputs: usize,
    expected_outputs: usize,
) -> Builder {
    push_asset_group_index(builder, asset)
        .push_opcode(OP_DUP)
        .push_int(2)
        .push_opcode(op::INSPECTASSETGROUPNUM)
        .push_int(expected_outputs as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(expected_inputs as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_opcode(op::INSPECTASSETGROUPCTRL)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_2DROP)
        .push_opcode(op::INSPECTASSETGROUPMETADATAHASH)
        .push_slice(PushBytesBuf::try_from(vec![0; 32]).expect("fixed-size metadata hash"))
        .push_opcode(OP_EQUALVERIFY)
}

fn push_local_asset_group_inputs(
    mut builder: Builder,
    asset: AssetId,
    input_count: usize,
) -> Builder {
    for item_index in 0..input_count {
        builder = push_asset_group_index(builder, asset)
            .push_int(item_index as i64)
            .push_int(0)
            .push_opcode(op::INSPECTASSETGROUP)
            // A local input pushes (type, vin, amount). An intent input has an
            // extra txid, so this final comparison cannot consume type=1.
            .push_opcode(OP_2DROP)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY);
    }
    builder
}

fn witness_v1_program(script: &ScriptBuf, name: &str) -> Result<PushBytesBuf> {
    if script.witness_version() != Some(WitnessVersion::V1) {
        return Err(anyhow!("{name} must be a witness-v1 program"));
    }
    PushBytesBuf::try_from(script.as_bytes()[2..].to_vec())
        .map_err(|error| anyhow!("invalid {name} witness program: {error}"))
}

fn script_int(value: u64, name: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| anyhow!("{name} does not fit an Arkade script integer"))
}

pub(crate) fn push_input_asset_lookup(
    builder: Builder,
    input_index: usize,
    asset: AssetId,
) -> Builder {
    push_asset_group_index(builder, asset)
        .push_opcode(OP_DROP)
        .push_int(input_index as i64)
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::INSPECTINASSETLOOKUP)
}

pub(crate) fn push_output_asset_lookup(
    builder: Builder,
    output_index: u16,
    asset: AssetId,
) -> Builder {
    push_asset_group_index(builder, asset)
        .push_opcode(OP_DROP)
        .push_int(i64::from(output_index))
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::INSPECTOUTASSETLOOKUP)
}

/// Asset lookup that preserves the opcode's `(0, 0)` result when the group is
/// absent. Mandatory marker lookups continue to use `push_asset_group_index`.
pub(crate) fn push_optional_input_asset_lookup(
    builder: Builder,
    input_index: usize,
    asset: AssetId,
) -> Builder {
    builder
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::FINDASSETGROUPBYASSETID)
        // Consume the found flag and discard the packet-local group position.
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP)
        .push_int(input_index as i64)
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::INSPECTINASSETLOOKUP)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DROP)
        .push_int(0)
        .push_int(0)
        .push_opcode(OP_ENDIF)
}

pub(crate) fn push_optional_output_asset_lookup(
    builder: Builder,
    output_index: u16,
    asset: AssetId,
) -> Builder {
    builder
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::FINDASSETGROUPBYASSETID)
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP)
        .push_int(i64::from(output_index))
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::INSPECTOUTASSETLOOKUP)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DROP)
        .push_int(0)
        .push_int(0)
        .push_opcode(OP_ENDIF)
}

fn push_asset_group_index(builder: Builder, asset: AssetId) -> Builder {
    builder
        .push_slice(asset_txid_bytes(asset))
        .push_int(i64::from(asset.group_index))
        .push_opcode(op::FINDASSETGROUPBYASSETID)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
}

fn asset_txid_bytes(asset: AssetId) -> PushBytesBuf {
    // Asset lookup opcodes consume the canonical issuance txid and group index;
    // FIND separately proves the asset exists in the current packet.
    PushBytesBuf::try_from(asset.txid.to_byte_array().to_vec())
        .expect("asset txid is a fixed-size push")
}

pub fn attach_tree_state_packet(psbt: &mut Psbt, state: TreeState) -> Result<()> {
    ark_core::extension::add_packet_to_psbt(psbt, TREE_STATE_PACKET_TYPE, &state.encode())
        .context("attach tree state packet")
}

pub fn attach_tree_health_packet(psbt: &mut Psbt, health: TreeHealth) -> Result<()> {
    ark_core::extension::add_packet_to_psbt(psbt, TREE_HEALTH_PACKET_TYPE, &health.encode())
        .context("attach tree health packet")
}

pub fn tree_health_from_tx(tx: &Transaction) -> Result<Option<TreeHealth>> {
    ark_core::extension::find_packet_payload(tx, TREE_HEALTH_PACKET_TYPE)
        .context("read tree health packet")?
        .map(TreeHealth::decode)
        .transpose()
}

pub fn tree_state_from_tx(tx: &Transaction) -> Result<Option<TreeState>> {
    ark_core::extension::find_packet_payload(tx, TREE_STATE_PACKET_TYPE)
        .context("read tree state packet")?
        .map(TreeState::decode)
        .transpose()
}

pub fn attach_tree_roll_packet(psbt: &mut Psbt, roll: TreeRoll) -> Result<()> {
    ark_core::extension::add_packet_to_psbt(psbt, TREE_ROLL_PACKET_TYPE, &roll.encode())
        .context("attach tree roll packet")
}

pub fn tree_roll_from_tx(tx: &Transaction) -> Result<Option<TreeRoll>> {
    ark_core::extension::find_packet_payload(tx, TREE_ROLL_PACKET_TYPE)
        .context("read tree roll packet")?
        .map(TreeRoll::decode)
        .transpose()
}

/// Attach the stump's creating transaction, preserved identity/roll, reset
/// health packet, and the regrowth leaf's emulator entry.
pub fn attach_tree_regrow_context(
    psbt: &mut Psbt,
    checkpoints: &[Psbt],
    contract: &TreeContract,
    previous_tx: &Transaction,
) -> Result<(TreeState, TreeRoll, TreeHealth)> {
    if psbt.unsigned_tx.input.len() != REGROW_INPUT_COUNT {
        return Err(anyhow!(
            "tree regrowth requires exactly {REGROW_INPUT_COUNT} input"
        ));
    }
    let mut updated = psbt.clone();
    crate::txbuild::attach_previous_ark_transactions(
        &mut updated,
        checkpoints,
        std::iter::once(previous_tx),
    )?;
    let state = tree_state_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous stump transaction has no tree state packet"))?;
    let roll = tree_roll_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous stump transaction has no tree roll packet"))?;
    let health = tree_health_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous stump transaction has no tree health packet"))?;
    if health.value() != 0 {
        return Err(anyhow!("tree regrowth input is not a stump"));
    }
    attach_tree_state_packet(&mut updated, state)?;
    attach_tree_roll_packet(&mut updated, roll)?;
    attach_tree_health_packet(&mut updated, TreeHealth::new(LOGS_PER_TREE)?)?;
    let packet = ark_core::introspector::packet::Packet::new(vec![
        ark_core::introspector::packet::IntrospectorEntry {
            vin: REGROW_TREE_INPUT_INDEX as u16,
            script: contract.regrow_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
    ])
    .context("build tree regrowth emulator packet")?;
    ark_core::introspector::packet::add_packet_to_psbt(&mut updated, &packet)
        .context("attach tree regrowth emulator packet")?;
    *psbt = updated;
    Ok((state, roll, health))
}

/// Verify the exact maintenance/operator/emulator signature set on a timed
/// regrowth transaction and its checkpoint.
pub fn verify_regrow_response(
    keys: &crate::Keys,
    service_keys: TreeServiceKeys,
    contract: &TreeContract,
    expected_ark: &Psbt,
    expected_checkpoints: &[Psbt],
    returned_ark: &Psbt,
    returned_checkpoints: Vec<Psbt>,
) -> Result<()> {
    if expected_ark.unsigned_tx != returned_ark.unsigned_tx
        || expected_ark.inputs.len() != REGROW_INPUT_COUNT
        || returned_ark.inputs.len() != REGROW_INPUT_COUNT
    {
        return Err(anyhow!(
            "emulator changed the submitted tree regrowth transaction"
        ));
    }
    let tweaked_emulator = ark_script::compute_arkade_script_public_key(
        &service_keys.emulator,
        &contract.regrow_arkade_script,
    )
    .context("derive tree regrowth emulator signer")?;
    let signers = [
        service_keys.operator,
        service_keys.maintenance,
        tweaked_emulator,
    ];
    let names = ["operator", "maintenance", "emulator"];
    verify_exact_chop_signatures(
        keys,
        expected_ark,
        returned_ark,
        REGROW_TREE_INPUT_INDEX,
        &signers,
        &names,
        "regrowth Ark input",
    )?;

    if expected_checkpoints.len() != REGROW_INPUT_COUNT
        || returned_checkpoints.len() != REGROW_INPUT_COUNT
    {
        return Err(anyhow!(
            "emulator returned an invalid regrowth checkpoint count"
        ));
    }
    let expected = &expected_checkpoints[0];
    let returned = &returned_checkpoints[0];
    if expected.unsigned_tx != returned.unsigned_tx
        || expected.inputs.len() != returned.inputs.len()
    {
        return Err(anyhow!("emulator changed the regrowth checkpoint"));
    }
    verify_exact_chop_signatures(
        keys,
        expected,
        returned,
        0,
        &signers,
        &names,
        "regrowth checkpoint input",
    )
}

fn verify_exact_chop_signatures(
    keys: &crate::Keys,
    expected: &Psbt,
    returned: &Psbt,
    input_index: usize,
    signers: &[XOnlyPublicKey],
    signer_names: &[&str],
    label: &str,
) -> Result<()> {
    if signers.is_empty() || signers.len() != signer_names.len() {
        return Err(anyhow!("invalid required signer set for {label}"));
    }
    let expected_input = expected
        .inputs
        .get(input_index)
        .ok_or_else(|| anyhow!("expected PSBT is missing {label} {input_index}"))?;
    let matching_scripts = expected_input
        .tap_scripts
        .values()
        .filter(|(script, _)| {
            let closure_signers = ark_core::script::extract_checksig_pubkeys(script);
            closure_signers.len() == signers.len()
                && signers
                    .iter()
                    .all(|signer| closure_signers.contains(signer))
        })
        .collect::<Vec<_>>();
    if matching_scripts.len() != 1 {
        return Err(anyhow!(
            "expected {label} {input_index} must have exactly one spend script with the required signer set"
        ));
    }

    let returned_input = returned
        .inputs
        .get(input_index)
        .ok_or_else(|| anyhow!("returned PSBT is missing {label} {input_index}"))?;
    if !returned_input.partial_sigs.is_empty()
        || returned_input.tap_key_sig.is_some()
        || returned_input.final_script_sig.is_some()
        || returned_input.final_script_witness.is_some()
    {
        return Err(anyhow!(
            "returned {label} {input_index} contains signatures outside the required script path"
        ));
    }
    if returned_input.tap_script_sigs.len() != signers.len() {
        return Err(anyhow!(
            "returned {label} {input_index} has {} signatures, expected {}",
            returned_input.tap_script_sigs.len(),
            signers.len(),
        ));
    }
    for (signer, name) in signers.iter().zip(signer_names) {
        crate::txbuild::verified_signature_for_key(
            keys,
            expected,
            returned,
            input_index,
            *signer,
            name,
        )?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::hex::DisplayHex;
    use bitcoin::secp256k1::{Keypair, SecretKey};
    use bitcoin::Txid;

    fn asset(byte: u8, group_index: u16) -> AssetId {
        AssetId {
            txid: Txid::from_byte_array([byte; 32]),
            group_index,
        }
    }

    fn xonly(secp: &Secp256k1<bitcoin::secp256k1::All>, byte: u8) -> XOnlyPublicKey {
        Keypair::from_secret_key(secp, &SecretKey::from_slice(&[byte; 32]).unwrap())
            .x_only_public_key()
            .0
    }

    fn state() -> TreeState {
        TreeState {
            tree_id: 417,
            x: 7,
            y: 13,
        }
    }

    fn roll_for(player_xp: u64, expected_success: bool) -> (TreeRoll, TreeRoll) {
        let mut previous = TreeRoll::initial(state());
        loop {
            let (next, success) = previous.advance(player_xp);
            if success == expected_success {
                return (previous, next);
            }
            previous = next;
        }
    }

    #[test]
    fn tree_wire_and_hash_vectors_are_stable() {
        let state = state();
        assert_eq!(
            state.encode().to_lower_hex_string(),
            "545201a101000007000d00"
        );
        let initial = TreeRoll::initial(state);
        assert_eq!(
            initial.encode().to_lower_hex_string(),
            "f316acb9f17b68cf6ca7c5dfe372d660bf1a62b0f5f0a79f6ec6059e52d8690e"
        );
        let next = initial.next();
        assert_eq!(
            next.encode().to_lower_hex_string(),
            "0f2cbd50926dacf90260117470f3d3fc313c34ff5f9cd7498002cf19dea42ba9"
        );
        assert_eq!(next.bucket(), 5_263);
        assert_eq!(
            respawn_delay_secs(
                state,
                bitcoin::OutPoint {
                    txid: Txid::from_byte_array([1; 32]),
                    vout: 2,
                },
            ),
            24
        );
        let mut negative_zero = [0_u8; 9];
        negative_zero[8] = 0x80;
        assert!(TreeHealth::decode(&negative_zero).is_err());
    }
    #[test]
    fn successful_chop_moves_xp_and_log_to_player() {
        let (previous_roll, next_roll) = roll_for(0, true);
        let valid = ChopTransition {
            previous_state: state(),
            next_state: state(),
            previous_roll,
            next_roll,
            previous_health: TreeHealth::new(5).unwrap(),
            next_health: TreeHealth::new(4).unwrap(),
            player_xp_before: 0,
            state_xp_before: 0,
            state_xp_after: 1,
            success: true,
            dust_sats: 330,
            tree_markers_before: 1,
            tree_markers_after: 1,
            tree_logs_before: 5,
            tree_logs_after: 4,
            tree_xp_balance_before: 5,
            tree_xp_balance_after: 4,
            player_logs_before: 9,
            player_logs_after: 10,
            player_xp_balance_before: 0,
            player_xp_balance_after: 1,
            player_value_before: 330,
            player_value_after: 330,
            tree_value_before: 1_980,
            tree_value_after: 1_980,
        };
        valid.validate().unwrap();

        let mut unburned_xp = valid;
        unburned_xp.tree_xp_balance_after = 5;
        assert!(unburned_xp.validate().is_err());
        let mut overburned_xp = valid;
        overburned_xp.tree_xp_balance_after = 3;
        assert!(overburned_xp.validate().is_err());
        let mut missing_xp = valid;
        missing_xp.state_xp_after = 0;
        assert!(missing_xp.validate().is_err());
        let mut extra_xp = valid;
        extra_xp.state_xp_after = 2;
        assert!(extra_xp.validate().is_err());
    }

    #[test]
    fn missed_chop_advances_roll_without_burning_inventory() {
        let (previous_roll, next_roll) = roll_for(0, false);
        let valid = ChopTransition {
            previous_state: state(),
            next_state: state(),
            previous_roll,
            next_roll,
            previous_health: TreeHealth::new(5).unwrap(),
            next_health: TreeHealth::new(5).unwrap(),
            player_xp_before: 0,
            state_xp_before: 0,
            state_xp_after: 0,
            success: false,
            dust_sats: 330,
            tree_markers_before: 1,
            tree_markers_after: 1,
            tree_logs_before: 5,
            tree_logs_after: 5,
            tree_xp_balance_before: 5,
            tree_xp_balance_after: 5,
            player_logs_before: 9,
            player_logs_after: 9,
            player_xp_balance_before: 0,
            player_xp_balance_after: 0,
            player_value_before: 330,
            player_value_after: 330,
            tree_value_before: 1_980,
            tree_value_after: 1_980,
        };
        valid.validate().unwrap();
        let mut forged_xp = valid;
        forged_xp.state_xp_after = 1;
        assert!(forged_xp.validate().is_err());
        let mut burned_xp = valid;
        burned_xp.tree_xp_balance_after = 4;
        assert!(burned_xp.validate().is_err());
    }

    #[test]
    fn level_bonus_thresholds_and_cap_are_exact() {
        assert_eq!(
            LEVEL_LOG_DROP_XP_THRESHOLDS,
            [10, 20, 30, 40, 50].map(|level| crate::player::xp_for_level(level).unwrap())
        );
        for (xp, basis_points) in [
            (0, 1_000),
            (1_153, 1_000),
            (1_154, 1_100),
            (4_469, 1_100),
            (4_470, 1_200),
            (13_363, 1_300),
            (37_224, 1_400),
            (101_332, 1_400),
            (101_333, 1_500),
            (u64::MAX, 1_500),
        ] {
            assert_eq!(log_drop_basis_points(xp), basis_points, "XP {xp}");
        }

        let mut previous = TreeRoll::initial(state());
        loop {
            let (level_nine_roll, level_nine_success) = previous.advance(1_153);
            let (level_ten_roll, level_ten_success) = previous.advance(1_154);
            assert_eq!(level_nine_roll, level_ten_roll);
            if !level_nine_success && level_ten_success {
                break;
            }
            previous = level_nine_roll;
        }
    }

    #[cfg(feature = "regtest-e2e")]
    #[test]
    fn respawn_delays_are_bounded_and_staggered() {
        let delays = crate::world::TREE_STATES
            .into_iter()
            .enumerate()
            .map(|(index, state)| {
                respawn_delay_secs(
                    state,
                    bitcoin::OutPoint {
                        txid: Txid::from_byte_array([(index + 1) as u8; 32]),
                        vout: TREE_OUTPUT_INDEX.into(),
                    },
                )
            })
            .collect::<std::collections::HashSet<_>>();
        assert!(delays.len() > 1);
        assert!(delays
            .iter()
            .all(|delay| (RESPAWN_MIN_SECS..=RESPAWN_MAX_SECS).contains(delay)));
    }

    #[test]
    fn regrowth_resets_health_without_issuing_assets() {
        let roll = TreeRoll::initial(state());
        let valid = RegrowTransition {
            previous_state: state(),
            next_state: state(),
            previous_roll: roll,
            next_roll: roll,
            previous_health: TreeHealth::new(0).unwrap(),
            next_health: TreeHealth::new(5).unwrap(),
            dust_sats: 330,
            tree_markers_before: 1,
            tree_markers_after: 1,
            tree_logs_before: 5,
            tree_logs_after: 5,
            tree_xp_balance_before: 5,
            tree_xp_balance_after: 5,
            tree_value_before: 1_980,
            tree_value_after: 1_980,
        };
        valid.validate().unwrap();
        let mut issued = valid;
        issued.tree_logs_after = 6;
        assert!(issued.validate().is_err());
        let mut low_reserve = valid;
        low_reserve.tree_xp_balance_before = 4;
        assert!(low_reserve.validate().is_err());
    }

    #[test]
    fn many_permissionless_players_preserve_world_supply_across_many_schedules() {
        #[derive(Clone, Copy)]
        struct SimTree {
            state: TreeState,
            roll: TreeRoll,
            health: TreeHealth,
            logs: u64,
            xp_balance: u64,
            regrown: bool,
        }

        for seed in 1_u64..=32 {
            let mut players_xp_counters = [0_u64; 256];
            let mut players_logs = [0_u64; 256];
            let mut players_xp_balances = [0_u64; 256];
            let mut trees = (0_u32..10)
                .map(|index| {
                    let state = TreeState {
                        tree_id: 417 + index,
                        x: index as u16,
                        y: (index * 2) as u16,
                    };
                    SimTree {
                        state,
                        roll: TreeRoll::initial(state),
                        health: TreeHealth::new(5).unwrap(),
                        logs: 10,
                        xp_balance: 10,
                        regrown: false,
                    }
                })
                .collect::<Vec<_>>();
            let mut entropy = seed;
            let mut attempts = 0_u64;

            loop {
                for tree in &mut trees {
                    if tree.health.value() == 0 && tree.logs >= LOGS_PER_TREE && !tree.regrown {
                        RegrowTransition {
                            previous_state: tree.state,
                            next_state: tree.state,
                            previous_roll: tree.roll,
                            next_roll: tree.roll,
                            previous_health: tree.health,
                            next_health: TreeHealth::new(LOGS_PER_TREE).unwrap(),
                            dust_sats: 330,
                            tree_markers_before: 1,
                            tree_markers_after: 1,
                            tree_logs_before: tree.logs,
                            tree_logs_after: tree.logs,
                            tree_xp_balance_before: tree.xp_balance,
                            tree_xp_balance_after: tree.xp_balance,
                            tree_value_before: 1_980,
                            tree_value_after: 1_980,
                        }
                        .validate()
                        .unwrap();
                        tree.health = TreeHealth::new(LOGS_PER_TREE).unwrap();
                        tree.regrown = true;
                    }
                }

                let active_count = trees.iter().filter(|tree| tree.health.value() > 0).count();
                if active_count == 0 {
                    break;
                }
                entropy = entropy
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let selected = (entropy as usize) % active_count;
                let tree_index = trees
                    .iter()
                    .enumerate()
                    .filter(|(_, tree)| tree.health.value() > 0)
                    .nth(selected)
                    .map(|(index, _)| index)
                    .unwrap();
                entropy = entropy
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let player_index = (entropy as usize) % players_xp_counters.len();

                let tree = &mut trees[tree_index];
                let previous_roll = tree.roll;
                let previous_health = tree.health;
                let previous_logs = tree.logs;
                let previous_xp_balance = tree.xp_balance;
                let previous_xp = players_xp_counters[player_index];
                let previous_player_logs = players_logs[player_index];
                let previous_player_xp_balance = players_xp_balances[player_index];
                let (next_roll, success) = previous_roll.advance(previous_xp);
                let reward = u64::from(success);
                let next_health =
                    TreeHealth::new(previous_health.value().checked_sub(reward).unwrap()).unwrap();
                let next_logs = previous_logs.checked_sub(reward).unwrap();
                let next_xp_balance = previous_xp_balance.checked_sub(reward).unwrap();
                let next_xp = previous_xp.checked_add(reward).unwrap();
                let next_player_logs = previous_player_logs.checked_add(reward).unwrap();
                let next_player_xp_balance =
                    previous_player_xp_balance.checked_add(reward).unwrap();

                ChopTransition {
                    previous_state: tree.state,
                    next_state: tree.state,
                    previous_roll,
                    next_roll,
                    previous_health,
                    next_health,
                    player_xp_before: previous_xp,
                    state_xp_before: previous_xp,
                    state_xp_after: next_xp,
                    success,
                    dust_sats: 330,
                    tree_markers_before: 1,
                    tree_markers_after: 1,
                    tree_logs_before: previous_logs,
                    tree_logs_after: next_logs,
                    tree_xp_balance_before: previous_xp_balance,
                    tree_xp_balance_after: next_xp_balance,
                    player_logs_before: previous_player_logs,
                    player_logs_after: next_player_logs,
                    player_xp_balance_before: previous_player_xp_balance,
                    player_xp_balance_after: next_player_xp_balance,
                    player_value_before: 330,
                    player_value_after: 330,
                    tree_value_before: 1_980,
                    tree_value_after: 1_980,
                }
                .validate()
                .unwrap();

                let identity = crate::player::PlayerIdentity {
                    player_id: [player_index as u8; 32],
                };
                let position = crate::player::PlayerPosition {
                    x: player_index as u16,
                    y: 17,
                };
                crate::player::PlayerChopTransition {
                    previous_state: crate::player::PlayerState {
                        identity,
                        position,
                        xp: crate::player::PlayerXp::new(previous_xp),
                    },
                    next_state: crate::player::PlayerState {
                        identity,
                        position,
                        xp: crate::player::PlayerXp::new(next_xp),
                    },
                    previous_tree_state: tree.state,
                    next_tree_state: tree.state,
                    previous_tree_roll: previous_roll,
                    next_tree_roll: next_roll,
                    previous_tree_health: previous_health,
                    next_tree_health: next_health,
                    success,
                    state_logs_before: previous_player_logs,
                    state_logs_after: next_player_logs,
                    state_xp_balance_before: previous_player_xp_balance,
                    state_xp_balance_after: next_player_xp_balance,
                    tree_markers_before: 1,
                    tree_markers_after: 1,
                    tree_logs_before: previous_logs,
                    tree_logs_after: next_logs,
                    tree_xp_balance_before: previous_xp_balance,
                    tree_xp_balance_after: next_xp_balance,
                    state_value_before: 330,
                    state_value_after: 330,
                    tree_value_before: 1_980,
                    tree_value_after: 1_980,
                    dust_sats: 330,
                }
                .validate()
                .unwrap();

                tree.roll = next_roll;
                tree.health = next_health;
                tree.logs = next_logs;
                tree.xp_balance = next_xp_balance;
                players_xp_counters[player_index] = next_xp;
                players_logs[player_index] = next_player_logs;
                players_xp_balances[player_index] = next_player_xp_balance;
                attempts += 1;
                assert!(attempts < 20_000, "schedule {seed} did not terminate");
            }

            assert!(trees.iter().all(|tree| {
                tree.regrown && tree.health.value() == 0 && tree.logs == 0 && tree.xp_balance == 0
            }));
            assert!(players_xp_counters
                .iter()
                .zip(players_logs.iter())
                .zip(players_xp_balances.iter())
                .all(|((xp, logs), xp_balance)| *xp == *logs && *xp == *xp_balance));
            assert_eq!(
                trees.iter().map(|tree| tree.logs).sum::<u64>() + players_logs.iter().sum::<u64>(),
                100
            );
            assert_eq!(
                trees.iter().map(|tree| tree.xp_balance).sum::<u64>()
                    + players_xp_balances.iter().sum::<u64>(),
                100
            );
        }
    }

    #[test]
    fn scripts_commit_xp_and_health_shapes() {
        let tree = asset(1, 0);
        let log = asset(1, 1);
        let xp_balance = asset(1, 2);
        let chop = tree_covenant_script(tree, log, xp_balance, 330).unwrap();
        let regrow = tree_regrow_covenant_script(tree, log, xp_balance, 330).unwrap();
        let renewal = tree_renewal_covenant_script(tree, log, xp_balance).unwrap();
        for script in [&chop, &regrow, &renewal] {
            assert!(script.len() <= 10_000);
        }
        let regrow_asm = ark_script::to_asm(&regrow).unwrap();
        assert!(regrow_asm.contains("OP_INSPECTNUMINPUTS OP_PUSHNUM_1 OP_EQUALVERIFY"));
        assert!(regrow_asm.contains("OP_INSPECTNUMASSETGROUPS"));
        let chop_asm = ark_script::to_asm(&chop).unwrap();
        assert!(chop_asm.contains("OP_SHA256"));
        assert!(chop_asm.contains("OP_BIN2NUM"));
        assert!(chop_asm.contains("OP_NUM2BIN"));
        assert!(chop_asm.contains("OP_MOD"));
        assert!(chop_asm.contains("OP_SUB") || chop_asm.contains("OP_ADD"));

        let secp = Secp256k1::new();
        let operator = xonly(&secp, 3);
        let emulator = xonly(&secp, 4);
        let maintenance = xonly(&secp, 5);
        let rollover = xonly(&secp, 6);
        let contract = build_tree_contract(
            &secp,
            operator,
            emulator,
            maintenance,
            rollover,
            Sequence::from_height(144),
            Network::Regtest,
            tree,
            log,
            xp_balance,
            330,
        )
        .unwrap();
        let regrow_emulator =
            ark_script::compute_arkade_script_public_key(&emulator, &contract.regrow_arkade_script)
                .unwrap();
        let regrow_signers =
            ark_core::script::extract_checksig_pubkeys(&contract.regrow_spend_script);
        assert_eq!(regrow_signers.len(), 3);
        assert!(regrow_signers.contains(&operator));
        assert!(regrow_signers.contains(&maintenance));
        assert!(regrow_signers.contains(&regrow_emulator));
        let renewal_emulator = ark_script::compute_arkade_script_public_key(
            &emulator,
            &contract.renewal_arkade_script,
        )
        .unwrap();
        let renewal_signers =
            ark_core::script::extract_checksig_pubkeys(&contract.renewal_spend_script);
        assert_eq!(renewal_signers.len(), 3);
        assert!(renewal_signers.contains(&operator));
        assert!(renewal_signers.contains(&rollover));
        assert!(renewal_signers.contains(&renewal_emulator));
        assert!(!renewal_signers.contains(&maintenance));
        let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY.parse().unwrap();
        let disabled_exit = ark_core::script::csv_sig_script(
            Sequence::from_height(144),
            nums.inner.x_only_public_key().0,
        );
        assert!(contract.vtxo.tapscripts().contains(&disabled_exit));
    }
}
