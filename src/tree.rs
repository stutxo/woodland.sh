//! Recursive tree covenant primitives.
//!
//! Tree health is a numeric packet separate from the tree's fixed LOG reserve.
//! Successful chops reduce both health and the remaining LOG/XP inventory;
//! renewal resets health without issuing assets.

use crate::protocol::{
    CHOP_ANCHOR_OUTPUT_INDEX, CHOP_ASSET_GROUP_COUNT, CHOP_EXTENSION_OUTPUT_INDEX,
    CHOP_INPUT_COUNT, CHOP_OUTPUT_COUNT, LOG_ASSET_GROUP_INDEX, PLAYER_IDENTITY_PACKET_TYPE,
    PLAYER_ID_ASSET_GROUP_INDEX, PLAYER_POSITION_PACKET_TYPE, PLAYER_STATE_INPUT_INDEX,
    PLAYER_STATE_OUTPUT_INDEX, RENEWAL_EXTENSION_OUTPUT_INDEX, RENEWAL_INPUT_COUNT,
    RENEWAL_OUTPUT_COUNT, RENEWAL_STATE_INPUT_INDEX, RENEWAL_STATE_OUTPUT_INDEX,
    RESTOCK_ANCHOR_OUTPUT_INDEX, RESTOCK_ASSET_GROUP_COUNT, RESTOCK_EXTENSION_OUTPUT_INDEX,
    RESTOCK_INPUT_COUNT, RESTOCK_OUTPUT_COUNT, RESTOCK_TREE_INPUT_INDEX, RESTOCK_TREE_OUTPUT_INDEX,
    RESTOCK_VAULT_INPUT_INDEX, TREE_ASSET_GROUP_INDEX, TREE_HEALTH_PACKET_TYPE, TREE_INPUT_INDEX,
    TREE_OUTPUT_INDEX, TREE_STATE_PACKET_TYPE, XP_ASSET_GROUP_INDEX,
};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use ark_script::{op, ArkadeLeaf, ArkadeTapscript, ArkadeVtxoInput, ArkadeVtxoScript};
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::{
    OP_2DROP, OP_ADD, OP_BOOLAND, OP_DROP, OP_DUP, OP_ELSE, OP_ENDIF, OP_EQUAL, OP_EQUALVERIFY,
    OP_FROMALTSTACK, OP_GREATERTHAN, OP_IF, OP_NIP, OP_NUMEQUAL, OP_OVER, OP_ROT, OP_SIZE, OP_SWAP,
    OP_TOALTSTACK, OP_VERIFY,
};
use bitcoin::script::witness_version::WitnessVersion;
use bitcoin::script::{Builder, PushBytesBuf};
use bitcoin::secp256k1::{Secp256k1, Verification};
use bitcoin::{Network, Psbt, ScriptBuf, Sequence, Transaction, XOnlyPublicKey};

pub const LOGS_PER_TREE: u64 = 5;

/// A tree carries exactly one dust of backing. LOG/XP are ledger entries with
/// no sats collateral, and the covenant pins this value through every
/// transition; on depletion the restock recycles it in-transaction.
pub fn tree_value_sats(dust_sats: u64) -> u64 {
    dust_sats
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeTransition {
    Deployment,
    Renewal,
    Chop,
    Restock,
}

impl TreeTransition {
    pub(crate) const fn output_index(self) -> u32 {
        match self {
            Self::Deployment | Self::Renewal | Self::Restock => 0,
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
        (CHOP_INPUT_COUNT, Some(CHOP_ASSET_GROUP_COUNT)) => Ok(TreeTransition::Chop),
        (RESTOCK_INPUT_COUNT, Some(RESTOCK_ASSET_GROUP_COUNT)) => Ok(TreeTransition::Restock),
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
        let (expected_luck, expected_success) =
            self.previous_player_luck.advance(self.player_xp_before);
        if self.next_player_luck != expected_luck || self.success != expected_success {
            return Err(anyhow!("tree chop luck transition is invalid"));
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

/// The complete contract material needed to fund and later spend a tree VTXO.
#[derive(Clone, Debug)]
pub struct TreeContract {
    pub vtxo: ark_core::Vtxo,
    pub chop_spend_script: ScriptBuf,
    pub chop_arkade_script: ScriptBuf,
    pub renewal_spend_script: ScriptBuf,
    pub renewal_arkade_script: ScriptBuf,
    pub retire_spend_script: ScriptBuf,
    pub retire_arkade_script: ScriptBuf,
}

/// Build the shared three-leaf tree contract: swing, permissionless renewal
/// with stump health refill, and permissionless retire-and-restock.
///
/// Every usable tapleaf is operator + covenant-tweaked emulator. No project-held
/// signer appears in the tree covenant: stumps refill when any renewal settles,
/// and depleted trees are replaced atomically with the supply vault by anyone.
///
/// The CSV exit is keyed to the NUMS point and is intentionally unusable as an
/// escape from the recursive covenant.
#[allow(clippy::too_many_arguments)]
pub fn build_tree_contract<C: Verification>(
    secp: &Secp256k1<C>,
    operator_pk: XOnlyPublicKey,
    emulator_pk: XOnlyPublicKey,
    exit_delay: Sequence,
    network: Network,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    log_reserve_per_tree: u64,
    xp_per_tree: u64,
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
    if operator_pk == emulator_pk {
        return Err(anyhow!("tree contract signers must be distinct"));
    }
    script_int(tree_value_sats(dust_sats), "tree value")?;

    let chop_arkade_script = tree_covenant_script(tree_asset, log_asset, xp_asset, dust_sats)?;
    let renewal_arkade_script = tree_renewal_covenant_script(tree_asset, log_asset, xp_asset)?;
    let retire_arkade_script = tree_retire_covenant_script(
        tree_asset,
        log_asset,
        xp_asset,
        log_reserve_per_tree,
        xp_per_tree,
        dust_sats,
    )?;
    // arkd requires a timelocked exit leaf on every batch VTXO. Shared trees
    // key it to the NUMS owner: it satisfies the server's exit-delay
    // accounting without giving anyone a unilateral path around the covenant.
    let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY
        .parse()
        .context("parse Arkade NUMS key")?;
    let owner = nums.inner.x_only_public_key().0;
    // Each covenant leaf's emulator position is the script-tweaked key
    // emulator + H("ArkScriptHash", script)·G. A tweaked key equal to a plain
    // signer of the same leaf lets that signer take the emulator path without
    // covenant execution; equal to the NUMS exit owner the leaf is dead.
    for (arkade_script, signers) in [
        (&chop_arkade_script, [operator_pk].as_slice()),
        (&renewal_arkade_script, [operator_pk].as_slice()),
        (&retire_arkade_script, [operator_pk].as_slice()),
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
        leaf(renewal_arkade_script.clone(), vec![operator_pk]),
        leaf(retire_arkade_script.clone(), vec![operator_pk]),
    ])
    .context("build tree Arkade tapleaves")?;
    let [chop_spend_script, renewal_spend_script, retire_spend_script] =
        processed.scripts.as_slice()
    else {
        return Err(anyhow!(
            "tree contract must have chop, renewal, and retire leaves"
        ));
    };
    let chop_spend_script = chop_spend_script.clone();
    let renewal_spend_script = renewal_spend_script.clone();
    let retire_spend_script = retire_spend_script.clone();
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
        renewal_spend_script,
        renewal_arkade_script,
        retire_spend_script,
        retire_arkade_script,
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
    let builder =
        crate::player::push_canonical_initial_player_luck(builder, PLAYER_STATE_INPUT_INDEX);
    // Compute the canonical reward once. Keep one copy on the altstack while
    // each conservation relation consumes a main-stack copy.
    let builder = crate::player::push_advanced_player_luck(builder, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_TOALTSTACK);

    // XP advances exactly when the player-bound luck transition succeeds.
    let builder = crate::player::push_player_xp_amounts(builder, PLAYER_STATE_INPUT_INDEX)
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_ROT)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUALVERIFY);
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
    let builder = builder
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
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
    Ok(builder
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_ROT)
        .push_opcode(OP_ADD)
        .push_opcode(OP_EQUAL)
        .into_script())
}

/// Pin an asset group's single input cell to a local-typed assignment at the
/// given input index, so restocked assets can only originate from the vault
/// and the marker only from the dead tree.
pub(crate) fn push_restock_asset_input(
    builder: Builder,
    asset: AssetId,
    input_index: usize,
) -> Builder {
    push_asset_group_index(builder, asset)
        .push_int(0)
        .push_int(0)
        .push_opcode(op::INSPECTASSETGROUP)
        .push_opcode(OP_DROP)
        .push_int(input_index as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
}

/// Tree half of the atomic retire-and-restock. A fully depleted tree (its LOG
/// and XP balances are gone, so it carries only the TREE marker) is spent
/// together with the supply vault and replaced at the same coordinate: new
/// full reserve and health, with the same recycled dust and marker. Player
/// reward entropy is independent of tree restock. The vault half pins supply.
///
/// Canonical shape:
///
/// ```text
/// vin 0 dead tree | vin 1 vault
/// vout 0 new tree | vout 1 vault | vout 2 extension | vout 3 anchor
/// groups 0..2 TREE | LOG | XP
/// ```
pub fn tree_retire_covenant_script(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    log_reserve_per_tree: u64,
    xp_per_tree: u64,
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
    let log_reserve = script_int(log_reserve_per_tree, "tree LOG reserve")?;
    let xp_reserve = script_int(xp_per_tree, "tree XP reserve")?;
    let anchor_program =
        witness_v1_program(&ark_core::anchor_output().script_pubkey, "Arkade anchor")?;
    let builder = Builder::new()
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(RESTOCK_TREE_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(RESTOCK_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(RESTOCK_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(RESTOCK_ASSET_GROUP_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        // A depleted tree carries only its marker: LOG and XP hit zero
        // together and zero-amount assets are never indexed.
        .push_int(RESTOCK_TREE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    // The replacement keeps the exact covenant P2TR, the recycled dust, and
    // the immutable identity packet.
    let builder = push_equal_input_output_scripts(
        builder,
        RESTOCK_TREE_INPUT_INDEX,
        RESTOCK_TREE_OUTPUT_INDEX,
    )
    .push_int(i64::from(RESTOCK_TREE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(RESTOCK_TREE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(TREE_STATE_PACKET_TYPE.into())
    .push_int(RESTOCK_TREE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTPACKET)
    .push_int(1)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(TREE_STATE_PACKET_TYPE.into())
    .push_opcode(op::INSPECTPACKET)
    .push_int(1)
    .push_opcode(OP_EQUALVERIFY)
    .push_opcode(OP_EQUALVERIFY);
    // Health resets to full, and the new tree carries exactly one dust.
    let dust = script_int(dust_sats, "tree dust")?;
    let builder = builder
        .push_int(i64::from(RESTOCK_TREE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(dust)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_health_packet_value(builder, None)
        .push_int(LOGS_PER_TREE as i64)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_extension_and_anchor_shape(
        builder,
        RESTOCK_EXTENSION_OUTPUT_INDEX,
        RESTOCK_ANCHOR_OUTPUT_INDEX,
        &anchor_program,
    )
    .push_int(i64::from(RESTOCK_EXTENSION_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(RESTOCK_ANCHOR_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY);

    let builder = push_canonical_asset_group(builder, tree_asset, 1, 1);
    let builder = push_restock_asset_input(builder, tree_asset, RESTOCK_TREE_INPUT_INDEX);
    let builder = push_canonical_asset_group(builder, log_asset, 1, 2);
    let builder = push_canonical_asset_group(builder, xp_asset, 1, 2);
    // LOG and XP enter only from the vault input: the single input cell of
    // each group is a local-typed assignment at the vault's vin.
    let builder = push_restock_asset_input(builder, log_asset, RESTOCK_VAULT_INPUT_INDEX);
    let builder = push_restock_asset_input(builder, xp_asset, RESTOCK_VAULT_INPUT_INDEX);

    let builder = push_input_asset_lookup(builder, RESTOCK_TREE_INPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_output_asset_lookup(builder, RESTOCK_TREE_OUTPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_output_asset_lookup(builder, RESTOCK_TREE_OUTPUT_INDEX, log_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(log_reserve)
        .push_opcode(OP_EQUALVERIFY);
    Ok(
        push_output_asset_lookup(builder, RESTOCK_TREE_OUTPUT_INDEX, xp_asset)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(xp_reserve)
            .push_opcode(OP_EQUAL)
            .into_script(),
    )
}

/// Covenant for the tree's batch-renewal leaf. It runs on a version-2 intent
/// proof and only permits an exact self-send: identical P2TR, value, TREE/LOG/XP
/// assets, identity, and health. A stump is refilled to health five; otherwise
/// renewal changes nothing and only re-enters the VTXO into a fresh batch.
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
    // Stumps with reserve left refill health on renewal; everything else
    // preserves it exactly. The branch condition is the canonical 0/1
    // MINIMALIF requires, ANDed with LOG presence so a depleted tree (no LOG
    // group) keeps its terminal zero health and stays renewable for restock.
    let builder = push_tree_health_amounts(builder, RENEWAL_STATE_INPUT_INDEX)
        .push_opcode(OP_SWAP)
        .push_opcode(OP_DUP)
        .push_int(0)
        .push_opcode(OP_NUMEQUAL);
    let builder = push_optional_input_asset_lookup(builder, RENEWAL_STATE_INPUT_INDEX, log_asset)
        .push_opcode(OP_DROP)
        .push_opcode(OP_BOOLAND)
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP)
        .push_int(LOGS_PER_TREE as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_ENDIF);
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

pub(crate) fn push_canonical_asset_group(
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

/// Attach both creating transactions, the replacement tree's state packets,
/// and both covenant leaves' emulator introspector entries for one atomic
/// retire-and-restock.
pub fn attach_restock_context(
    psbt: &mut Psbt,
    checkpoints: &[Psbt],
    contract: &TreeContract,
    vault: &crate::vault::VaultContract,
    tree_previous_tx: &Transaction,
    vault_previous_tx: &Transaction,
    state: TreeState,
) -> Result<()> {
    if psbt.unsigned_tx.input.len() != crate::protocol::RESTOCK_INPUT_COUNT {
        return Err(anyhow!(
            "tree restock requires exactly {} inputs",
            crate::protocol::RESTOCK_INPUT_COUNT
        ));
    }
    let mut updated = psbt.clone();
    crate::txbuild::attach_previous_ark_transactions(
        &mut updated,
        checkpoints,
        [tree_previous_tx, vault_previous_tx],
    )?;
    let previous_state = tree_state_from_tx(tree_previous_tx)?
        .ok_or_else(|| anyhow!("previous tree transaction has no state packet"))?;
    if previous_state != state {
        return Err(anyhow!("tree restock must keep the tree identity"));
    }
    let previous_health = tree_health_from_tx(tree_previous_tx)?
        .ok_or_else(|| anyhow!("previous tree transaction has no health packet"))?;
    if previous_health.value() != 0 {
        return Err(anyhow!("tree restock input is not depleted"));
    }
    attach_tree_state_packet(&mut updated, state)?;
    attach_tree_health_packet(&mut updated, TreeHealth::new(LOGS_PER_TREE)?)?;
    let packet = ark_core::introspector::packet::Packet::new(vec![
        ark_core::introspector::packet::IntrospectorEntry {
            vin: crate::protocol::RESTOCK_TREE_INPUT_INDEX as u16,
            script: contract.retire_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
        ark_core::introspector::packet::IntrospectorEntry {
            vin: crate::protocol::RESTOCK_VAULT_INPUT_INDEX as u16,
            script: vault.restock_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
    ])
    .context("build tree restock emulator packet")?;
    ark_core::introspector::packet::add_packet_to_psbt(&mut updated, &packet)
        .context("attach tree restock emulator packet")?;
    *psbt = updated;
    Ok(())
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

    fn luck_for(
        player_xp: u64,
        expected_success: bool,
    ) -> (crate::player::PlayerLuck, crate::player::PlayerLuck) {
        let identity = crate::player::PlayerIdentity { player_id: [7; 32] };
        let mut previous = crate::player::PlayerLuck::initial(identity);
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
    fn transaction_shape_distinguishes_chops_from_restock() {
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
            classify_transition_shape(RESTOCK_INPUT_COUNT, Some(RESTOCK_ASSET_GROUP_COUNT), false)
                .unwrap(),
            TreeTransition::Restock
        );
        assert!(classify_transition_shape(CHOP_INPUT_COUNT, Some(2), false).is_err());
    }

    #[test]
    fn tree_wire_vectors_are_stable() {
        let state = state();
        assert_eq!(
            state.encode().to_lower_hex_string(),
            "545201a101000007000d00"
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
            tree_value_before: 330,
            tree_value_after: 330,
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
    fn missed_chop_advances_player_luck_without_burning_inventory() {
        let (previous_player_luck, next_player_luck) = luck_for(0, false);
        let valid = ChopTransition {
            previous_state: state(),
            next_state: state(),
            previous_player_luck,
            next_player_luck,
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
            tree_value_before: 330,
            tree_value_after: 330,
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
    fn renewal_refills_only_zero_health() {
        // Host mirror of the covenant's conditional refill branch: renewal
        // preserves health unless the tree is a stump.
        let refill = |health: u64| -> u64 {
            if health == 0 {
                LOGS_PER_TREE
            } else {
                health
            }
        };
        assert_eq!(refill(0), LOGS_PER_TREE);
        for health in 1..=LOGS_PER_TREE {
            assert_eq!(refill(health), health);
        }
        // The renewal script carries the conditional as an IF/ELSE branch.
        let renewal = tree_renewal_covenant_script(asset(1, 0), asset(1, 1), asset(1, 2)).unwrap();
        let asm = ark_script::to_asm(&renewal).unwrap();
        assert!(asm.contains("OP_IF"));
        assert!(asm.contains("OP_ELSE"));
        assert!(asm.contains("OP_ENDIF"));
        assert!(renewal.len() <= 10_000);
    }

    #[test]
    fn many_permissionless_players_preserve_world_supply_across_many_schedules() {
        #[derive(Clone, Copy)]
        struct SimTree {
            state: TreeState,
            health: TreeHealth,
            logs: u64,
            xp_balance: u64,
            refills: u64,
        }

        #[derive(Clone, Copy)]
        struct SimPlayer {
            state: crate::player::PlayerState,
            logs: u64,
            xp_balance: u64,
        }

        for seed in 1_u64..=32 {
            let mut players = (0_u16..256)
                .map(|index| {
                    let identity = crate::player::PlayerIdentity {
                        player_id: [index as u8; 32],
                    };
                    SimPlayer {
                        state: crate::player::PlayerState {
                            identity,
                            position: crate::player::PlayerPosition { x: index, y: 17 },
                            luck: crate::player::PlayerLuck::initial(identity),
                            xp: crate::player::PlayerXp::new(0),
                        },
                        logs: 0,
                        xp_balance: 0,
                    }
                })
                .collect::<Vec<_>>();
            let mut trees = (0_u32..10)
                .map(|index| SimTree {
                    state: TreeState {
                        tree_id: 417 + index,
                        x: index as u16,
                        y: (index * 2) as u16,
                    },
                    health: TreeHealth::new(5).unwrap(),
                    logs: 10,
                    xp_balance: 10,
                    refills: 0,
                })
                .collect::<Vec<_>>();
            let mut entropy = seed;
            let mut attempts = 0_u64;

            loop {
                // Stumps refill on renewal as often as needed while reserve
                // remains; player luck and tree reserve are untouched.
                for tree in &mut trees {
                    if tree.health.value() == 0 && tree.logs > 0 {
                        tree.health = TreeHealth::new(LOGS_PER_TREE).unwrap();
                        tree.refills += 1;
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
                let player_index = (entropy as usize) % players.len();

                let tree = &mut trees[tree_index];
                let player = &mut players[player_index];
                let previous_health = tree.health;
                let previous_logs = tree.logs;
                let previous_tree_xp_balance = tree.xp_balance;
                let previous_player_state = player.state;
                let previous_player_logs = player.logs;
                let previous_player_xp_balance = player.xp_balance;
                let previous_xp = previous_player_state.xp.value();
                let (next_player_luck, success) = previous_player_state.luck.advance(previous_xp);
                let reward = u64::from(success);
                let next_health =
                    TreeHealth::new(previous_health.value().checked_sub(reward).unwrap()).unwrap();
                let next_logs = previous_logs.checked_sub(reward).unwrap();
                let next_tree_xp_balance = previous_tree_xp_balance.checked_sub(reward).unwrap();
                let next_xp = previous_xp.checked_add(reward).unwrap();
                let next_player_logs = previous_player_logs.checked_add(reward).unwrap();
                let next_player_xp_balance =
                    previous_player_xp_balance.checked_add(reward).unwrap();
                let next_player_state = crate::player::PlayerState {
                    luck: next_player_luck,
                    xp: crate::player::PlayerXp::new(next_xp),
                    ..previous_player_state
                };

                ChopTransition {
                    previous_state: tree.state,
                    next_state: tree.state,
                    previous_player_luck: previous_player_state.luck,
                    next_player_luck,
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

            assert!(trees.iter().all(|tree| {
                tree.refills > 0
                    && tree.health.value() == 0
                    && tree.logs == 0
                    && tree.xp_balance == 0
            }));
            assert!(players.iter().all(|player| {
                player.state.xp.value() == player.logs
                    && player.state.xp.value() == player.xp_balance
            }));
            assert_eq!(
                trees.iter().map(|tree| tree.logs).sum::<u64>()
                    + players.iter().map(|player| player.logs).sum::<u64>(),
                100
            );
            assert_eq!(
                trees.iter().map(|tree| tree.xp_balance).sum::<u64>()
                    + players.iter().map(|player| player.xp_balance).sum::<u64>(),
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
        let retire = tree_retire_covenant_script(tree, log, xp_balance, 1_000, 1_000, 330).unwrap();
        let renewal = tree_renewal_covenant_script(tree, log, xp_balance).unwrap();
        for script in [&chop, &retire, &renewal] {
            assert!(script.len() <= 10_000);
        }
        let retire_asm = ark_script::to_asm(&retire).unwrap();
        assert!(retire_asm.contains("OP_INSPECTNUMINPUTS OP_PUSHNUM_2 OP_EQUALVERIFY"));
        assert!(retire_asm.contains("OP_INSPECTNUMASSETGROUPS"));
        assert!(retire_asm.contains("OP_INSPECTINASSETCOUNT OP_PUSHNUM_1 OP_EQUALVERIFY"));
        let renewal_asm = ark_script::to_asm(&renewal).unwrap();
        assert!(renewal_asm.contains("OP_IF"));
        assert!(renewal_asm.contains("OP_ELSE"));
        let chop_asm = ark_script::to_asm(&chop).unwrap();
        assert!(chop_asm.contains("OP_SHA256"));
        assert!(chop_asm.contains("OP_BIN2NUM"));
        assert!(chop_asm.contains("OP_NUM2BIN"));
        assert!(chop_asm.contains("OP_MOD"));
        assert!(chop_asm.contains("OP_SUB") || chop_asm.contains("OP_ADD"));

        let secp = Secp256k1::new();
        let operator = xonly(&secp, 3);
        let emulator = xonly(&secp, 4);
        let contract = build_tree_contract(
            &secp,
            operator,
            emulator,
            Sequence::from_height(144),
            Network::Regtest,
            tree,
            log,
            xp_balance,
            1_000,
            1_000,
            330,
        )
        .unwrap();
        assert_eq!(contract.vtxo.tapscripts().len(), 4);
        for (spend, arkade) in [
            (&contract.chop_spend_script, &contract.chop_arkade_script),
            (
                &contract.renewal_spend_script,
                &contract.renewal_arkade_script,
            ),
            (
                &contract.retire_spend_script,
                &contract.retire_arkade_script,
            ),
        ] {
            let tweaked = ark_script::compute_arkade_script_public_key(&emulator, arkade).unwrap();
            let signers = ark_core::script::extract_checksig_pubkeys(spend);
            assert_eq!(signers.len(), 2);
            assert!(signers.contains(&operator));
            assert!(signers.contains(&tweaked));
        }
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
        let (tree, log, xp_balance) = (asset(1, 0), asset(1, 1), asset(1, 2));
        let chop = tree_covenant_script(tree, log, xp_balance, 330).unwrap();
        let retire = tree_retire_covenant_script(tree, log, xp_balance, 1_000, 1_000, 330).unwrap();
        let renewal = tree_renewal_covenant_script(tree, log, xp_balance).unwrap();
        // An operator key equal to a script-tweaked emulator key could
        // satisfy the emulator position without executing the covenant.
        for script in [&chop, &retire, &renewal] {
            let operator = ark_script::compute_arkade_script_public_key(&emulator, script).unwrap();
            let error = build_tree_contract(
                &secp,
                operator,
                emulator,
                Sequence::from_height(144),
                Network::Regtest,
                tree,
                log,
                xp_balance,
                1_000,
                1_000,
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
