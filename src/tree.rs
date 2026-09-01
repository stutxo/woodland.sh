//! Recursive tree covenant primitives.
//!
//! Tree health is a numeric packet separate from the tree's fixed LOG reserve.
//! Successful chops reduce both health and the remaining LOG/XP inventory;
//! renewal resets health without issuing assets.

use crate::protocol::{
    CHOP_ANCHOR_OUTPUT_INDEX, CHOP_ASSET_GROUP_COUNT, CHOP_EXTENSION_OUTPUT_INDEX,
    CHOP_FEE_ANCHOR_OUTPUT_INDEX, CHOP_FEE_CHANGE_OUTPUT_INDEX, CHOP_FEE_EXTENSION_OUTPUT_INDEX,
    CHOP_FEE_INPUT_COUNT, CHOP_FEE_INPUT_INDEX, CHOP_FEE_OUTPUT_COUNT, CHOP_INPUT_COUNT,
    CHOP_OUTPUT_COUNT, LOG_ASSET_GROUP_INDEX, PLAYER_ID_ASSET_GROUP_INDEX,
    PLAYER_STATE_INPUT_INDEX, PLAYER_STATE_OUTPUT_INDEX, RENEWAL_EXTENSION_OUTPUT_INDEX,
    RENEWAL_FEE_CHANGE_OUTPUT_INDEX, RENEWAL_FEE_EXTENSION_OUTPUT_INDEX, RENEWAL_FEE_INPUT_COUNT,
    RENEWAL_FEE_INPUT_INDEX, RENEWAL_FEE_OUTPUT_COUNT, RENEWAL_INPUT_COUNT, RENEWAL_OUTPUT_COUNT,
    RENEWAL_STATE_INPUT_INDEX, RENEWAL_STATE_OUTPUT_INDEX, TREE_ASSET_GROUP_INDEX,
    TREE_HEALTH_PACKET_TYPE, TREE_INPUT_INDEX, TREE_OUTPUT_INDEX, TREE_STATE_PACKET_TYPE,
    XP_ASSET_GROUP_INDEX,
};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use ark_script::{op, ArkadeLeaf, ArkadeTapscript, ArkadeVtxoInput, ArkadeVtxoScript};
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::{
    OP_2DROP, OP_ADD, OP_DROP, OP_DUP, OP_ELSE, OP_ENDIF, OP_EQUAL, OP_EQUALVERIFY,
    OP_FROMALTSTACK, OP_GREATERTHAN, OP_IF, OP_NIP, OP_OVER, OP_ROT, OP_SIZE, OP_TOALTSTACK,
    OP_VERIFY,
};
use bitcoin::script::witness_version::WitnessVersion;
use bitcoin::script::{Builder, PushBytesBuf};
use bitcoin::secp256k1::{Secp256k1, Verification};
use bitcoin::{Network, Psbt, ScriptBuf, Sequence, Transaction, XOnlyPublicKey};

pub const LOGS_PER_TREE: u64 = 10;

/// A tree carries exactly one dust of backing. LOG/XP are ledger entries with
/// no sats collateral, and the covenant pins this value through every
/// transition.
pub fn tree_value_sats(dust_sats: u64) -> u64 {
    dust_sats
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeTransition {
    Deployment,
    Renewal,
    Chop,
}

impl TreeTransition {
    pub(crate) const fn output_index(self) -> u32 {
        match self {
            Self::Deployment | Self::Renewal => 0,
            Self::Chop => TREE_OUTPUT_INDEX as u32,
        }
    }
}

fn classify_transition_shape(
    input_count: usize,
    group_count: Option<usize>,
    is_deployment: bool,
) -> Result<TreeTransition> {
    if is_deployment {
        return Ok(TreeTransition::Deployment);
    }
    // The intent proof's fake input is not part of the settled renewal
    // transaction, leaving the renewed tree's parent as its sole input.
    if input_count == 1 {
        return Ok(TreeTransition::Renewal);
    }
    match (input_count, group_count) {
        (CHOP_INPUT_COUNT | CHOP_FEE_INPUT_COUNT, Some(CHOP_ASSET_GROUP_COUNT)) => {
            Ok(TreeTransition::Chop)
        }
        _ => Err(anyhow!("current tree transaction has an invalid shape")),
    }
}

pub(crate) fn classify_transition(
    transaction: &Transaction,
    is_deployment: bool,
) -> Result<TreeTransition> {
    let group_count = (!is_deployment && transaction.input.len() != 1)
        .then(|| crate::asset_packet::asset_group_count(transaction))
        .transpose()?;
    classify_transition_shape(transaction.input.len(), group_count, is_deployment)
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

fn push_numeric_tree_packet_value(
    builder: Builder,
    packet_type: u8,
    input_index: Option<usize>,
) -> Builder {
    let builder = builder.push_int(packet_type.into());
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
        // Require the unique non-negative fixed-width encoding. Without this,
        // negative zero could numerically pass a transition and brick the tree.
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

fn push_health_packet_value(builder: Builder, input_index: Option<usize>) -> Builder {
    push_numeric_tree_packet_value(builder, TREE_HEALTH_PACKET_TYPE, input_index)
}

fn push_tree_health_amounts(builder: Builder, input_index: usize) -> Builder {
    let builder = push_health_packet_value(builder, Some(input_index));
    push_health_packet_value(builder, None)
}

/// Host-side mirror of the asset invariants enforced by the Arkade Script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChopTransition {
    pub previous_state: TreeState,
    pub next_state: TreeState,
    pub previous_player_luck: crate::player::PlayerLuck,
    pub next_player_luck: crate::player::PlayerLuck,
    pub previous_health: TreeHealth,
    pub next_health: TreeHealth,
    pub player_xp_before: u64,
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
        let (expected_luck, expected_success) =
            self.previous_player_luck.advance(self.player_xp_before);
        if self.next_player_luck != expected_luck || self.success != expected_success {
            return Err(anyhow!("tree chop luck transition is invalid"));
        }
        let reward = u64::from(self.success);
        if self.player_xp_before != self.player_xp_balance_before
            || self.player_xp_balance_after
                != self
                    .player_xp_balance_before
                    .checked_add(reward)
                    .ok_or_else(|| anyhow!("player XP overflow"))?
        {
            return Err(anyhow!(
                "player XP asset balance does not match the chop reward"
            ));
        }
        if self.next_health.value()
            != self
                .previous_health
                .value()
                .checked_sub(reward)
                .ok_or_else(|| anyhow!("tree health underflow"))?
        {
            return Err(anyhow!("tree health does not match the chop reward"));
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
            return Err(anyhow!("LOG balances do not match the chop reward"));
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
            return Err(anyhow!("XP asset balances do not match the chop reward"));
        }
        if self.dust_sats == 0
            || self.player_value_before != self.dust_sats
            || self.player_value_after != self.dust_sats
        {
            return Err(anyhow!("player state must preserve one dust value"));
        }
        let expected_tree_value = tree_value_sats(self.dust_sats);
        if self.tree_value_before != expected_tree_value
            || self.tree_value_after != expected_tree_value
        {
            return Err(anyhow!("tree value must remain fixed during chop"));
        }
        Ok(())
    }
}

/// Complete material needed to fund and spend one recursive tree VTXO.
#[derive(Clone, Debug)]
pub struct TreeContract {
    pub vtxo: ark_core::Vtxo,
    pub chop_spend_script: ScriptBuf,
    pub chop_arkade_script: ScriptBuf,
    /// Permissionless funded-stump regrowth.
    pub regrowth_spend_script: ScriptBuf,
    pub regrowth_arkade_script: ScriptBuf,
    /// Exact-state lifecycle renewal guarded by the world's rollover signer.
    pub maintenance_spend_script: ScriptBuf,
    pub maintenance_arkade_script: ScriptBuf,
}

/// Build the shared tree contract: covenant-enforced swings, permissionless
/// funded-stump regrowth, and guarded exact-state lifecycle maintenance.
///
/// Regrowth has no project signer. Maintenance adds only the low-authority
/// rollover signer and cannot alter state or assets. The CSV exit is keyed to
/// the NUMS point and cannot bypass recursion.
#[allow(clippy::too_many_arguments)]
pub fn build_tree_contract<C: Verification>(
    secp: &Secp256k1<C>,
    operator_pk: XOnlyPublicKey,
    emulator_pk: XOnlyPublicKey,
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
    if [operator_pk, emulator_pk, rollover_pk]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("tree contract signers must be distinct"));
    }
    script_int(tree_value_sats(dust_sats), "tree value")?;

    let chop_arkade_script = tree_covenant_script(tree_asset, log_asset, xp_asset, dust_sats)?;
    let regrowth_arkade_script = tree_regrowth_covenant_script(tree_asset, log_asset, xp_asset)?;
    let maintenance_arkade_script =
        tree_maintenance_covenant_script(tree_asset, log_asset, xp_asset)?;
    let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY
        .parse()
        .context("parse Arkade NUMS key")?;
    let owner = nums.inner.x_only_public_key().0;
    for (arkade_script, signers) in [
        (&chop_arkade_script, [operator_pk].as_slice()),
        (&regrowth_arkade_script, [operator_pk].as_slice()),
        (
            &maintenance_arkade_script,
            [operator_pk, rollover_pk].as_slice(),
        ),
    ] {
        let tweaked_emulator =
            ark_script::compute_arkade_script_public_key(&emulator_pk, arkade_script)
                .context("derive tree emulator signer")?;
        if signers.contains(&tweaked_emulator) {
            return Err(anyhow!("tweaked emulator collides with a tree signer"));
        }
        if tweaked_emulator == owner {
            return Err(anyhow!("tweaked emulator collides with the exit key"));
        }
    }
    let leaf = |arkade_script: ScriptBuf, pubkeys: Vec<XOnlyPublicKey>| {
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script,
            tapscript: ArkadeTapscript::Multisig { pubkeys },
            introspectors: vec![emulator_pk],
        })
    };
    let processed = ArkadeVtxoScript::new(vec![
        leaf(chop_arkade_script.clone(), vec![operator_pk]),
        leaf(regrowth_arkade_script.clone(), vec![operator_pk]),
        leaf(
            maintenance_arkade_script.clone(),
            vec![operator_pk, rollover_pk],
        ),
    ])
    .context("build tree Arkade tapleaves")?;
    let [chop_spend_script, regrowth_spend_script, maintenance_spend_script] =
        processed.scripts.as_slice()
    else {
        return Err(anyhow!(
            "tree contract must have chop, regrowth, and maintenance leaves"
        ));
    };
    let chop_spend_script = chop_spend_script.clone();
    let regrowth_spend_script = regrowth_spend_script.clone();
    let maintenance_spend_script = maintenance_spend_script.clone();
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
        regrowth_spend_script,
        regrowth_arkade_script,
        maintenance_spend_script,
        maintenance_arkade_script,
    })
}

/// Build the tree half of the atomic PLAYER/TREE chop.
///
/// The tree transfers LOG and soulbound XP into owner-authorized player state.
/// The XP asset balance is the only progression value.
///
/// Canonical shape:
///
/// ```text
/// vin 0 player | vin 1 tree
/// vout 0 player | vout 1 tree | vout 2 extension | vout 3 anchor
/// groups 0..3 PLAYER_ID | TREE | LOG | XP
/// ```
///
/// This half owns the global game rule: tree identity, player-bound luck
/// advancement, success probability, health, reward movement, and fixed supply.
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
    let tree_value = script_int(tree_value_sats(dust_sats), "tree value")?;
    let anchor_program =
        witness_v1_program(&ark_core::anchor_output().script_pubkey, "Arkade anchor")?;
    let builder = push_chop_shape(Builder::new(), TREE_INPUT_INDEX, &anchor_program)?
        .push_int(TREE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_int(3)
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

    let builder =
        crate::player::push_canonical_initial_player_luck(builder, PLAYER_STATE_INPUT_INDEX);
    // Compute the canonical reward once. Keep one copy on the altstack while
    // each conservation relation consumes a main-stack copy.
    let builder =
        crate::player::push_advanced_player_luck(builder, PLAYER_STATE_INPUT_INDEX, xp_asset)
            .push_opcode(OP_TOALTSTACK);

    let builder = push_tree_health_amounts(builder, TREE_INPUT_INDEX)
        .push_opcode(OP_OVER)
        .push_int(0)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    let builder = builder
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);

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
    let builder = builder
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, log_asset)
        .push_opcode(OP_DROP);
    let builder = push_optional_output_asset_lookup(builder, PLAYER_STATE_OUTPUT_INDEX, log_asset)
        .push_opcode(OP_DROP);
    let builder = builder
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_ROT)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);

    // XP is conserved and moved into player state as its sole progression value.
    let builder = push_input_asset_lookup(builder, TREE_INPUT_INDEX, xp_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int(0)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    let builder = push_optional_output_asset_lookup(builder, TREE_OUTPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    let builder = builder
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    let builder = push_optional_output_asset_lookup(builder, PLAYER_STATE_OUTPUT_INDEX, xp_asset)
        .push_opcode(OP_DROP);
    Ok(builder
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_ROT)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUAL)
        .into_script())
}

/// Permissionless renewal for one funded stump. Active trees and reserve-empty
/// terminal stumps cannot use this leaf.
pub fn tree_regrowth_covenant_script(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
) -> Result<ScriptBuf> {
    let builder = tree_renewal_base(tree_asset, log_asset, xp_asset)?;
    let builder = push_health_packet_value(builder, Some(RENEWAL_STATE_INPUT_INDEX))
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_optional_input_asset_lookup(builder, RENEWAL_STATE_INPUT_INDEX, log_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(0)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY);
    let builder = push_health_packet_value(builder, None)
        .push_int(LOGS_PER_TREE as i64)
        .push_opcode(OP_EQUALVERIFY);
    Ok(builder.push_int(1).into_script())
}

/// Guarded lifecycle renewal for active trees and terminal stumps. The state,
/// health, assets, P2TR, and value must all remain exact.
pub fn tree_maintenance_covenant_script(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
) -> Result<ScriptBuf> {
    let builder = tree_renewal_base(tree_asset, log_asset, xp_asset)?;
    let builder = push_health_packet_value(builder, Some(RENEWAL_STATE_INPUT_INDEX));
    let builder = push_health_packet_value(builder, None).push_opcode(OP_EQUALVERIFY);
    Ok(builder.push_int(1).into_script())
}

fn tree_renewal_base(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
) -> Result<Builder> {
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
        .push_opcode(OP_EQUALVERIFY))
}

/// Shared proof shape for every woodland.sh renewal leaf. The spending
/// transaction is either an exact state self-send or the same self-send plus
/// one asset-free wallet input and an asset-free change output for fees.
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
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(RENEWAL_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_ELSE)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(RENEWAL_FEE_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(RENEWAL_FEE_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(RENEWAL_FEE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(RENEWAL_FEE_CHANGE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(RENEWAL_FEE_CHANGE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(RENEWAL_FEE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_ENDIF))
}

/// Require a state packet to be carried byte-for-byte from the given input
/// into this transaction's merged extension.
pub(crate) fn push_equal_state_packet_at(
    builder: Builder,
    packet_type: u8,
    input_index: usize,
) -> Builder {
    builder
        .push_int(packet_type.into())
        .push_int(input_index as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(packet_type.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
}

/// Require the renewed state packet to be carried byte-for-byte from the
/// previous transaction into this proof's merged extension.
pub(crate) fn push_equal_state_packet(builder: Builder, packet_type: u8) -> Builder {
    push_equal_state_packet_at(builder, packet_type, RENEWAL_STATE_INPUT_INDEX)
}

/// Pin the extension shape and prove assets cannot leak anywhere except the
/// state output. The optional fee input and change output are asset-free.
pub(crate) fn push_renewal_asset_shell(builder: Builder) -> Result<Builder> {
    let builder = builder
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(RENEWAL_INPUT_COUNT as i64)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF);
    let builder =
        push_assetless_extension(builder, RENEWAL_EXTENSION_OUTPUT_INDEX).push_opcode(OP_ELSE);
    let builder =
        push_assetless_extension(builder, RENEWAL_FEE_EXTENSION_OUTPUT_INDEX).push_opcode(OP_ENDIF);
    Ok(builder
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(RENEWAL_STATE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(i64::from(RENEWAL_STATE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_opcode(OP_EQUALVERIFY))
}

fn push_assetless_extension(builder: Builder, output_index: u16) -> Builder {
    builder
        .push_int(i64::from(output_index))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(output_index))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(-1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DROP)
        .push_int(i64::from(output_index))
        .push_opcode(op::INSPECTOUTASSETCOUNT)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
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

pub(crate) fn push_extension_and_anchor_shape(
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

pub(crate) fn push_chop_shape(
    builder: Builder,
    current_input_index: usize,
    anchor_program: &PushBytesBuf,
) -> Result<Builder> {
    let builder = builder
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(current_input_index as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(CHOP_INPUT_COUNT as i64)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(CHOP_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_extension_and_anchor_shape(
        builder,
        CHOP_EXTENSION_OUTPUT_INDEX,
        CHOP_ANCHOR_OUTPUT_INDEX,
        anchor_program,
    )
    .push_opcode(OP_ELSE)
    .push_opcode(op::INSPECTNUMINPUTS)
    .push_int(CHOP_FEE_INPUT_COUNT as i64)
    .push_opcode(OP_EQUALVERIFY)
    .push_opcode(op::INSPECTNUMOUTPUTS)
    .push_int(CHOP_FEE_OUTPUT_COUNT as i64)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(CHOP_FEE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(CHOP_FEE_CHANGE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(CHOP_FEE_CHANGE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
    .push_int(1)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(CHOP_FEE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTSCRIPTPUBKEY)
    .push_int(1)
    .push_opcode(OP_EQUALVERIFY)
    .push_opcode(OP_EQUALVERIFY);
    let builder = push_extension_and_anchor_shape(
        builder,
        CHOP_FEE_EXTENSION_OUTPUT_INDEX,
        CHOP_FEE_ANCHOR_OUTPUT_INDEX,
        anchor_program,
    );
    Ok(builder
        .push_opcode(OP_ENDIF)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(CHOP_ASSET_GROUP_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY))
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

pub(crate) fn witness_v1_program(script: &ScriptBuf, name: &str) -> Result<PushBytesBuf> {
    if script.witness_version() != Some(WitnessVersion::V1) {
        return Err(anyhow!("{name} must be a witness-v1 program"));
    }
    PushBytesBuf::try_from(script.as_bytes()[2..].to_vec())
        .map_err(|error| anyhow!("invalid {name} witness program: {error}"))
}

pub(crate) fn script_int(value: u64, name: &str) -> Result<i64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::hex::DisplayHex;
    use bitcoin::secp256k1::{Keypair, SecretKey};
    use bitcoin::{Address, Txid};

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

    fn player_script(byte: u8) -> ScriptBuf {
        let secp = Secp256k1::new();
        Address::p2tr(&secp, xonly(&secp, byte), None, Network::Regtest).script_pubkey()
    }

    fn luck_for(
        player_xp: u64,
        expected_success: bool,
    ) -> (crate::player::PlayerLuck, crate::player::PlayerLuck) {
        let mut previous = crate::player::PlayerLuck::initial(&player_script(7)).unwrap();
        previous.credit = crate::player::PlayerLuckCredit::new(
            crate::player::CHOP_ROLL_BASIS_POINTS - crate::player::log_drop_basis_points(player_xp),
        )
        .unwrap();
        loop {
            let (next, success) = previous.advance(player_xp);
            if success == expected_success {
                return (previous, next);
            }
            previous.roll = next.roll;
        }
    }

    #[test]
    fn transaction_shape_distinguishes_deployment_renewal_and_chop() {
        assert_eq!(
            classify_transition_shape(0, None, true).unwrap(),
            TreeTransition::Deployment
        );
        assert_eq!(
            classify_transition_shape(1, None, false).unwrap(),
            TreeTransition::Renewal
        );
        assert_eq!(
            classify_transition_shape(CHOP_INPUT_COUNT, Some(CHOP_ASSET_GROUP_COUNT), false)
                .unwrap(),
            TreeTransition::Chop
        );
        assert_eq!(
            classify_transition_shape(CHOP_FEE_INPUT_COUNT, Some(CHOP_ASSET_GROUP_COUNT), false,)
                .unwrap(),
            TreeTransition::Chop
        );
        assert!(classify_transition_shape(CHOP_INPUT_COUNT, Some(2), false).is_err());
        assert!(classify_transition_shape(4, Some(CHOP_ASSET_GROUP_COUNT), false).is_err());
    }

    #[test]
    fn tree_wire_vectors_are_stable() {
        let state = state();
        assert_eq!(
            state.encode().to_lower_hex_string(),
            "545201a101000007000d00"
        );
        assert_eq!(
            TreeHealth::new(LOGS_PER_TREE)
                .unwrap()
                .encode()
                .to_lower_hex_string(),
            "0a0000000000000000"
        );
        let mut negative_zero = [0_u8; 9];
        negative_zero[8] = 0x80;
        assert!(TreeHealth::decode(&negative_zero).is_err());
    }
    #[test]
    fn successful_chop_moves_xp_and_log_to_player() {
        let (previous_player_luck, next_player_luck) = luck_for(0, true);
        let valid = ChopTransition {
            previous_state: state(),
            next_state: state(),
            previous_player_luck,
            next_player_luck,
            previous_health: TreeHealth::new(5).unwrap(),
            next_health: TreeHealth::new(4).unwrap(),
            player_xp_before: 0,
            success: true,
            dust_sats: 330,
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
            tree_value_before: 330,
            tree_value_after: 330,
            tree_markers_before: 1,
            tree_markers_after: 1,
        };
        valid.validate().unwrap();

        let mut unburned_xp = valid;
        unburned_xp.tree_xp_balance_after = 5;
        assert!(unburned_xp.validate().is_err());
        let mut overburned_xp = valid;
        overburned_xp.tree_xp_balance_after = 3;
        assert!(overburned_xp.validate().is_err());
        let mut missing_xp = valid;
        missing_xp.player_xp_balance_after = 0;
        assert!(missing_xp.validate().is_err());
        let mut extra_xp = valid;
        extra_xp.player_xp_balance_after = 2;
        assert!(extra_xp.validate().is_err());
    }

    #[test]
    fn failed_chop_moves_no_assets() {
        let (previous_player_luck, next_player_luck) = luck_for(0, false);
        let valid = ChopTransition {
            previous_state: state(),
            next_state: state(),
            previous_player_luck,
            next_player_luck,
            previous_health: TreeHealth::new(5).unwrap(),
            next_health: TreeHealth::new(5).unwrap(),
            player_xp_before: 0,
            success: false,
            dust_sats: 330,
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
            tree_value_before: 330,
            tree_value_after: 330,
            tree_markers_before: 1,
            tree_markers_after: 1,
        };
        valid.validate().unwrap();

        let mut forged_xp = valid;
        forged_xp.player_xp_balance_after = 1;
        assert!(forged_xp.validate().is_err());
        let mut burned_xp = valid;
        burned_xp.tree_xp_balance_after = 4;
        assert!(burned_xp.validate().is_err());
    }

    #[test]
    fn level_bonus_thresholds_and_cap_are_exact() {
        assert_eq!(
            crate::player::LEVEL_LOG_DROP_XP_THRESHOLDS,
            [10, 20, 30, 40, 50].map(|level| crate::player::xp_for_level(level).unwrap())
        );
        for (xp, basis_points) in [
            (0, 2_000),
            (1_153, 2_000),
            (1_154, 2_200),
            (4_469, 2_200),
            (4_470, 2_400),
            (13_362, 2_400),
            (13_363, 2_600),
            (37_223, 2_600),
            (37_224, 2_800),
            (101_332, 2_800),
            (101_333, 3_000),
            (u64::MAX, 3_000),
        ] {
            assert_eq!(
                crate::player::log_drop_basis_points(xp),
                basis_points,
                "XP {xp}"
            );
        }
    }

    #[test]
    fn one_batch_regrows_only_funded_stumps() {
        let regrow = |health: u64, logs: u64| -> u64 {
            if health == 0 && logs > 0 {
                LOGS_PER_TREE
            } else {
                health
            }
        };
        assert_eq!(regrow(0, 1), LOGS_PER_TREE);
        assert_eq!(regrow(0, 0), 0);
        assert_eq!(regrow(5, 1), 5);

        let tree = asset(1, 0);
        let log = asset(1, 1);
        let xp = asset(1, 2);
        let regrowth = tree_regrowth_covenant_script(tree, log, xp).unwrap();
        let maintenance = tree_maintenance_covenant_script(tree, log, xp).unwrap();
        assert_ne!(regrowth, maintenance);
        for script in [&regrowth, &maintenance] {
            assert!(!ark_script::to_asm(script)
                .unwrap()
                .contains("OP_INSPECTLOCKTIME"));
            assert!(script.len() <= 10_000);
        }
    }

    #[test]
    fn many_permissionless_players_preserve_world_supply_across_many_schedules() {
        #[derive(Clone, Copy)]
        struct SimTree {
            state: TreeState,
            health: TreeHealth,
            logs: u64,
            xp_balance: u64,
            regrowths: u64,
        }

        #[derive(Clone, Copy)]
        struct SimPlayer {
            state: crate::player::PlayerState,
            logs: u64,
            xp_balance: u64,
        }

        for seed in 1_u64..=32 {
            let mut players = (0_u16..256)
                .map(|index| SimPlayer {
                    state: crate::player::PlayerState {
                        luck: crate::player::PlayerLuck::initial(&player_script(
                            (index % 250 + 1) as u8,
                        ))
                        .unwrap(),
                    },
                    logs: 0,
                    xp_balance: 0,
                })
                .collect::<Vec<_>>();
            let mut trees = (0_u32..10)
                .map(|index| SimTree {
                    state: TreeState {
                        tree_id: index,
                        x: index as u16,
                        y: (index * 2) as u16,
                    },
                    health: TreeHealth::new(LOGS_PER_TREE).unwrap(),
                    logs: 100,
                    xp_balance: 100,
                    regrowths: 0,
                })
                .collect::<Vec<_>>();
            let mut entropy = seed;
            let mut attempts = 0_u64;

            loop {
                // A permissionless renewal immediately turns every funded
                // stump into a fresh-health tree.
                for tree in &mut trees {
                    if tree.health.value() == 0 && tree.logs > 0 {
                        tree.health = TreeHealth::new(LOGS_PER_TREE).unwrap();
                        tree.regrowths += 1;
                    }
                }

                let active_count = trees.iter().filter(|tree| tree.health.value() > 0).count();
                if active_count == 0 {
                    if trees.iter().any(|tree| tree.logs > 0) {
                        continue;
                    }
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
                let player_index = (entropy as usize) % players.len();

                let tree = &mut trees[tree_index];
                let player = &mut players[player_index];
                let previous_health = tree.health;
                let previous_logs = tree.logs;
                let previous_tree_xp_balance = tree.xp_balance;
                let previous_player_state = player.state;
                let previous_player_logs = player.logs;
                let previous_player_xp_balance = player.xp_balance;
                let previous_xp = previous_player_xp_balance;
                let (next_player_luck, success) = previous_player_state.luck.advance(previous_xp);
                let reward = u64::from(success);
                let next_health =
                    TreeHealth::new(previous_health.value().checked_sub(reward).unwrap()).unwrap();
                let next_logs = previous_logs.checked_sub(reward).unwrap();
                let next_tree_xp_balance = previous_tree_xp_balance.checked_sub(reward).unwrap();
                let next_player_logs = previous_player_logs.checked_add(reward).unwrap();
                let next_player_xp_balance =
                    previous_player_xp_balance.checked_add(reward).unwrap();
                let next_player_state = crate::player::PlayerState {
                    luck: next_player_luck,
                };

                ChopTransition {
                    previous_state: tree.state,
                    next_state: tree.state,
                    previous_player_luck: previous_player_state.luck,
                    next_player_luck,
                    previous_health,
                    next_health,
                    player_xp_before: previous_xp,
                    success,
                    dust_sats: 330,
                    tree_markers_before: 1,
                    tree_markers_after: 1,
                    tree_logs_before: previous_logs,
                    tree_logs_after: next_logs,
                    tree_xp_balance_before: previous_tree_xp_balance,
                    tree_xp_balance_after: next_tree_xp_balance,
                    player_logs_before: previous_player_logs,
                    player_logs_after: next_player_logs,
                    player_xp_balance_before: previous_player_xp_balance,
                    player_xp_balance_after: next_player_xp_balance,
                    player_value_before: 330,
                    player_value_after: 330,
                    tree_value_before: 330,
                    tree_value_after: 330,
                }
                .validate()
                .unwrap();

                crate::player::PlayerChopTransition {
                    previous_state: previous_player_state,
                    next_state: next_player_state,
                    previous_tree_state: tree.state,
                    next_tree_state: tree.state,
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
                    tree_xp_balance_before: previous_tree_xp_balance,
                    tree_xp_balance_after: next_tree_xp_balance,
                    state_value_before: 330,
                    state_value_after: 330,
                    tree_value_before: 330,
                    tree_value_after: 330,
                    dust_sats: 330,
                }
                .validate()
                .unwrap();

                tree.health = next_health;
                tree.logs = next_logs;
                tree.xp_balance = next_tree_xp_balance;
                player.state = next_player_state;
                player.logs = next_player_logs;
                player.xp_balance = next_player_xp_balance;
                attempts += 1;
                assert!(attempts < 20_000, "schedule {seed} did not terminate");
            }

            for tree in &trees {
                assert!(
                    tree.regrowths > 0,
                    "seed {seed}, tree {} never regrew",
                    tree.state.tree_id
                );
                assert_eq!(
                    tree.health.value(),
                    0,
                    "seed {seed}, tree {} health",
                    tree.state.tree_id
                );
                assert_eq!(tree.logs, 0, "seed {seed}, tree {} LOG", tree.state.tree_id);
                assert_eq!(
                    tree.xp_balance, 0,
                    "seed {seed}, tree {} XP",
                    tree.state.tree_id
                );
            }
            assert!(players
                .iter()
                .all(|player| player.logs == player.xp_balance));
            assert_eq!(
                trees.iter().map(|tree| tree.logs).sum::<u64>()
                    + players.iter().map(|player| player.logs).sum::<u64>(),
                1_000
            );
            assert_eq!(
                trees.iter().map(|tree| tree.xp_balance).sum::<u64>()
                    + players.iter().map(|player| player.xp_balance).sum::<u64>(),
                1_000
            );
        }
    }

    #[test]
    fn scripts_commit_assets_health_and_split_regrowth_from_maintenance() {
        let tree = asset(1, 0);
        let log = asset(1, 1);
        let xp_balance = asset(1, 2);
        let chop = tree_covenant_script(tree, log, xp_balance, 330).unwrap();
        let regrowth = tree_regrowth_covenant_script(tree, log, xp_balance).unwrap();
        let maintenance = tree_maintenance_covenant_script(tree, log, xp_balance).unwrap();
        for script in [&chop, &regrowth, &maintenance] {
            assert!(script.len() <= 10_000);
            assert!(!ark_script::to_asm(script)
                .unwrap()
                .contains("OP_INSPECTLOCKTIME"));
        }
        assert_ne!(regrowth, maintenance);
        let chop_asm = ark_script::to_asm(&chop).unwrap();
        assert!(chop_asm.contains("OP_SHA256"));
        assert!(chop_asm.contains("OP_BIN2NUM"));
        assert!(chop_asm.contains("OP_NUM2BIN"));
        assert!(chop_asm.contains("OP_MOD"));
        assert!(chop_asm.contains("OP_SUB") || chop_asm.contains("OP_ADD"));

        let secp = Secp256k1::new();
        let operator = xonly(&secp, 3);
        let emulator = xonly(&secp, 4);
        let rollover = xonly(&secp, 5);
        let contract = build_tree_contract(
            &secp,
            operator,
            emulator,
            rollover,
            Sequence::from_height(144),
            Network::Regtest,
            tree,
            log,
            xp_balance,
            330,
        )
        .unwrap();
        assert_eq!(contract.vtxo.tapscripts().len(), 4);
        for (spend, arkade) in [
            (&contract.chop_spend_script, &contract.chop_arkade_script),
            (
                &contract.regrowth_spend_script,
                &contract.regrowth_arkade_script,
            ),
        ] {
            let tweaked = ark_script::compute_arkade_script_public_key(&emulator, arkade).unwrap();
            let signers = ark_core::script::extract_checksig_pubkeys(spend);
            assert_eq!(signers.len(), 2);
            assert!(signers.contains(&operator));
            assert!(signers.contains(&tweaked));
        }
        let maintenance_tweaked = ark_script::compute_arkade_script_public_key(
            &emulator,
            &contract.maintenance_arkade_script,
        )
        .unwrap();
        let maintenance_signers =
            ark_core::script::extract_checksig_pubkeys(&contract.maintenance_spend_script);
        assert_eq!(maintenance_signers.len(), 3);
        assert!(maintenance_signers.contains(&operator));
        assert!(maintenance_signers.contains(&rollover));
        assert!(maintenance_signers.contains(&maintenance_tweaked));

        let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY.parse().unwrap();
        let disabled_exit = ark_core::script::csv_sig_script(
            Sequence::from_height(144),
            nums.inner.x_only_public_key().0,
        );
        assert!(contract.vtxo.tapscripts().contains(&disabled_exit));
    }

    #[test]
    fn tree_contract_rejects_tweaked_emulator_collision() {
        let secp = Secp256k1::new();
        let emulator = xonly(&secp, 4);
        let rollover = xonly(&secp, 5);
        let (tree, log, xp_balance) = (asset(1, 0), asset(1, 1), asset(1, 2));
        let chop = tree_covenant_script(tree, log, xp_balance, 330).unwrap();
        let regrowth = tree_regrowth_covenant_script(tree, log, xp_balance).unwrap();
        let maintenance = tree_maintenance_covenant_script(tree, log, xp_balance).unwrap();
        // An operator key equal to a script-tweaked emulator key could
        // satisfy the emulator position without executing the covenant.
        for script in [&chop, &regrowth, &maintenance] {
            let operator = ark_script::compute_arkade_script_public_key(&emulator, script).unwrap();
            let error = build_tree_contract(
                &secp,
                operator,
                emulator,
                rollover,
                Sequence::from_height(144),
                Network::Regtest,
                tree,
                log,
                xp_balance,
                330,
            )
            .err()
            .unwrap();
            assert_eq!(
                error.to_string(),
                "tweaked emulator collides with a tree signer"
            );
        }
    }
}
