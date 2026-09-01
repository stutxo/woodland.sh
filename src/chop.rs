//! Chop and axe-craft construction plus the durable prepared-chop journal.
//!
//! `prepare_chop` and `prepare_craft` are the host-neutral builders shared by
//! the browser, e2e probes, and headless clients, so both covenant shapes live
//! in exactly one place.

use crate::arkade::VtxoRecord;
use crate::keys::Keys;
use crate::player::{self, PlayerContract, PlayerState};
use crate::tree::{TreeContract, TreeHealth};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::packet::{AssetGroup, AssetInput, AssetOutput, AssetRef, Packet};
use ark_core::asset::AssetId;
use ark_core::send::{
    build_offchain_transactions, sign_ark_transaction, sign_checkpoint_transaction, SendReceiver,
    VtxoInput,
};
#[cfg(any(target_arch = "wasm32", test))]
use base64::Engine;
#[cfg(any(target_arch = "wasm32", test))]
use bitcoin::Txid;
use bitcoin::{Amount, OutPoint, Psbt, ScriptBuf, Transaction, TxOut};
#[cfg(any(target_arch = "wasm32", test))]
use serde::{Deserialize, Serialize};

/// Durable journal for one prepared swing, keyed to the exact transactions.
/// Used by the browser host; headless callers persist their own recovery.
#[cfg(any(target_arch = "wasm32", test))]
const PENDING_CHOP_SCHEMA_VERSION: u32 = 2;

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingChop {
    schema_version: u32,
    pub(crate) tree_id: u32,
    pub success: bool,
    pub material: player::MaterialDrop,
    pub(crate) expected_txid: String,
    player_state_input: String,
    tree_input: String,
    ark_psbt: String,
    checkpoint_psbts: Vec<String>,
}

#[cfg(any(target_arch = "wasm32", test))]
impl PendingChop {
    pub(crate) fn new(
        tree_id: u32,
        success: bool,
        material: player::MaterialDrop,
        player_state_input: OutPoint,
        tree_input: OutPoint,
        ark_psbt: &Psbt,
        checkpoint_psbts: &[Psbt],
    ) -> Self {
        Self {
            schema_version: PENDING_CHOP_SCHEMA_VERSION,
            tree_id,
            success,
            material,
            expected_txid: ark_psbt.unsigned_tx.compute_txid().to_string(),
            player_state_input: player_state_input.to_string(),
            tree_input: tree_input.to_string(),
            ark_psbt: encode_psbt(ark_psbt),
            checkpoint_psbts: checkpoint_psbts.iter().map(encode_psbt).collect(),
        }
    }

    pub(crate) fn from_json(json: &str) -> Result<Self> {
        let pending: Self = serde_json::from_str(json).context("parse pending chop journal")?;
        if pending.schema_version != PENDING_CHOP_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported pending chop journal schema {}",
                pending.schema_version
            ));
        }
        pending.player_state_input.parse::<OutPoint>()?;
        pending.tree_input.parse::<OutPoint>()?;
        let expected_txid = pending.expected_txid.parse::<Txid>()?;
        let (ark_psbt, checkpoints) = pending.decode_psbts()?;
        if ark_psbt.unsigned_tx.compute_txid() != expected_txid
            || ark_psbt.unsigned_tx.input.len() != crate::protocol::CHOP_INPUT_COUNT
            || ark_psbt.unsigned_tx.output.len() != crate::protocol::CHOP_OUTPUT_COUNT
            || checkpoints.len() != crate::protocol::CHOP_INPUT_COUNT
        {
            return Err(anyhow!("pending chop journal transaction shape is invalid"));
        }
        Ok(pending)
    }

    pub(crate) fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialize pending chop journal")
    }

    pub(crate) fn txid(&self) -> Result<Txid> {
        self.expected_txid
            .parse()
            .context("parse pending chop transaction ID")
    }

    pub(crate) fn player_state_input(&self) -> Result<OutPoint> {
        self.player_state_input
            .parse()
            .context("parse pending player-state input")
    }

    pub(crate) fn tree_input(&self) -> Result<OutPoint> {
        self.tree_input.parse().context("parse pending tree input")
    }

    pub(crate) fn decode_psbts(&self) -> Result<(Psbt, Vec<Psbt>)> {
        let ark_psbt = decode_psbt(&self.ark_psbt).context("decode pending chop Ark PSBT")?;
        let checkpoints = self
            .checkpoint_psbts
            .iter()
            .map(|encoded| decode_psbt(encoded).context("decode pending chop checkpoint"))
            .collect::<Result<Vec<_>>>()?;
        Ok((ark_psbt, checkpoints))
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn encode_psbt(psbt: &Psbt) -> String {
    base64::engine::general_purpose::STANDARD.encode(psbt.serialize())
}

#[cfg(any(target_arch = "wasm32", test))]
fn decode_psbt(encoded: &str) -> Result<Psbt> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("decode PSBT base64")?;
    Psbt::deserialize(&bytes).context("decode PSBT bytes")
}

/// One fully constructed and owner-signed LOG withdrawal.
pub struct PreparedWithdraw {
    pub amount: u64,
    pub player_state_input: OutPoint,
    pub destination: TxOut,
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
}

/// Build owner-authorized LOG withdrawal. XP, materials, sats, PLAYER_ID, and
/// every player packet remain in the state; only LOG reaches the owner-chosen
/// destination funded by the wallet input.
#[allow(clippy::too_many_arguments)]
pub fn prepare_withdraw(
    keys: &Keys,
    info: &ark_core::server::Info,
    contract: &PlayerContract,
    player_asset: AssetId,
    record: &VtxoRecord,
    previous_tx: &Transaction,
    state: PlayerState,
    funding: &VtxoRecord,
    funding_previous_tx: &Transaction,
    funding_vtxo: &ark_core::Vtxo,
    amount: u64,
    destination: bitcoin::Address,
) -> Result<PreparedWithdraw> {
    record.validate_creating_transaction(previous_tx)?;
    crate::player::validate_player_state_record(record, contract, player_asset)?;
    if amount == 0 {
        return Err(anyhow!("withdraw amount must be positive"));
    }
    let logs = record.asset_amount(contract.log_asset).unwrap_or(0);
    if amount > logs {
        return Err(anyhow!("withdraw amount exceeds the player LOG balance"));
    }
    if !funding.assets.is_empty() {
        return Err(anyhow!("withdraw funding input must be asset-free"));
    }
    let now = crate::arkade::now_unix();
    record
        .ensure_live(now, crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS)
        .context("player state input")?;
    funding
        .ensure_live(now, crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS)
        .context("withdraw funding input")?;

    let control_block = contract
        .vtxo
        .get_spend_info(contract.withdraw_spend_script.clone())
        .map_err(|error| anyhow!("withdraw spend info: {error}"))?;
    let player_input = VtxoInput::new(
        contract.withdraw_spend_script.clone(),
        None,
        control_block,
        contract.vtxo.tapscripts(),
        contract.vtxo.script_pubkey(),
        Amount::from_sat(record.amount_sats),
        record.outpoint,
        record.assets.clone(),
    );
    let funding_input = crate::txbuild::vtxo_input(funding, funding_vtxo)?;
    let destination = TxOut {
        value: Amount::from_sat(funding.amount_sats),
        script_pubkey: destination.script_pubkey(),
    };
    let mut withdraw = build_offchain_transactions(
        &[
            SendReceiver::bitcoin(
                contract.vtxo.to_ark_address(),
                Amount::from_sat(contract.dust_sats),
            ),
            SendReceiver::bitcoin(
                funding_vtxo.to_ark_address(),
                Amount::from_sat(funding.amount_sats),
            ),
        ],
        &funding_vtxo.to_ark_address(),
        &[player_input, funding_input],
        info,
    )
    .map_err(|error| anyhow!("build withdraw transaction: {error}"))?;
    if withdraw.ark_tx.unsigned_tx.output.len() != crate::protocol::WITHDRAW_OUTPUT_COUNT - 1 {
        return Err(anyhow!(
            "withdraw builder produced an unexpected output count"
        ));
    }

    let logs_after = logs - amount;
    let mut log_outputs = Vec::new();
    if logs_after > 0 {
        log_outputs.push((crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX, logs_after));
    }
    log_outputs.push((crate::protocol::WITHDRAW_DESTINATION_OUTPUT_INDEX, amount));
    let mut groups = vec![
        transfer_group(
            player_asset,
            vec![(crate::protocol::WITHDRAW_STATE_INPUT_INDEX as u16, 1)],
            vec![(crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX, 1)],
        ),
        transfer_group(
            contract.log_asset,
            vec![(crate::protocol::WITHDRAW_STATE_INPUT_INDEX as u16, logs)],
            log_outputs,
        ),
    ];
    for asset_id in [
        contract.xp_asset,
        contract.stone_asset,
        contract.iron_ore_asset,
    ] {
        let balance = record.asset_amount(asset_id).unwrap_or(0);
        if balance > 0 {
            groups.push(transfer_group(
                asset_id,
                vec![(crate::protocol::WITHDRAW_STATE_INPUT_INDEX as u16, balance)],
                vec![(crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX, balance)],
            ));
        }
    }
    ark_core::asset::packet::add_asset_packet_to_psbt(&mut withdraw.ark_tx, &Packet { groups })
        .map_err(|error| anyhow!("attach withdraw asset packet: {error}"))?;

    let mut updated = withdraw.ark_tx.clone();
    crate::txbuild::attach_previous_ark_transactions(
        &mut updated,
        &withdraw.checkpoint_txs,
        [previous_tx, funding_previous_tx],
    )?;
    crate::player::attach_player_state_packets(&mut updated, state)?;
    let packet = ark_core::introspector::packet::Packet::new(vec![
        ark_core::introspector::packet::IntrospectorEntry {
            vin: crate::protocol::WITHDRAW_STATE_INPUT_INDEX as u16,
            script: contract.withdraw_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
    ])
    .context("build withdraw emulator packet")?;
    ark_core::introspector::packet::add_packet_to_psbt(&mut updated, &packet)
        .context("attach withdraw emulator packet")?;
    withdraw.ark_tx = updated;
    if withdraw.ark_tx.unsigned_tx.output.len() != crate::protocol::WITHDRAW_OUTPUT_COUNT {
        return Err(anyhow!("withdraw transaction has an invalid output count"));
    }
    for input_index in [
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
        crate::protocol::WITHDRAW_FUNDING_INPUT_INDEX,
    ] {
        sign_ark_transaction(
            |_, message| Ok(keys.sign_msg(&message)),
            &mut withdraw.ark_tx,
            input_index,
        )
        .map_err(|error| anyhow!("sign withdraw Ark input {input_index}: {error}"))?;
    }
    for input_index in [
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
        crate::protocol::WITHDRAW_FUNDING_INPUT_INDEX,
    ] {
        sign_checkpoint_transaction(
            |_, message| Ok(keys.sign_msg(&message)),
            &mut withdraw.checkpoint_txs[input_index],
        )
        .map_err(|error| anyhow!("sign withdraw checkpoint {input_index}: {error}"))?;
    }
    Ok(PreparedWithdraw {
        amount,
        player_state_input: record.outpoint,
        destination,
        ark_tx: withdraw.ark_tx,
        checkpoint_txs: withdraw.checkpoint_txs,
    })
}

/// One fully constructed and owner-signed axe upgrade.
pub struct PreparedCraft {
    pub recipe: player::AxeRecipe,
    pub player_state_input: OutPoint,
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
}

/// Build an owner-authorized one-tier axe upgrade. The covenant burns exactly
/// the selected recipe, preserves PLAYER_ID, XP, value, roll, and luck, and
/// recreates the recursive player state with the next axe packet.
pub fn prepare_craft(
    keys: &Keys,
    info: &ark_core::server::Info,
    contract: &PlayerContract,
    player_asset: AssetId,
    record: &VtxoRecord,
    previous_tx: &Transaction,
    state: PlayerState,
) -> Result<PreparedCraft> {
    record.validate_creating_transaction(previous_tx)?;
    crate::player::validate_player_state_record(record, contract, player_asset)?;
    if crate::player::player_state_from_tx(previous_tx)? != Some(state) {
        return Err(anyhow!(
            "indexed player state packets do not match the creating transaction"
        ));
    }
    record
        .ensure_live(
            crate::arkade::now_unix(),
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .context("player state input")?;
    let recipe = state
        .axe
        .next_recipe()
        .ok_or_else(|| anyhow!("the Iron Axe is already the highest tier"))?;
    let xp_balance = record.asset_amount(contract.xp_asset).unwrap_or(0);
    if xp_balance < recipe.required_xp_balance() {
        return Err(anyhow!(
            "{} requires Woodcutting level {}",
            recipe.axe.display_name(),
            recipe.required_level
        ));
    }
    for (asset_id, required, label) in [
        (contract.log_asset, recipe.log_cost, "LOG"),
        (contract.stone_asset, recipe.stone_cost, "STONE"),
        (contract.iron_ore_asset, recipe.iron_ore_cost, "IRON ORE"),
    ] {
        if record.asset_amount(asset_id).unwrap_or(0) < required {
            return Err(anyhow!(
                "{} requires {required} {label}",
                recipe.axe.display_name()
            ));
        }
    }

    let control_block = contract
        .vtxo
        .get_spend_info(contract.craft_spend_script.clone())
        .map_err(|error| anyhow!("craft spend info: {error}"))?;
    let input = VtxoInput::new(
        contract.craft_spend_script.clone(),
        None,
        control_block,
        contract.vtxo.tapscripts(),
        contract.vtxo.script_pubkey(),
        Amount::from_sat(record.amount_sats),
        record.outpoint,
        record.assets.clone(),
    );
    let mut craft = build_offchain_transactions(
        &[SendReceiver::bitcoin(
            contract.vtxo.to_ark_address(),
            Amount::from_sat(contract.dust_sats),
        )],
        &contract.vtxo.to_ark_address(),
        &[input],
        info,
    )
    .map_err(|error| anyhow!("build axe crafting transaction: {error}"))?;
    if craft.ark_tx.unsigned_tx.output.len() != crate::protocol::CRAFT_OUTPUT_COUNT - 1 {
        return Err(anyhow!(
            "axe crafting builder produced an unexpected output count"
        ));
    }

    let mut groups = vec![transfer_group(
        player_asset,
        vec![(crate::protocol::CRAFT_STATE_INPUT_INDEX as u16, 1)],
        vec![(crate::protocol::CRAFT_STATE_OUTPUT_INDEX, 1)],
    )];
    for (asset_id, cost) in [
        (contract.log_asset, recipe.log_cost),
        (contract.xp_asset, 0),
        (contract.stone_asset, recipe.stone_cost),
        (contract.iron_ore_asset, recipe.iron_ore_cost),
    ] {
        let before = record.asset_amount(asset_id).unwrap_or(0);
        if before == 0 {
            continue;
        }
        let after = before
            .checked_sub(cost)
            .ok_or_else(|| anyhow!("axe recipe exceeds the player inventory"))?;
        let outputs = if after == 0 {
            Vec::new()
        } else {
            vec![(crate::protocol::CRAFT_STATE_OUTPUT_INDEX, after)]
        };
        groups.push(transfer_group(
            asset_id,
            vec![(crate::protocol::CRAFT_STATE_INPUT_INDEX as u16, before)],
            outputs,
        ));
    }
    ark_core::asset::packet::add_asset_packet_to_psbt(&mut craft.ark_tx, &Packet { groups })
        .map_err(|error| anyhow!("attach axe crafting asset packet: {error}"))?;

    let next_state = PlayerState {
        luck: state.luck,
        axe: recipe.axe,
    };
    let mut updated = craft.ark_tx.clone();
    crate::txbuild::attach_previous_ark_transactions(
        &mut updated,
        &craft.checkpoint_txs,
        [previous_tx],
    )?;
    crate::player::attach_player_state_packets(&mut updated, next_state)?;
    let packet = ark_core::introspector::packet::Packet::new(vec![
        ark_core::introspector::packet::IntrospectorEntry {
            vin: crate::protocol::CRAFT_STATE_INPUT_INDEX as u16,
            script: contract.craft_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
    ])
    .context("build axe crafting emulator packet")?;
    ark_core::introspector::packet::add_packet_to_psbt(&mut updated, &packet)
        .context("attach axe crafting emulator packet")?;
    craft.ark_tx = updated;
    if craft.ark_tx.unsigned_tx.output.len() != crate::protocol::CRAFT_OUTPUT_COUNT {
        return Err(anyhow!(
            "axe crafting transaction has an invalid output count"
        ));
    }
    sign_ark_transaction(
        |_, message| Ok(keys.sign_msg(&message)),
        &mut craft.ark_tx,
        crate::protocol::CRAFT_STATE_INPUT_INDEX,
    )
    .map_err(|error| anyhow!("sign axe crafting Ark input: {error}"))?;
    sign_checkpoint_transaction(
        |_, message| Ok(keys.sign_msg(&message)),
        &mut craft.checkpoint_txs[crate::protocol::CRAFT_STATE_INPUT_INDEX],
    )
    .map_err(|error| anyhow!("sign axe crafting checkpoint: {error}"))?;
    Ok(PreparedCraft {
        recipe,
        player_state_input: record.outpoint,
        ark_tx: craft.ark_tx,
        checkpoint_txs: craft.checkpoint_txs,
    })
}

/// Expected outcome of a selected swing. The adversarial e2e build checks it
/// before construction so a changed world fails fast instead of signing a
/// stale transition.
#[cfg(target_arch = "wasm32")]
pub(crate) struct ExpectedChop {
    pub(crate) tree_outpoint: String,
    pub(crate) player_state_outpoint: String,
    pub(crate) drop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChopMutation {
    None,
    NonCanonicalHealth,
    SwapWorldGroups,
    SwapLogXpGroups,
    ReplaceXpGroup,
    WrongRoll,
    WrongLuckCredit,
    WrongLogDelta,
    WrongXpDelta,
    WrongMaterialDelta,
    DoubleTreeMarker,
    ExtraOutput,
    WrongAnchor,
    AssetMetadata,
    PlayerMarkerMetadata,
    AssetControl,
    FundExtension,
    SubmissionFailure,
}

impl ChopMutation {
    #[cfg(all(target_arch = "wasm32", feature = "regtest-e2e"))]
    pub(crate) fn parse(name: &str) -> Result<Self> {
        match name {
            "wrong-roll" => Ok(Self::WrongRoll),
            "wrong-luck-credit" => Ok(Self::WrongLuckCredit),
            "wrong-log-delta" => Ok(Self::WrongLogDelta),
            "wrong-xp-delta" => Ok(Self::WrongXpDelta),
            "wrong-material-delta" => Ok(Self::WrongMaterialDelta),
            "noncanonical-health-zero" => Ok(Self::NonCanonicalHealth),
            "double-tree-marker" => Ok(Self::DoubleTreeMarker),
            "extra-output" => Ok(Self::ExtraOutput),
            "wrong-anchor" => Ok(Self::WrongAnchor),
            "asset-metadata" => Ok(Self::AssetMetadata),
            "player-marker-metadata" => Ok(Self::PlayerMarkerMetadata),
            "asset-control" => Ok(Self::AssetControl),
            "fund-extension" => Ok(Self::FundExtension),
            _ => Err(anyhow!("unknown chop mutation {name}")),
        }
    }

    fn mutate_groups(self, groups: &mut [AssetGroup], tree_asset: AssetId) {
        match self {
            Self::PlayerMarkerMetadata => {
                groups[crate::protocol::PLAYER_ID_ASSET_GROUP_INDEX].metadata =
                    Some(vec![("forged".to_owned(), "metadata".to_owned())]);
            }
            Self::AssetMetadata => {
                groups[crate::protocol::LOG_ASSET_GROUP_INDEX].metadata =
                    Some(vec![("forged".to_owned(), "metadata".to_owned())]);
            }
            Self::AssetControl => {
                groups[crate::protocol::LOG_ASSET_GROUP_INDEX].control_asset =
                    Some(AssetRef::ById(tree_asset));
            }
            Self::SwapWorldGroups => groups.swap(
                crate::protocol::TREE_ASSET_GROUP_INDEX,
                crate::protocol::LOG_ASSET_GROUP_INDEX,
            ),
            Self::SwapLogXpGroups => groups.swap(
                crate::protocol::LOG_ASSET_GROUP_INDEX,
                crate::protocol::XP_ASSET_GROUP_INDEX,
            ),
            Self::ReplaceXpGroup => {
                groups[crate::protocol::XP_ASSET_GROUP_INDEX].asset_id = Some(AssetId {
                    txid: tree_asset.txid,
                    group_index: u16::MAX,
                });
            }
            Self::DoubleTreeMarker => {
                groups[crate::protocol::TREE_ASSET_GROUP_INDEX].outputs[0].amount = 2;
            }
            _ => {}
        }
    }

    fn mutate_extensions(self, psbt: &mut bitcoin::Psbt, next_state: PlayerState) -> Result<()> {
        match self {
            Self::NonCanonicalHealth => {
                let mut negative_zero = [0_u8; 9];
                negative_zero[8] = 0x80;
                replace_extension_packet(
                    psbt,
                    crate::protocol::TREE_HEALTH_PACKET_TYPE,
                    &negative_zero,
                )?;
            }
            Self::WrongRoll => replace_extension_packet(
                psbt,
                crate::protocol::PLAYER_ROLL_PACKET_TYPE,
                &next_state.luck.roll.next().encode(),
            )?,
            Self::WrongLuckCredit => {
                let value = next_state.luck.credit.value();
                let wrong = if value < crate::player::MAX_LUCK_CREDIT {
                    value + 1
                } else {
                    value - 1
                };
                replace_extension_packet(
                    psbt,
                    crate::protocol::PLAYER_LUCK_CREDIT_PACKET_TYPE,
                    &crate::player::PlayerLuckCredit::new(wrong)?.encode(),
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn mutate_outputs(self, psbt: &mut bitcoin::Psbt) -> Result<()> {
        match self {
            Self::ExtraOutput => {
                psbt.unsigned_tx.output.push(TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new(),
                });
                psbt.outputs.push(Default::default());
            }
            Self::WrongAnchor => {
                psbt.unsigned_tx.output[crate::protocol::CHOP_ANCHOR_OUTPUT_INDEX as usize]
                    .script_pubkey = ScriptBuf::new();
            }
            Self::FundExtension => {
                let state_index = crate::protocol::PLAYER_STATE_OUTPUT_INDEX as usize;
                let extension_index = crate::protocol::CHOP_EXTENSION_OUTPUT_INDEX as usize;
                let state_sats = psbt.unsigned_tx.output[state_index]
                    .value
                    .to_sat()
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("player state cannot fund extension mutation"))?;
                psbt.unsigned_tx.output[state_index].value = Amount::from_sat(state_sats);
                psbt.unsigned_tx.output[extension_index].value = Amount::from_sat(1);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Covenant-facing world constants a swing commits to.
pub struct ChopWorld<'a> {
    pub contract: &'a TreeContract,
    pub tree_asset: AssetId,
    pub log_asset: AssetId,
    pub xp_asset: AssetId,
    pub stone_asset: AssetId,
    pub iron_ore_asset: AssetId,
    pub dust_sats: u64,
    pub map_width: u16,
}

/// Live player lineage input for one swing.
pub struct PlayerChopState<'a> {
    pub record: &'a VtxoRecord,
    pub previous_tx: &'a Transaction,
    pub contract: &'a PlayerContract,
    pub state: PlayerState,
    pub player_asset: AssetId,
}

/// Live tree lineage input for one swing.
pub struct TreeChopState<'a> {
    pub record: &'a VtxoRecord,
    pub previous_tx: &'a Transaction,
    pub health: TreeHealth,
}

/// One fully constructed and player-signed swing.
pub struct PreparedChop {
    pub success: bool,
    pub material: player::MaterialDrop,
    pub player_state_input: OutPoint,
    pub tree_input: OutPoint,
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
}

/// Validate both lineage inputs and build the two-input / four-output swing:
/// player-bound reward accounting, asset group conservation, packet continuity,
/// and the player's covenant signatures. `mutation` exists only for the
/// adversarial e2e probes and is `None` in production play.
pub fn prepare_chop(
    keys: &Keys,
    info: &ark_core::server::Info,
    world: &ChopWorld,
    player_state: &PlayerChopState,
    tree: &TreeChopState,
    mutation: ChopMutation,
) -> Result<PreparedChop> {
    let player_logs_before = player_state
        .record
        .asset_amount(world.log_asset)
        .unwrap_or(0);
    let player_xp_balance_before = player_state
        .record
        .asset_amount(world.xp_asset)
        .unwrap_or(0);
    let player_stone_before = player_state
        .record
        .asset_amount(world.stone_asset)
        .unwrap_or(0);
    let player_iron_ore_before = player_state
        .record
        .asset_amount(world.iron_ore_asset)
        .unwrap_or(0);
    require_asset_amount(
        player_state.record,
        player_state.player_asset,
        1,
        "PLAYER_ID",
    )?;
    let tree_logs_before =
        require_nonzero_asset(tree.record, world.log_asset, "tree has no LOG reserve")?;
    let tree_xp_balance_before =
        require_nonzero_asset(tree.record, world.xp_asset, "tree has no XP")?;
    let tree_stone_before =
        require_nonzero_asset(tree.record, world.stone_asset, "tree has no STONE reserve")?;
    let tree_iron_ore_before = require_nonzero_asset(
        tree.record,
        world.iron_ore_asset,
        "tree has no IRON ORE reserve",
    )?;
    if tree.health.value() == 0 {
        return Err(anyhow!("cannot chop a stump"));
    }
    require_asset_amount(tree.record, world.tree_asset, 1, "tree marker")?;
    if tree.record.amount_sats != world.dust_sats {
        return Err(anyhow!("tree does not retain its fixed value"));
    }
    let now = crate::arkade::now_unix();
    for (record, label) in [(player_state.record, "player state"), (tree.record, "tree")] {
        record
            .ensure_live(now, crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS)
            .with_context(|| format!("{label} input"))?;
    }
    let (next_luck, success) = player_state
        .state
        .luck
        .advance(player_xp_balance_before, player_state.state.axe);
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
    let material = player::material_drop(next_luck.roll, player_xp_balance_before, success);
    let expected_stone_reward = u64::from(material == player::MaterialDrop::Stone);
    let stone_reward = if matches!(mutation, ChopMutation::WrongMaterialDelta) {
        1 - expected_stone_reward
    } else {
        expected_stone_reward
    };
    let iron_ore_reward = u64::from(material == player::MaterialDrop::IronOre);
    let tree_logs_after = tree_logs_before - log_reward;
    let player_logs_after = player_logs_before
        .checked_add(log_reward)
        .ok_or_else(|| anyhow!("player LOG balance overflow"))?;
    let tree_xp_balance_after = tree_xp_balance_before - xp_reward;
    let player_xp_balance_after = player_xp_balance_before
        .checked_add(xp_reward)
        .ok_or_else(|| anyhow!("player XP balance overflow"))?;
    let tree_stone_after = tree_stone_before - stone_reward;
    let player_stone_after = player_stone_before
        .checked_add(stone_reward)
        .ok_or_else(|| anyhow!("player STONE balance overflow"))?;
    let tree_iron_ore_after = tree_iron_ore_before - iron_ore_reward;
    let player_iron_ore_after = player_iron_ore_before
        .checked_add(iron_ore_reward)
        .ok_or_else(|| anyhow!("player IRON ORE balance overflow"))?;
    let next_state = PlayerState {
        luck: next_luck,
        axe: player_state.state.axe,
    };

    let inputs = [
        player::player_state_vtxo_input(
            player_state.record,
            player_state.contract,
            player_state.player_asset,
        )?,
        tree_vtxo_input(tree.record, world.contract)?,
    ];
    let mut chop = build_offchain_transactions(
        &[
            SendReceiver::bitcoin(
                player_state.contract.vtxo.to_ark_address(),
                Amount::from_sat(world.dust_sats),
            ),
            SendReceiver::bitcoin(
                world.contract.vtxo.to_ark_address(),
                Amount::from_sat(world.dust_sats),
            ),
        ],
        &player_state.contract.vtxo.to_ark_address(),
        &inputs,
        info,
    )
    .map_err(|error| anyhow!("build chop transaction: {error}"))?;
    if chop.ark_tx.unsigned_tx.output.len() != crate::protocol::CHOP_OUTPUT_COUNT_BEFORE_EXTENSION {
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
            player_state.player_asset,
            vec![(crate::protocol::PLAYER_STATE_INPUT_INDEX as u16, 1)],
            vec![(crate::protocol::PLAYER_STATE_OUTPUT_INDEX, 1)],
        ),
        transfer_group(
            world.tree_asset,
            vec![(crate::protocol::TREE_INPUT_INDEX as u16, 1)],
            vec![(crate::protocol::TREE_OUTPUT_INDEX, 1)],
        ),
        transfer_group(world.log_asset, log_inputs, log_outputs),
        transfer_group(world.xp_asset, xp_inputs, xp_outputs),
        player_tree_transfer_group(
            world.stone_asset,
            player_stone_before,
            tree_stone_before,
            player_stone_after,
            tree_stone_after,
        ),
        player_tree_transfer_group(
            world.iron_ore_asset,
            player_iron_ore_before,
            tree_iron_ore_before,
            player_iron_ore_after,
            tree_iron_ore_after,
        ),
    ];
    mutation.mutate_groups(&mut groups, world.tree_asset);
    ark_core::asset::packet::add_asset_packet_to_psbt(&mut chop.ark_tx, &Packet { groups })
        .map_err(|error| anyhow!("attach chop asset packet: {error}"))?;
    player::attach_player_chop_context(
        &mut chop.ark_tx,
        &chop.checkpoint_txs,
        player_state.contract,
        world.contract,
        [player_state.previous_tx, tree.previous_tx],
        player_xp_balance_before,
        next_state,
    )?;
    mutation.mutate_extensions(&mut chop.ark_tx, next_state)?;
    if chop.ark_tx.unsigned_tx.output.len() != crate::protocol::CHOP_OUTPUT_COUNT {
        return Err(anyhow!("chop transaction has an invalid output count"));
    }
    mutation.mutate_outputs(&mut chop.ark_tx)?;
    sign_ark_transaction(
        |_, message| Ok(keys.sign_msg(&message)),
        &mut chop.ark_tx,
        crate::protocol::PLAYER_STATE_INPUT_INDEX,
    )
    .map_err(|error| anyhow!("sign player Ark input: {error}"))?;
    sign_checkpoint_transaction(
        |_, message| Ok(keys.sign_msg(&message)),
        &mut chop.checkpoint_txs[crate::protocol::PLAYER_STATE_INPUT_INDEX],
    )
    .map_err(|error| anyhow!("sign player checkpoint: {error}"))?;
    Ok(PreparedChop {
        success,
        material,
        player_state_input: player_state.record.outpoint,
        tree_input: tree.record.outpoint,
        ark_tx: chop.ark_tx,
        checkpoint_txs: chop.checkpoint_txs,
    })
}

fn player_tree_transfer_group(
    asset_id: AssetId,
    player_before: u64,
    tree_before: u64,
    player_after: u64,
    tree_after: u64,
) -> AssetGroup {
    let mut inputs = Vec::with_capacity(2);
    if player_before > 0 {
        inputs.push((
            crate::protocol::PLAYER_STATE_INPUT_INDEX as u16,
            player_before,
        ));
    }
    inputs.push((crate::protocol::TREE_INPUT_INDEX as u16, tree_before));
    let mut outputs = Vec::with_capacity(2);
    if player_after > 0 {
        outputs.push((crate::protocol::PLAYER_STATE_OUTPUT_INDEX, player_after));
    }
    if tree_after > 0 {
        outputs.push((crate::protocol::TREE_OUTPUT_INDEX, tree_after));
    }
    transfer_group(asset_id, inputs, outputs)
}

pub fn transfer_group(
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

pub(crate) fn require_nonzero_asset(
    record: &VtxoRecord,
    asset_id: AssetId,
    message: &str,
) -> Result<u64> {
    record
        .asset_amount(asset_id)
        .filter(|amount| *amount > 0)
        .ok_or_else(|| anyhow!(message.to_string()))
}

pub(crate) fn require_asset_amount(
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::{absolute, transaction, Amount, ScriptBuf, Transaction, TxIn, TxOut};

    fn psbt(inputs: usize, outputs: usize) -> Psbt {
        Psbt::from_unsigned_tx(Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default(); inputs],
            output: vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new(),
                };
                outputs
            ],
        })
        .unwrap()
    }

    #[test]
    fn pending_chop_round_trips_exact_prepared_transaction() {
        let ark = psbt(
            crate::protocol::CHOP_INPUT_COUNT,
            crate::protocol::CHOP_OUTPUT_COUNT,
        );
        let checkpoints = (0..crate::protocol::CHOP_INPUT_COUNT)
            .map(|_| psbt(1, 1))
            .collect::<Vec<_>>();
        let pending = PendingChop::new(
            417,
            true,
            player::MaterialDrop::Stone,
            OutPoint::null(),
            OutPoint::null(),
            &ark,
            &checkpoints,
        );
        let decoded = PendingChop::from_json(&pending.to_json().unwrap()).unwrap();
        let (decoded_ark, decoded_checkpoints) = decoded.decode_psbts().unwrap();
        assert_eq!(decoded.txid().unwrap(), ark.unsigned_tx.compute_txid());
        assert_eq!(decoded.player_state_input().unwrap(), OutPoint::null());
        assert_eq!(decoded.tree_input().unwrap(), OutPoint::null());
        assert_eq!(decoded.material, player::MaterialDrop::Stone);
        assert_eq!(decoded_ark, ark);
        assert_eq!(decoded_checkpoints, checkpoints);
    }

    #[test]
    fn pending_chop_rejects_tampered_identity_or_shape() {
        let ark = psbt(
            crate::protocol::CHOP_INPUT_COUNT,
            crate::protocol::CHOP_OUTPUT_COUNT,
        );
        let checkpoints = (0..crate::protocol::CHOP_INPUT_COUNT)
            .map(|_| psbt(1, 1))
            .collect::<Vec<_>>();
        let pending = PendingChop::new(
            417,
            false,
            player::MaterialDrop::None,
            OutPoint::null(),
            OutPoint::null(),
            &ark,
            &checkpoints,
        );
        let mut json = serde_json::to_value(pending).unwrap();
        json["expectedTxid"] = serde_json::Value::String(Txid::all_zeros().to_string());
        assert!(PendingChop::from_json(&json.to_string()).is_err());
        json["expectedTxid"] =
            serde_json::Value::String(ark.unsigned_tx.compute_txid().to_string());
        json["schemaVersion"] = serde_json::Value::from(1);
        assert!(PendingChop::from_json(&json.to_string()).is_err());
        json["schemaVersion"] = serde_json::Value::from(PENDING_CHOP_SCHEMA_VERSION);
        json["checkpointPsbts"].as_array_mut().unwrap().pop();
        assert!(PendingChop::from_json(&json.to_string()).is_err());
    }
}
