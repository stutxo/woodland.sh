//! Batch-renewal intent construction for Forest state VTXOs.
//!
//! A fresh VTXO lifetime comes from a new Ark batch. Each renewal spends one
//! state VTXO through its dedicated renewal leaf in a version-2 intent proof
//! whose sole purpose is a covenant-constrained self-send. Identity, P2TR,
//! value, and assets are preserved; a funded tree stump's health alone resets
//! to full. The emulator executes the covenant and counters the proof; the
//! batch then recreates the VTXO with a new expiry.
//!
//! Shared tree renewal is permissionless and player state is gated by its
//! owner, so only a client that constructed the exact preservation proof can
//! register it. The covenant constrains that signature to the self-send, never
//! to a transfer.

use crate::arkade::{now_unix, VtxoRecord};
use crate::keys::Keys;
use crate::player::{PlayerContract, PlayerState};
use crate::protocol::{
    PLAYER_AXE_PACKET_TYPE, PLAYER_LUCK_CREDIT_PACKET_TYPE, PLAYER_ROLL_PACKET_TYPE,
    RENEWAL_FEE_INPUT_INDEX, RENEWAL_INPUT_COUNT, RENEWAL_STATE_INPUT_INDEX,
    RENEWAL_STATE_OUTPUT_INDEX, TREE_HEALTH_PACKET_TYPE, TREE_STATE_PACKET_TYPE,
};
use crate::tree::{TreeContract, TreeState};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::packet::{AssetGroup, AssetInput, AssetOutput, Packet as AssetPacket};
use ark_core::asset::AssetId;
use ark_core::intent::{self, Intent, IntentMessage};
use ark_core::introspector::packet::{IntrospectorEntry, Packet as EmulatorPacket};
use ark_core::Asset;
use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Amount, Psbt, ScriptBuf, Sequence, Transaction, TxOut};

/// Registered intents are short-lived; the renewal must reach a batch quickly.
const INTENT_MESSAGE_TTL_SECS: u64 = 900;
const INTENT_DELETE_TTL_SECS: u64 = 120;

/// One validated state VTXO prepared for batch renewal.
pub struct RenewalIntent {
    input: intent::Input,
    funding: Option<RenewalFunding>,
    outputs: Vec<intent::Output>,
    groups: Vec<AssetGroup>,
    state_packets: Vec<(u8, Vec<u8>)>,
    input_created_at: i64,
    input_expires_at: i64,
    /// The renewal Arkade script; its tweaked emulator key must countersign.
    pub arkade_script: ScriptBuf,
}

struct RenewalFunding {
    input: intent::Input,
    previous_tx: Transaction,
}

/// A rollover proof signed by the dedicated service key and bound to its exact
/// state, previous transaction, message, and ephemeral batch cosigner.
pub struct PreparedRenewal {
    pub intent: Intent,
    pub message: IntentMessage,
    /// Exact JSON committed by the proof's fake message input.
    pub message_json: String,
    pub input: intent::Input,
    pub funding_input: Option<intent::Input>,
    pub input_expires_at: i64,
    /// Exact non-anchor outputs arkd must place in this intent's batch leaf.
    pub leaf_outputs: Vec<TxOut>,
    pub arkade_script: ScriptBuf,
}

/// An emulator-countersigned renewal ready to join a batch.
pub struct ApprovedRenewal {
    pub proof: Psbt,
    pub message: IntentMessage,
    pub message_json: String,
    pub input: intent::Input,
    pub funding_input: Option<intent::Input>,
    pub input_expires_at: i64,
    /// Exact non-anchor outputs arkd must place in this intent's batch leaf.
    pub leaf_outputs: Vec<TxOut>,
    pub arkade_script: ScriptBuf,
}

/// Submit a prepared rollover to the emulator and verify that it added only its
/// script-tweaked signature without altering the exact proof.
pub async fn approve(
    verifier: &Keys,
    emulator: &crate::arkade::EmulatorRest,
    emulator_pk: bitcoin::XOnlyPublicKey,
    prepared: PreparedRenewal,
) -> Result<ApprovedRenewal> {
    use base64::Engine;
    let expected_proof = prepared.intent.proof.clone();
    let approved = emulator
        .submit_intent(
            &base64::engine::general_purpose::STANDARD.encode(expected_proof.serialize()),
            &prepared.message_json,
        )
        .await?;
    let approved = combine_and_verify_emulator_approval(
        verifier,
        &expected_proof,
        approved,
        emulator_pk,
        &prepared.arkade_script,
    )?;
    Ok(ApprovedRenewal {
        proof: approved,
        message: prepared.message,
        message_json: prepared.message_json,
        input: prepared.input,
        input_expires_at: prepared.input_expires_at,
        funding_input: prepared.funding_input,
        leaf_outputs: prepared.leaf_outputs,
        arkade_script: prepared.arkade_script,
    })
}

/// Read and validate one indexed tree VTXO and build its renewal intent.
/// `previous_tx` is the Ark transaction that created the VTXO; it carries the
/// tree-state packet the renewal must preserve.
#[allow(clippy::too_many_arguments)]
pub fn prepare_tree(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &TreeContract,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    stone_asset: AssetId,
    iron_ore_asset: AssetId,
    dust_sats: u64,
    expiry_margin_secs: i64,
) -> Result<RenewalIntent> {
    record.validate_creating_transaction(previous_tx)?;
    let state = crate::tree::tree_state_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous tree transaction has no state packet"))?;
    let raw_health = crate::tree::tree_health_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous tree transaction has no health packet"))?;
    let assets = record_assets(record)?;
    let tree_count = asset_amount(&assets, tree_asset);
    let log_count = asset_amount(&assets, log_asset);
    let xp_count = asset_amount(&assets, xp_asset);
    let stone_count = asset_amount(&assets, stone_asset);
    let iron_ore_count = asset_amount(&assets, iron_ore_asset);
    let other_assets = assets.iter().any(|asset| {
        ![tree_asset, log_asset, xp_asset, stone_asset, iron_ore_asset].contains(&asset.asset_id)
    });
    let funded_stump = raw_health.value() == 0 && log_count > 0;
    let (health, spend_script, arkade_script) = if funded_stump {
        (
            crate::tree::TreeHealth::new(crate::tree::LOGS_PER_TREE)?,
            &contract.regrowth_spend_script,
            contract.regrowth_arkade_script.clone(),
        )
    } else {
        (
            raw_health,
            &contract.maintenance_spend_script,
            contract.maintenance_arkade_script.clone(),
        )
    };
    if tree_count != 1
        || other_assets
        || log_count != xp_count
        || raw_health.value() > log_count
        || record.amount_sats != crate::tree::tree_value_sats(dust_sats)
    {
        return Err(anyhow!("indexed tree VTXO holdings are not canonical"));
    }

    let mut groups = vec![renewal_group(tree_asset, 1)];
    if log_count > 0 {
        groups.push(renewal_group(log_asset, log_count));
    }
    if xp_count > 0 {
        groups.push(renewal_group(xp_asset, xp_count));
    }
    if stone_count > 0 {
        groups.push(renewal_group(stone_asset, stone_count));
    }
    if iron_ore_count > 0 {
        groups.push(renewal_group(iron_ore_asset, iron_ore_count));
    }
    build(
        record,
        &contract.vtxo,
        spend_script,
        arkade_script,
        assets,
        groups,
        vec![
            (TREE_STATE_PACKET_TYPE, state.encode().to_vec()),
            (TREE_HEALTH_PACKET_TYPE, health.encode().to_vec()),
        ],
        expiry_margin_secs,
    )
}

/// Read and validate one player-state VTXO for owner-authorized renewal.
pub fn prepare_player(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &PlayerContract,
    player_asset: AssetId,
    expiry_margin_secs: i64,
) -> Result<RenewalIntent> {
    prepare_player_for_path(
        record,
        previous_tx,
        contract,
        player_asset,
        false,
        expiry_margin_secs,
    )
}

/// Prepare the same exact self-send through the optional watchtower leaf.
pub fn prepare_player_watchtower(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &PlayerContract,
    player_asset: AssetId,
    expiry_margin_secs: i64,
) -> Result<RenewalIntent> {
    prepare_player_for_path(
        record,
        previous_tx,
        contract,
        player_asset,
        true,
        expiry_margin_secs,
    )
}

fn prepare_player_for_path(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &PlayerContract,
    player_asset: AssetId,
    watchtower: bool,
    expiry_margin_secs: i64,
) -> Result<RenewalIntent> {
    record.validate_creating_transaction(previous_tx)?;
    let state = crate::player::player_state_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous player transaction has no state packets"))?;
    crate::player::validate_player_state_record(record, contract, player_asset)?;
    let assets = record_assets(record)?;
    // Mirror the tree guard: fail loudly here rather than build an intent the
    // emulator always rejects, which would silently strand the VTXO.
    let other_assets = assets.iter().any(|asset| {
        ![
            player_asset,
            contract.log_asset,
            contract.xp_asset,
            contract.stone_asset,
            contract.iron_ore_asset,
        ]
        .contains(&asset.asset_id)
    });
    if other_assets {
        return Err(anyhow!("indexed player VTXO holdings are not canonical"));
    }
    let mut groups = vec![renewal_group(player_asset, 1)];
    for asset_id in [
        contract.log_asset,
        contract.xp_asset,
        contract.stone_asset,
        contract.iron_ore_asset,
    ] {
        let count = asset_amount(&assets, asset_id);
        if count > 0 {
            groups.push(renewal_group(asset_id, count));
        }
    }
    let (spend_script, arkade_script) = if watchtower {
        (
            &contract.watchtower_renewal_spend_script,
            contract.renewal_arkade_script.clone(),
        )
    } else {
        (
            &contract.renewal_spend_script,
            contract.renewal_arkade_script.clone(),
        )
    };
    build(
        record,
        &contract.vtxo,
        spend_script,
        arkade_script,
        assets,
        groups,
        vec![
            (PLAYER_ROLL_PACKET_TYPE, state.luck.roll.encode().to_vec()),
            (
                PLAYER_LUCK_CREDIT_PACKET_TYPE,
                state.luck.credit.encode().to_vec(),
            ),
            (PLAYER_AXE_PACKET_TYPE, state.axe.encode().to_vec()),
        ],
        expiry_margin_secs,
    )
}

/// Decode the player state carried by the transaction that created a player
/// VTXO, so the renewal can preserve it exactly.
pub fn player_state_from_tx(tx: &Transaction) -> Result<Option<PlayerState>> {
    crate::player::player_state_from_tx(tx)
}

/// Decode the tree state carried by the transaction that created a tree VTXO.
pub fn tree_state_from_tx(tx: &Transaction) -> Result<Option<TreeState>> {
    crate::tree::tree_state_from_tx(tx)
}

#[allow(clippy::too_many_arguments)]
fn build(
    record: &VtxoRecord,
    vtxo: &ark_core::Vtxo,
    spend_script: &ScriptBuf,
    arkade_script: ScriptBuf,
    assets: Vec<Asset>,
    groups: Vec<AssetGroup>,
    state_packets: Vec<(u8, Vec<u8>)>,
    expiry_margin_secs: i64,
) -> Result<RenewalIntent> {
    record.ensure_live(now_unix(), expiry_margin_secs)?;
    let input_created_at = record
        .created_at
        .ok_or_else(|| anyhow!("renewal input has no indexed creation time"))?;
    let input_expires_at = record
        .expires_at
        .ok_or_else(|| anyhow!("renewal input has no indexed expiry"))?;
    let control_block = vtxo
        .get_spend_info(spend_script.clone())
        .map_err(|error| anyhow!("renewal spend info: {error}"))?;
    let input = intent::Input::new(
        record.outpoint,
        Sequence::MAX,
        None,
        TxOut {
            value: Amount::from_sat(record.amount_sats),
            script_pubkey: vtxo.script_pubkey(),
        },
        vtxo.tapscripts(),
        (spend_script.clone(), control_block),
        false,
        false,
        assets,
    );

    let extension = renewal_extension_txout(groups.clone(), state_packets.clone(), &arkade_script)?;
    let outputs = vec![
        intent::Output::Offchain(TxOut {
            value: Amount::from_sat(record.amount_sats),
            script_pubkey: vtxo.script_pubkey(),
        }),
        intent::Output::AssetPacket(extension),
    ];

    Ok(RenewalIntent {
        input,
        funding: None,
        outputs,
        groups,
        state_packets,
        input_created_at,
        input_expires_at,
        arkade_script,
    })
}
impl RenewalIntent {
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) fn state_outpoint(&self) -> bitcoin::OutPoint {
        self.input.outpoint()
    }

    pub(crate) fn estimate_base_fee(&self, estimators: &[ark_fees::Estimator]) -> Result<u64> {
        let input = [ark_fees::OffchainInput {
            amount: self.input.amount().to_sat(),
            expiry: Some(self.input_expires_at),
            birth: Some(self.input_created_at),
            input_type: ark_fees::VtxoType::Vtxo,
            weight: 0.0,
        }];
        let outputs = self
            .outputs
            .iter()
            .map(output_txout)
            .map(|output| output.map(fee_output))
            .collect::<Result<Vec<_>>>()?;
        estimators
            .iter()
            .map(|estimator| {
                estimator
                    .eval(&input, &[], &outputs, &[])
                    .map(|fee| fee.to_satoshis())
                    .map_err(|error| anyhow!("evaluate renewal intent fee: {error}"))
            })
            .collect::<Result<Vec<_>>>()
            .map(|fees| fees.into_iter().max().unwrap_or(0))
    }

    pub(crate) fn estimate_sponsored_fee(
        &self,
        funding_record: &VtxoRecord,
        funding_previous_tx: &Transaction,
        funding_vtxo: &ark_core::Vtxo,
        estimators: &[ark_fees::Estimator],
        minimum_change_sats: u64,
        expiry_margin_secs: i64,
    ) -> Result<u64> {
        validate_fee_funding(
            funding_record,
            funding_previous_tx,
            funding_vtxo,
            expiry_margin_secs,
        )?;
        if funding_record.outpoint == self.input.outpoint() {
            return Err(anyhow!("renewal fee input duplicates the state input"));
        }

        let state_output = output_txout(
            self.outputs
                .first()
                .ok_or_else(|| anyhow!("renewal intent has no state output"))?,
        )?;
        let extension_output = output_txout(
            self.outputs
                .last()
                .ok_or_else(|| anyhow!("renewal intent has no extension output"))?,
        )?;
        let fee_inputs = [
            ark_fees::OffchainInput {
                amount: self.input.amount().to_sat(),
                expiry: Some(self.input_expires_at),
                birth: Some(self.input_created_at),
                input_type: ark_fees::VtxoType::Vtxo,
                weight: 0.0,
            },
            ark_fees::OffchainInput {
                amount: funding_record.amount_sats,
                expiry: funding_record.expires_at,
                birth: funding_record.created_at,
                input_type: ark_fees::VtxoType::Vtxo,
                weight: 0.0,
            },
        ];
        let mut paid_fee = 0_u64;
        for _ in 0..64 {
            let change_sats = funding_record
                .amount_sats
                .checked_sub(paid_fee)
                .filter(|change| *change >= minimum_change_sats)
                .ok_or_else(|| anyhow!("renewal fee leaves sub-minimum wallet change"))?;
            let outputs = [
                fee_output(state_output),
                ark_fees::Output {
                    amount: change_sats,
                    script: script_hex(&funding_record.script),
                },
                fee_output(extension_output),
            ];
            let required_fee = estimators
                .iter()
                .map(|estimator| {
                    estimator
                        .eval(&fee_inputs, &[], &outputs, &[])
                        .map(|fee| fee.to_satoshis())
                        .map_err(|error| anyhow!("evaluate renewal intent fee: {error}"))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .max()
                .unwrap_or(0);
            if required_fee <= paid_fee {
                return Ok(paid_fee);
            }
            paid_fee = required_fee;
        }
        Err(anyhow!("renewal fee program did not converge"))
    }

    pub(crate) fn add_fee_funding(
        mut self,
        funding_record: &VtxoRecord,
        funding_previous_tx: &Transaction,
        funding_vtxo: &ark_core::Vtxo,
        fee_sats: u64,
        minimum_change_sats: u64,
        expiry_margin_secs: i64,
    ) -> Result<Self> {
        validate_fee_funding(
            funding_record,
            funding_previous_tx,
            funding_vtxo,
            expiry_margin_secs,
        )?;
        if funding_record.outpoint == self.input.outpoint() {
            return Err(anyhow!("renewal fee input duplicates the state input"));
        }
        let change_sats = funding_record
            .amount_sats
            .checked_sub(fee_sats)
            .filter(|change| *change >= minimum_change_sats)
            .ok_or_else(|| anyhow!("renewal fee leaves sub-minimum wallet change"))?;
        let (spend_script, control_block) = funding_vtxo
            .forfeit_spend_info()
            .map_err(|error| anyhow!("fee input spend info: {error}"))?;
        let input = intent::Input::new(
            funding_record.outpoint,
            Sequence::MAX,
            None,
            TxOut {
                value: Amount::from_sat(funding_record.amount_sats),
                script_pubkey: funding_vtxo.script_pubkey(),
            },
            funding_vtxo.tapscripts(),
            (spend_script, control_block),
            false,
            false,
            Vec::new(),
        );
        self.outputs.insert(
            1,
            intent::Output::Offchain(TxOut {
                value: Amount::from_sat(change_sats),
                script_pubkey: funding_vtxo.script_pubkey(),
            }),
        );
        self.input_expires_at = self.input_expires_at.min(
            funding_record
                .expires_at
                .expect("fee funding liveness validated the expiry"),
        );
        self.funding = Some(RenewalFunding {
            input,
            previous_tx: funding_previous_tx.clone(),
        });
        Ok(self)
    }
}

fn validate_fee_funding(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    vtxo: &ark_core::Vtxo,
    expiry_margin_secs: i64,
) -> Result<()> {
    record.validate_creating_transaction(previous_tx)?;
    record.ensure_live(now_unix(), expiry_margin_secs)?;
    if !record.assets.is_empty() || record.amount_sats == 0 || record.script != vtxo.script_pubkey()
    {
        return Err(anyhow!(
            "renewal fee input must be an asset-free wallet VTXO"
        ));
    }
    Ok(())
}

fn output_txout(output: &intent::Output) -> Result<&TxOut> {
    match output {
        intent::Output::Offchain(output) | intent::Output::AssetPacket(output) => Ok(output),
        intent::Output::Onchain(_) => Err(anyhow!("renewal intent contains an onchain output")),
    }
}

fn fee_output(output: &TxOut) -> ark_fees::Output {
    ark_fees::Output {
        amount: output.value.to_sat(),
        script: script_hex(&output.script_pubkey),
    }
}

fn script_hex(script: &ScriptBuf) -> String {
    use bitcoin::hex::DisplayHex;
    script.as_bytes().to_lower_hex_string()
}

/// Bind the exact self-send to a short-lived registration message, sign it
/// with whichever key appears in the selected renewal leaf, and attach the
/// creating transaction required for input introspection.
/// A short-lived ownership proof that authorizes arkd to remove every queued
/// intent overlapping this renewal input. Deletion changes only arkd's pending
/// intent set; it never spends the covenant VTXO.
pub(crate) struct IntentDeletion {
    pub message: IntentMessage,
    pub proof: Psbt,
}

fn sign_intent_input(
    authorizer_keys: &Keys,
    psbt_input: &mut bitcoin::psbt::Input,
    message: bitcoin::secp256k1::Message,
) -> std::result::Result<
    Vec<(
        bitcoin::secp256k1::schnorr::Signature,
        bitcoin::XOnlyPublicKey,
    )>,
    ark_core::Error,
> {
    let script = psbt_input.witness_script.clone().ok_or_else(|| {
        ark_core::Error::ad_hoc("missing witness script when signing renewal intent")
    })?;
    if ark_core::script::extract_checksig_pubkeys(&script).contains(&authorizer_keys.owner_pk()) {
        Ok(authorizer_keys.sign_msg(&message))
    } else {
        Ok(Vec::new())
    }
}

fn reject_onchain_intent_input(
    _: &mut bitcoin::psbt::Input,
    _: bitcoin::secp256k1::Message,
) -> std::result::Result<
    (
        bitcoin::secp256k1::schnorr::Signature,
        bitcoin::XOnlyPublicKey,
    ),
    ark_core::Error,
> {
    Err(ark_core::Error::ad_hoc(
        "renewal intent proofs have no onchain inputs",
    ))
}

/// Build the proof accepted by arkd's overlap-based `deleteIntent` endpoint.
/// A fresh proof is sufficient even after a process restart: arkd matches the
/// proven outpoint, not the old intent id or registration proof.
pub(crate) fn prepare_intent_deletion(
    authorizer_keys: &Keys,
    input: &intent::Input,
) -> Result<IntentDeletion> {
    let now = u64::try_from(now_unix()).context("system clock is before the Unix epoch")?;
    let expire_at = now
        .checked_add(INTENT_DELETE_TTL_SECS)
        .ok_or_else(|| anyhow!("intent deletion expiry overflow"))?;
    let message = IntentMessage::Delete { expire_at };
    let intent = intent::make_intent(
        |psbt_input, sighash| sign_intent_input(authorizer_keys, psbt_input, sighash),
        reject_onchain_intent_input,
        vec![input.clone()],
        Vec::new(),
        message.clone(),
    )
    .map_err(|error| anyhow!("build renewal intent deletion proof: {error}"))?;
    Ok(IntentDeletion {
        message,
        proof: intent.proof,
    })
}

pub fn bind(
    authorizer_keys: &Keys,
    prepared: RenewalIntent,
    previous_tx: &Transaction,
    cosigner_pk: PublicKey,
) -> Result<PreparedRenewal> {
    let RenewalIntent {
        input,
        funding,
        outputs,
        groups,
        state_packets,
        input_created_at: _,
        input_expires_at,
        arkade_script,
    } = prepared;
    validate_previous_input(&input, previous_tx, "renewal state")?;
    let funding_input = funding.as_ref().map(|funding| funding.input.clone());
    if let Some(funding) = &funding {
        validate_previous_input(&funding.input, &funding.previous_tx, "renewal fee")?;
    }
    let continuation_outputs = outputs
        .iter()
        .take(
            outputs
                .len()
                .checked_sub(1)
                .ok_or_else(|| anyhow!("renewal intent has no extension output"))?,
        )
        .map(output_txout)
        .map(|output| output.cloned())
        .collect::<Result<Vec<_>>>()?;
    let mut real_inputs = vec![input.clone()];
    if let Some(funding_input) = &funding_input {
        real_inputs.push(funding_input.clone());
    }

    let now = now_unix() as u64;
    let message = IntentMessage::Register {
        onchain_output_indexes: Vec::new(),
        valid_at: now,
        expire_at: now + INTENT_MESSAGE_TTL_SECS,
        own_cosigner_pks: vec![cosigner_pk],
    };

    let sign_for_vtxo = |psbt_input: &mut bitcoin::psbt::Input,
                         message: bitcoin::secp256k1::Message| {
        sign_intent_input(authorizer_keys, psbt_input, message)
    };
    let sign_for_onchain = reject_onchain_intent_input;

    let message_json = message
        .encode()
        .map_err(|error| anyhow!("encode renewal intent message: {error}"))?;
    let mut intent = intent::make_intent(
        sign_for_vtxo,
        sign_for_onchain,
        real_inputs,
        outputs,
        message.clone(),
    )
    .map_err(|error| anyhow!("build renewal intent: {error}"))?;

    attach_previous_transaction(
        &mut intent,
        previous_tx,
        RENEWAL_STATE_INPUT_INDEX,
        "renewal state",
    )?;
    if let Some(funding) = &funding {
        attach_previous_transaction(
            &mut intent,
            &funding.previous_tx,
            RENEWAL_FEE_INPUT_INDEX,
            "renewal fee",
        )?;
    }

    let mut leaf_outputs = continuation_outputs;
    leaf_outputs.push(batch_leaf_extension_txout(
        &groups,
        &state_packets,
        &arkade_script,
        intent.proof.unsigned_tx.compute_txid(),
    )?);

    Ok(PreparedRenewal {
        intent,
        message,
        message_json,
        input,
        funding_input,
        input_expires_at,
        leaf_outputs,
        arkade_script,
    })
}

fn validate_previous_input(
    input: &intent::Input,
    previous_tx: &Transaction,
    label: &str,
) -> Result<()> {
    if previous_tx.compute_txid() != input.outpoint().txid {
        return Err(anyhow!(
            "{label} creating transaction {} does not match {}",
            previous_tx.compute_txid(),
            input.outpoint()
        ));
    }
    let previous_output = previous_tx
        .output
        .get(input.outpoint().vout as usize)
        .ok_or_else(|| anyhow!("{label} creating transaction omits the input"))?;
    if previous_output.script_pubkey != *input.script_pubkey()
        || previous_output.value != input.amount()
    {
        return Err(anyhow!(
            "{label} creating transaction output does not match the indexed input"
        ));
    }
    Ok(())
}

fn attach_previous_transaction(
    intent: &mut Intent,
    previous_tx: &Transaction,
    input_index: usize,
    label: &str,
) -> Result<()> {
    // The fake message input has no previous Ark transaction. Every real VTXO
    // input must carry its exact creating transaction for emulator
    // introspection.
    let key = bitcoin::psbt::raw::Key {
        type_value: 0xde,
        key: b"prevarktx".to_vec(),
    };
    let proof_input = intent
        .proof
        .inputs
        .get_mut(input_index)
        .ok_or_else(|| anyhow!("renewal proof is missing its {label} input"))?;
    if proof_input.unknown.contains_key(&key) {
        return Err(anyhow!(
            "previous Ark transaction field already exists on input {input_index}"
        ));
    }
    proof_input
        .unknown
        .insert(key, bitcoin::consensus::encode::serialize(previous_tx));
    Ok(())
}

/// Merge the emulator response into the exact submitted PSBT, then verify its
/// script-tweaked signatures on the fake and state inputs. An optional clean
/// fee input is signed only by its wallet owner.
pub fn combine_and_verify_emulator_approval(
    verifier: &Keys,
    expected: &Psbt,
    approved: Psbt,
    emulator_pk: bitcoin::XOnlyPublicKey,
    arkade_script: &ScriptBuf,
) -> Result<Psbt> {
    let expected_input_count = expected.inputs.len();
    if expected.unsigned_tx != approved.unsigned_tx
        || expected_input_count != approved.inputs.len()
        || ![RENEWAL_INPUT_COUNT, RENEWAL_FEE_INPUT_INDEX + 1].contains(&expected_input_count)
    {
        return Err(anyhow!("emulator changed the renewal intent proof"));
    }
    let mut combined = expected.clone();
    combined
        .combine(approved)
        .map_err(|error| anyhow!("combine emulator renewal approval: {error}"))?;
    let tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, arkade_script)
            .context("derive renewal emulator signer")?;
    let authorizer_pk = verifier.owner_pk();
    // The authorizer signs state inputs only when its key is a renewal-leaf
    // signer. Permissionless regrowth needs only the emulator plus arkd.
    let state_authorizer_expected = expected.inputs[RENEWAL_STATE_INPUT_INDEX]
        .witness_script
        .as_ref()
        .map(ark_core::script::extract_checksig_pubkeys)
        .is_some_and(|signers| signers.contains(&authorizer_pk));
    for input_index in 0..RENEWAL_INPUT_COUNT {
        if state_authorizer_expected {
            crate::txbuild::verified_signature_for_key_with_sighash(
                verifier,
                expected,
                &combined,
                input_index,
                authorizer_pk,
                "renewal authorizer",
                bitcoin::TapSighashType::Default,
            )?;
        }
        crate::txbuild::verified_signature_for_key_with_sighash(
            verifier,
            expected,
            &combined,
            input_index,
            tweaked_emulator,
            "emulator",
            bitcoin::TapSighashType::All,
        )?;
        let expected_sigs = 1 + usize::from(state_authorizer_expected);
        if combined.inputs[input_index].tap_script_sigs.len() != expected_sigs {
            return Err(anyhow!(
                "renewal proof contains an unexpected state signature"
            ));
        }
    }
    if expected_input_count > RENEWAL_INPUT_COUNT {
        let funding_signers = expected.inputs[RENEWAL_FEE_INPUT_INDEX]
            .witness_script
            .as_ref()
            .map(ark_core::script::extract_checksig_pubkeys)
            .ok_or_else(|| anyhow!("renewal fee input has no witness script"))?;
        if !funding_signers.contains(&authorizer_pk) {
            return Err(anyhow!("renewal authorizer does not own the fee input"));
        }
        crate::txbuild::verified_signature_for_key_with_sighash(
            verifier,
            expected,
            &combined,
            RENEWAL_FEE_INPUT_INDEX,
            authorizer_pk,
            "fee input owner",
            bitcoin::TapSighashType::Default,
        )?;
        if combined.inputs[RENEWAL_FEE_INPUT_INDEX]
            .tap_script_sigs
            .len()
            != 1
        {
            return Err(anyhow!(
                "renewal fee input contains an unexpected signature"
            ));
        }
    }
    Ok(combined)
}

fn renewal_group(asset: AssetId, amount: u64) -> AssetGroup {
    AssetGroup {
        asset_id: Some(asset),
        control_asset: None,
        metadata: None,
        inputs: vec![AssetInput {
            input_index: RENEWAL_STATE_INPUT_INDEX as u16,
            amount,
        }],
        outputs: vec![AssetOutput {
            output_index: RENEWAL_STATE_OUTPUT_INDEX,
            amount,
        }],
    }
}

fn renewal_extension_txout(
    groups: Vec<AssetGroup>,
    state_packets: Vec<(u8, Vec<u8>)>,
    arkade_script: &ScriptBuf,
) -> Result<TxOut> {
    let mut packets: Vec<(u8, Vec<u8>)> = Vec::new();
    if !groups.is_empty() {
        packets.push((0, AssetPacket { groups }.encode()));
    }
    packets.extend(state_packets);
    let emulator_packet = EmulatorPacket::new(vec![IntrospectorEntry {
        vin: RENEWAL_STATE_INPUT_INDEX as u16,
        script: arkade_script.clone(),
        witness: bitcoin::Witness::default(),
    }])
    .context("build renewal emulator packet")?;
    packets.push((
        1,
        emulator_packet.encode().context("encode emulator packet")?,
    ));

    let mut payload = ark_core::extension::MAGIC_BYTES.to_vec();
    for (packet_type, packet_payload) in packets {
        payload.push(packet_type);
        ark_core::extension::encode_uvarint(&mut payload, packet_payload.len() as u64);
        payload.extend_from_slice(&packet_payload);
    }
    Ok(TxOut {
        value: Amount::ZERO,
        script_pubkey: op_return_script(&payload),
    })
}

/// Reproduce arkd's `LeafTxPacket` conversion for Forest's deliberately
/// narrow renewal asset groups. Local proof inputs become intent references;
/// state and emulator packets are copied byte-for-byte.
fn batch_leaf_extension_txout(
    groups: &[AssetGroup],
    state_packets: &[(u8, Vec<u8>)],
    arkade_script: &ScriptBuf,
    intent_txid: bitcoin::Txid,
) -> Result<TxOut> {
    use bitcoin::hashes::Hash;

    let mut asset_packet = Vec::new();
    ark_core::extension::encode_uvarint(&mut asset_packet, groups.len() as u64);
    for group in groups {
        let asset_id = group
            .asset_id
            .ok_or_else(|| anyhow!("renewal batch leaf cannot contain an issuance"))?;
        if group.control_asset.is_some()
            || group.metadata.is_some()
            || group.inputs.len() != 1
            || group.outputs.len() != 1
            || group.inputs[0].input_index != RENEWAL_STATE_INPUT_INDEX as u16
            || group.outputs[0].output_index != RENEWAL_STATE_OUTPUT_INDEX
            || group.inputs[0].amount != group.outputs[0].amount
        {
            return Err(anyhow!("renewal asset group is not an exact self-send"));
        }

        asset_packet.push(0x01); // asset ID present
        let mut asset_txid = asset_id.txid.to_byte_array();
        asset_txid.reverse();
        asset_packet.extend_from_slice(&asset_txid);
        asset_packet.extend_from_slice(&asset_id.group_index.to_le_bytes());

        ark_core::extension::encode_uvarint(&mut asset_packet, 1); // one intent input
        asset_packet.push(0x02); // AssetInputTypeIntent
        let mut proof_txid = intent_txid.to_byte_array();
        proof_txid.reverse();
        asset_packet.extend_from_slice(&proof_txid);
        asset_packet.extend_from_slice(&0_u16.to_le_bytes());
        ark_core::extension::encode_uvarint(&mut asset_packet, 0);

        ark_core::extension::encode_uvarint(&mut asset_packet, 1); // one local output
        asset_packet.push(0x01); // AssetOutputTypeLocal
        asset_packet.extend_from_slice(&RENEWAL_STATE_OUTPUT_INDEX.to_le_bytes());
        ark_core::extension::encode_uvarint(&mut asset_packet, group.outputs[0].amount);
    }

    let emulator_packet = EmulatorPacket::new(vec![IntrospectorEntry {
        vin: RENEWAL_STATE_INPUT_INDEX as u16,
        script: arkade_script.clone(),
        witness: bitcoin::Witness::default(),
    }])
    .context("build renewal batch-leaf emulator packet")?;
    let mut packets = Vec::new();
    if !groups.is_empty() {
        packets.push((0, asset_packet));
    }
    packets.extend(state_packets.iter().cloned());
    packets.push((
        1,
        emulator_packet
            .encode()
            .context("encode renewal batch-leaf emulator packet")?,
    ));

    let mut payload = ark_core::extension::MAGIC_BYTES.to_vec();
    for (packet_type, packet_payload) in packets {
        payload.push(packet_type);
        ark_core::extension::encode_uvarint(&mut payload, packet_payload.len() as u64);
        payload.extend_from_slice(&packet_payload);
    }
    Ok(TxOut {
        value: Amount::ZERO,
        script_pubkey: op_return_script(&payload),
    })
}

/// Mirrors `ark_core::extension`'s OP_RETURN framing, which is private there.
fn op_return_script(data: &[u8]) -> ScriptBuf {
    let mut script = Vec::with_capacity(data.len() + 5);
    script.push(OP_RETURN.to_u8());
    let len = data.len();
    if len <= 75 {
        script.push(len as u8);
    } else if len <= 0xff {
        script.push(0x4c);
        script.push(len as u8);
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

fn record_assets(record: &VtxoRecord) -> Result<Vec<Asset>> {
    let mut seen = std::collections::HashSet::new();
    record
        .assets
        .iter()
        .map(|asset| {
            if asset.amount == 0 || !seen.insert(asset.asset_id) {
                return Err(anyhow!(
                    "indexed VTXO has duplicate or zero asset {}",
                    asset.asset_id
                ));
            }
            Ok(asset.clone())
        })
        .collect()
}

fn asset_amount(assets: &[Asset], asset_id: AssetId) -> u64 {
    assets
        .iter()
        .find(|asset| asset.asset_id == asset_id)
        .map(|asset| asset.amount)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
    use bitcoin::{Network, OutPoint, Txid};

    fn xonly(byte: u8) -> bitcoin::XOnlyPublicKey {
        let secp = Secp256k1::new();
        Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[byte; 32]).unwrap())
            .x_only_public_key()
            .0
    }

    fn asset(byte: u8, group_index: u16) -> AssetId {
        AssetId {
            txid: Txid::from_byte_array([byte; 32]),
            group_index,
        }
    }

    fn indexed(asset_id: AssetId, amount: u64) -> Asset {
        Asset { asset_id, amount }
    }

    fn assigned_group(asset_id: AssetId, amount: u64) -> AssetGroup {
        AssetGroup {
            asset_id: Some(asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount,
            }],
            outputs: vec![AssetOutput {
                output_index: 0,
                amount,
            }],
        }
    }

    fn record(byte: u8, script: &ScriptBuf, amount_sats: u64, assets: Vec<Asset>) -> VtxoRecord {
        VtxoRecord {
            outpoint: OutPoint {
                txid: Txid::from_byte_array([byte; 32]),
                vout: 0,
            },
            script: script.clone(),
            amount_sats,
            assets,
            created_at: Some(1),
            expires_at: Some(i64::MAX),
            is_preconfirmed: false,
            is_swept: false,
            spent_by: None,
            settled_by: None,
            is_unrolled: false,
            is_spent: false,
        }
    }

    fn tree_contract() -> (Secp256k1<bitcoin::secp256k1::All>, TreeContract) {
        let secp = Secp256k1::new();
        let contract = crate::tree::build_tree_contract(
            &secp,
            xonly(3),
            xonly(4),
            xonly(7),
            Sequence::from_height(144),
            Network::Regtest,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
        )
        .unwrap();
        (secp, contract)
    }

    fn player_contract() -> PlayerContract {
        let (secp, tree) = tree_contract();
        crate::player::build_player_contract(
            &secp,
            xonly(6),
            xonly(3),
            xonly(4),
            xonly(7),
            Sequence::from_height(144),
            Network::Regtest,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            &tree.vtxo.script_pubkey(),
        )
        .unwrap()
    }

    fn tree_tx(
        contract: &TreeContract,
        state: TreeState,
        health: u64,
        reserve: u64,
    ) -> Transaction {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: contract.vtxo.script_pubkey(),
                },
                ark_core::anchor_output(),
            ],
        })
        .unwrap();
        let mut groups = vec![assigned_group(asset(2, 0), 1)];
        if reserve > 0 {
            groups.push(assigned_group(asset(2, 1), reserve));
            groups.push(assigned_group(asset(2, 2), reserve));
            groups.push(assigned_group(asset(2, 3), reserve));
            groups.push(assigned_group(asset(2, 4), reserve));
        }
        ark_core::asset::packet::add_asset_packet_to_psbt(&mut psbt, &AssetPacket { groups })
            .unwrap();
        crate::tree::attach_tree_state_packet(&mut psbt, state).unwrap();
        crate::tree::attach_tree_health_packet(
            &mut psbt,
            crate::tree::TreeHealth::new(health).unwrap(),
        )
        .unwrap();
        psbt.unsigned_tx
    }

    fn previous_tree_tx(contract: &TreeContract, state: TreeState) -> Transaction {
        tree_tx(contract, state, 5, 5)
    }

    fn player_state(contract: &PlayerContract) -> PlayerState {
        PlayerState {
            luck: crate::player::PlayerLuck::initial(&contract.vtxo.script_pubkey()).unwrap(),
            axe: crate::player::AxeTier::None,
        }
    }

    fn previous_player_tx(
        contract: &PlayerContract,
        player_asset: AssetId,
        state: PlayerState,
        xp_balance: u64,
    ) -> Transaction {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::from_sat(contract.dust_sats),
                    script_pubkey: contract.vtxo.script_pubkey(),
                },
                ark_core::anchor_output(),
            ],
        })
        .unwrap();
        let mut groups = vec![assigned_group(player_asset, 1)];
        if xp_balance > 0 {
            groups.push(assigned_group(contract.log_asset, 2));
            groups.push(assigned_group(contract.xp_asset, xp_balance));
            groups.push(assigned_group(contract.stone_asset, 2));
            groups.push(assigned_group(contract.iron_ore_asset, 1));
        }
        ark_core::asset::packet::add_asset_packet_to_psbt(&mut psbt, &AssetPacket { groups })
            .unwrap();
        crate::player::attach_player_state_packets(&mut psbt, state).unwrap();
        psbt.unsigned_tx
    }

    #[test]
    fn prepare_tree_rejects_non_canonical_records_without_touching_the_intent() {
        let (_secp, contract) = tree_contract();
        let state = TreeState {
            tree_id: 7,
            x: 1,
            y: 2,
        };
        let previous = previous_tree_tx(&contract, state);
        let good = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(asset(2, 0), 1),
                indexed(asset(2, 1), 5),
                indexed(asset(2, 2), 5),
                indexed(asset(2, 3), 5),
                indexed(asset(2, 4), 5),
            ],
        );

        // The fixture's record txid must match the previous transaction.
        let mut good = good;
        good.outpoint.txid = previous.compute_txid();
        let prepared = prepare_tree(
            &good,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .expect("canonical tree record");
        assert_eq!(prepared.input.assets().len(), 5);

        let mut wrong_amount = good.clone();
        wrong_amount.amount_sats = 331;
        assert!(prepare_tree(
            &wrong_amount,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
        )
        .is_err());

        let mut poisoned = good.clone();
        poisoned.assets.push(indexed(asset(3, 0), 1));
        assert!(prepare_tree(
            &poisoned,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
        )
        .is_err());

        let mut foreign = good.clone();
        foreign.script = ScriptBuf::from_bytes(vec![0x51]);
        assert!(prepare_tree(
            &foreign,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
        )
        .is_err());

        let mut swept = good.clone();
        swept.is_swept = true;
        assert!(prepare_tree(
            &swept,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .is_err());

        let mut expiring = good.clone();
        expiring.expires_at = Some(now_unix() + 10);
        assert!(prepare_tree(
            &expiring,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
        )
        .is_err());
        // The forced rescue path may still renew a live input inside the
        // usual margin, but never an expired one.
        prepare_tree(
            &expiring,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            0,
        )
        .expect("forced renewal inside the margin");
        let mut expired = good.clone();
        expired.expires_at = Some(now_unix() - 1);
        assert!(prepare_tree(
            &expired,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            0,
        )
        .is_err());
    }

    #[test]
    fn extension_txout_carries_asset_state_and_emulator_packets() {
        let (_secp, contract) = tree_contract();
        let state = TreeState {
            tree_id: 7,
            x: 1,
            y: 2,
        };
        let previous = previous_tree_tx(&contract, state);
        let mut good = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(asset(2, 0), 1),
                indexed(asset(2, 1), 5),
                indexed(asset(2, 2), 5),
                indexed(asset(2, 3), 5),
                indexed(asset(2, 4), 5),
            ],
        );
        good.outpoint.txid = previous.compute_txid();
        let prepared = prepare_tree(
            &good,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
        assert_eq!(
            prepared.state_packets,
            [
                (TREE_STATE_PACKET_TYPE, state.encode().to_vec()),
                (
                    TREE_HEALTH_PACKET_TYPE,
                    crate::tree::TreeHealth::new(5).unwrap().encode().to_vec(),
                ),
            ]
        );

        let intent::Output::AssetPacket(extension) = &prepared.outputs[1] else {
            panic!("second renewal output must be the extension");
        };
        let payload = ark_core::extension::extension_payload(&extension.script_pubkey).unwrap();
        let packet_types: Vec<_> = ark_core::extension::iter_packets(payload)
            .unwrap()
            .into_iter()
            .map(|(packet_type, _)| packet_type)
            .collect();
        assert_eq!(
            packet_types,
            [0, TREE_STATE_PACKET_TYPE, TREE_HEALTH_PACKET_TYPE, 1]
        );
        assert_eq!(extension.value, Amount::ZERO);
    }

    #[test]
    fn funded_stump_regrows_in_one_permissionless_batch() {
        let (_secp, contract) = tree_contract();
        let state = TreeState {
            tree_id: 7,
            x: 1,
            y: 2,
        };
        let previous = tree_tx(&contract, state, 0, 5);
        let mut indexed_tree = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(asset(2, 0), 1),
                indexed(asset(2, 1), 5),
                indexed(asset(2, 2), 5),
                indexed(asset(2, 3), 5),
                indexed(asset(2, 4), 5),
            ],
        );
        indexed_tree.outpoint.txid = previous.compute_txid();

        let prepared = prepare_tree(
            &indexed_tree,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
        assert_eq!(
            prepared.input.spend_info().0,
            contract.regrowth_spend_script
        );
        assert_eq!(
            prepared.state_packets,
            [
                (TREE_STATE_PACKET_TYPE, state.encode().to_vec()),
                (
                    TREE_HEALTH_PACKET_TYPE,
                    crate::tree::TreeHealth::new(crate::tree::LOGS_PER_TREE)
                        .unwrap()
                        .encode()
                        .to_vec(),
                ),
            ]
        );
        assert!(prepared.groups.iter().all(|group| {
            group.inputs[0].amount == group.outputs[0].amount
                && group.inputs[0].input_index == RENEWAL_STATE_INPUT_INDEX as u16
                && group.outputs[0].output_index == RENEWAL_STATE_OUTPUT_INDEX
        }));
    }

    #[test]
    fn terminal_stump_renewal_cannot_restore_health_or_assets() {
        let (_secp, contract) = tree_contract();
        let state = TreeState {
            tree_id: 7,
            x: 1,
            y: 2,
        };
        let previous = tree_tx(&contract, state, 0, 0);
        let mut indexed_tree = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![indexed(asset(2, 0), 1)],
        );
        indexed_tree.outpoint.txid = previous.compute_txid();
        let prepared = prepare_tree(
            &indexed_tree,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
        assert_eq!(
            prepared.input.spend_info().0,
            contract.maintenance_spend_script
        );
        assert_eq!(prepared.groups.len(), 1);
        assert_eq!(
            prepared.state_packets,
            [
                (TREE_STATE_PACKET_TYPE, state.encode().to_vec()),
                (
                    TREE_HEALTH_PACKET_TYPE,
                    crate::tree::TreeHealth::new(0).unwrap().encode().to_vec(),
                ),
            ]
        );
    }

    #[test]
    fn intent_deletion_proves_the_same_input_without_creating_outputs() {
        let (_secp, contract) = tree_contract();
        let state = TreeState {
            tree_id: 7,
            x: 1,
            y: 2,
        };
        let previous = previous_tree_tx(&contract, state);
        let mut indexed_tree = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(asset(2, 0), 1),
                indexed(asset(2, 1), 5),
                indexed(asset(2, 2), 5),
                indexed(asset(2, 3), 5),
                indexed(asset(2, 4), 5),
            ],
        );
        indexed_tree.outpoint.txid = previous.compute_txid();
        let prepared = prepare_tree(
            &indexed_tree,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
        let authorizer = Keys::from_hex(&"03".repeat(32)).unwrap();
        let before = now_unix() as u64;
        let deletion = prepare_intent_deletion(&authorizer, &prepared.input).unwrap();
        let after = now_unix() as u64;

        let IntentMessage::Delete { expire_at } = deletion.message else {
            panic!("cleanup proof must carry a delete message");
        };
        assert!(expire_at >= before + INTENT_DELETE_TTL_SECS);
        assert!(expire_at <= after + INTENT_DELETE_TTL_SECS);
        assert_eq!(
            deletion.proof.unsigned_tx.input[1].previous_output,
            prepared.input.outpoint()
        );
        assert_eq!(deletion.proof.unsigned_tx.output.len(), 1);
        assert_eq!(deletion.proof.unsigned_tx.output[0].value, Amount::ZERO);
        assert!(deletion.proof.unsigned_tx.output[0]
            .script_pubkey
            .is_op_return());
        // The message and real input both prove the rollover key. Arkd skips
        // its own covenant signer during ownership verification.
        assert_eq!(deletion.proof.inputs[0].tap_script_sigs.len(), 1);
        assert_eq!(deletion.proof.inputs[1].tap_script_sigs.len(), 1);
    }

    #[test]
    fn player_renewal_preserves_identity_asset_inventory_and_luck() {
        let contract = player_contract();
        let player_asset = asset(9, 0);
        let state = player_state(&contract);
        let previous = previous_player_tx(&contract, player_asset, state, 83);
        let mut good = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(player_asset, 1),
                indexed(contract.log_asset, 2),
                indexed(contract.xp_asset, 83),
                indexed(contract.stone_asset, 2),
                indexed(contract.iron_ore_asset, 1),
            ],
        );
        good.outpoint.txid = previous.compute_txid();
        let prepared = prepare_player(
            &good,
            &previous,
            &contract,
            player_asset,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
        assert_eq!(prepared.groups.len(), 5);
        assert_eq!(prepared.input.spend_info().0, contract.renewal_spend_script);
        assert_eq!(
            prepared.state_packets,
            [
                (PLAYER_ROLL_PACKET_TYPE, state.luck.roll.encode().to_vec()),
                (
                    PLAYER_LUCK_CREDIT_PACKET_TYPE,
                    state.luck.credit.encode().to_vec()
                ),
                (PLAYER_AXE_PACKET_TYPE, state.axe.encode().to_vec()),
            ]
        );

        let mut leaked = good.clone();
        leaked.assets.push(indexed(asset(2, 0), 1));
        assert!(prepare_player(
            &leaked,
            &previous,
            &contract,
            player_asset,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
        )
        .is_err());

        let zero_previous = previous_player_tx(&contract, player_asset, state, 0);
        let mut zero_record = record(
            10,
            &contract.vtxo.script_pubkey(),
            330,
            vec![indexed(player_asset, 1)],
        );
        zero_record.outpoint.txid = zero_previous.compute_txid();
        let prepared = prepare_player(
            &zero_record,
            &zero_previous,
            &contract,
            player_asset,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
        assert_eq!(prepared.groups.len(), 1);
        assert_eq!(prepared.input.spend_info().0, contract.renewal_spend_script);
        let watchtower = prepare_player_watchtower(
            &zero_record,
            &zero_previous,
            &contract,
            player_asset,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
        assert_eq!(
            watchtower.input.spend_info().0,
            contract.watchtower_renewal_spend_script
        );
    }
    #[test]
    fn prepare_player_rejects_foreign_assets_even_without_xp() {
        let contract = player_contract();
        let player_asset = asset(9, 0);
        let state = player_state(&contract);
        let previous = previous_player_tx(&contract, player_asset, state, 0);
        let mut foreign = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(player_asset, 1),
                indexed(contract.log_asset, 2),
                indexed(asset(2, 0), 1),
            ],
        );
        foreign.outpoint.txid = previous.compute_txid();
        assert!(prepare_player(
            &foreign,
            &previous,
            &contract,
            player_asset,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
        )
        .is_err());
        assert!(prepare_player_watchtower(
            &foreign,
            &previous,
            &contract,
            player_asset,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
        )
        .is_err());
    }
    fn fee_estimator(input_sats: u64, output_sats: u64) -> ark_fees::Estimator {
        ark_fees::Estimator::new(ark_fees::Config {
            intent_offchain_input_program: format!("{input_sats}.0"),
            intent_offchain_output_program: format!("{output_sats}.0"),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn renewal_fee_funding_preserves_state_and_returns_exact_change() {
        let (secp, contract) = tree_contract();
        let state = TreeState {
            tree_id: 7,
            x: 1,
            y: 2,
        };
        let previous = previous_tree_tx(&contract, state);
        let mut indexed_tree = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(asset(2, 0), 1),
                indexed(asset(2, 1), 5),
                indexed(asset(2, 2), 5),
                indexed(asset(2, 3), 5),
                indexed(asset(2, 4), 5),
            ],
        );
        indexed_tree.outpoint.txid = previous.compute_txid();
        let renewal = prepare_tree(
            &indexed_tree,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();

        let authorizer = Keys::from_hex(&"07".repeat(32)).unwrap();
        let wallet = ark_core::Vtxo::new_default(
            &secp,
            xonly(3),
            authorizer.owner_pk(),
            Sequence::from_height(144),
            Network::Regtest,
        )
        .unwrap();
        let funding_previous = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::from_sat(1_000),
                    script_pubkey: wallet.script_pubkey(),
                },
                ark_core::anchor_output(),
            ],
        };
        let mut funding_record = record(10, &wallet.script_pubkey(), 1_000, Vec::new());
        funding_record.outpoint.txid = funding_previous.compute_txid();
        let estimators = [fee_estimator(1, 2), fee_estimator(3, 4)];
        assert_eq!(renewal.estimate_base_fee(&estimators).unwrap(), 11);
        let fee = renewal
            .estimate_sponsored_fee(
                &funding_record,
                &funding_previous,
                &wallet,
                &estimators,
                330,
                crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
            )
            .unwrap();
        assert_eq!(fee, 18);

        let mut asset_bearing = funding_record.clone();
        asset_bearing.assets.push(indexed(asset(9, 9), 1));
        assert!(renewal
            .estimate_sponsored_fee(
                &asset_bearing,
                &funding_previous,
                &wallet,
                &estimators,
                330,
                crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
            )
            .is_err());

        let funded = renewal
            .add_fee_funding(
                &funding_record,
                &funding_previous,
                &wallet,
                fee,
                330,
                crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
            )
            .unwrap();
        assert!(funded.funding.is_some());
        assert_eq!(funded.outputs.len(), 3);
        assert_eq!(
            output_txout(&funded.outputs[1]).unwrap().value,
            Amount::from_sat(982)
        );

        let cosigner =
            Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[8; 32]).unwrap()).public_key();
        let bound = bind(&authorizer, funded, &previous, cosigner).unwrap();
        let previous_key = bitcoin::psbt::raw::Key {
            type_value: 0xde,
            key: b"prevarktx".to_vec(),
        };
        assert_eq!(
            bound.intent.proof.inputs[1].unknown.get(&previous_key),
            Some(&bitcoin::consensus::encode::serialize(&previous))
        );
        assert_eq!(
            bound.intent.proof.inputs[2].unknown.get(&previous_key),
            Some(&bitcoin::consensus::encode::serialize(&funding_previous))
        );
        assert!(bound.funding_input.is_some());
        assert_eq!(bound.intent.proof.unsigned_tx.input.len(), 3);
        assert_eq!(bound.intent.proof.unsigned_tx.output.len(), 3);
        assert_eq!(bound.leaf_outputs.len(), 3);
        assert_eq!(bound.leaf_outputs[1].value, Amount::from_sat(982));
        assert_eq!(bound.intent.proof.inputs[2].tap_script_sigs.len(), 1);
    }
}
