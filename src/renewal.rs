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
    PLAYER_IDENTITY_PACKET_TYPE, PLAYER_LUCK_CREDIT_PACKET_TYPE, PLAYER_POSITION_PACKET_TYPE,
    PLAYER_ROLL_PACKET_TYPE, PLAYER_XP_PACKET_TYPE, RENEWAL_INPUT_COUNT, RENEWAL_STATE_INPUT_INDEX,
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
    outputs: Vec<intent::Output>,
    groups: Vec<AssetGroup>,
    state_packets: Vec<(u8, Vec<u8>)>,
    input_expires_at: i64,
    /// The renewal Arkade script; its tweaked emulator key must countersign.
    pub arkade_script: ScriptBuf,
}

/// A rollover proof signed by the dedicated service key and bound to its exact
/// state, previous transaction, message, and ephemeral batch cosigner.
pub struct PreparedRenewal {
    pub intent: Intent,
    pub message: IntentMessage,
    /// Exact JSON committed by the proof's fake message input.
    pub message_json: String,
    pub input: intent::Input,
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
    let other_assets = assets.iter().any(|asset| {
        asset.asset_id != tree_asset && asset.asset_id != log_asset && asset.asset_id != xp_asset
    });
    let health = if raw_health.value() == 0 && log_count > 0 {
        crate::tree::TreeHealth::new(crate::tree::LOGS_PER_TREE)?
    } else {
        raw_health
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
    build(
        record,
        &contract.vtxo,
        &contract.renewal_spend_script,
        contract.renewal_arkade_script.clone(),
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
        asset.asset_id != player_asset
            && asset.asset_id != contract.log_asset
            && asset.asset_id != contract.xp_asset
    });
    if other_assets {
        return Err(anyhow!("indexed player VTXO holdings are not canonical"));
    }
    let log_count = asset_amount(&assets, contract.log_asset);
    let xp_count = asset_amount(&assets, contract.xp_asset);
    if state.xp.value() != xp_count {
        return Err(anyhow!(
            "player XP counter is not backed by its XP asset balance"
        ));
    }
    let mut groups = vec![renewal_group(player_asset, 1)];
    if log_count > 0 {
        groups.push(renewal_group(contract.log_asset, log_count));
    }
    if xp_count > 0 {
        groups.push(renewal_group(contract.xp_asset, xp_count));
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
            (
                PLAYER_IDENTITY_PACKET_TYPE,
                state.identity.encode().to_vec(),
            ),
            (
                PLAYER_POSITION_PACKET_TYPE,
                state.position.encode().to_vec(),
            ),
            (PLAYER_ROLL_PACKET_TYPE, state.luck.roll.encode().to_vec()),
            (
                PLAYER_LUCK_CREDIT_PACKET_TYPE,
                state.luck.credit.encode().to_vec(),
            ),
            (PLAYER_XP_PACKET_TYPE, state.xp.encode().to_vec()),
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
        outputs,
        groups,
        state_packets,
        input_expires_at,
        arkade_script,
    })
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
        outputs,
        groups,
        state_packets,
        input_expires_at,
        arkade_script,
    } = prepared;
    if previous_tx.compute_txid() != input.outpoint().txid {
        return Err(anyhow!(
            "previous transaction {} does not create renewal input {}",
            previous_tx.compute_txid(),
            input.outpoint()
        ));
    }
    let previous_output = previous_tx
        .output
        .get(input.outpoint().vout as usize)
        .ok_or_else(|| anyhow!("previous transaction omits the renewal input"))?;
    if previous_output.script_pubkey != *input.script_pubkey()
        || previous_output.value != input.amount()
    {
        return Err(anyhow!(
            "previous transaction output does not match the indexed renewal input"
        ));
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
        vec![input.clone()],
        outputs,
        message.clone(),
    )
    .map_err(|error| anyhow!("build renewal intent: {error}"))?;

    attach_previous_transaction(&mut intent, previous_tx)?;

    let leaf_outputs = vec![
        TxOut {
            value: input.amount(),
            script_pubkey: input.script_pubkey().clone(),
        },
        batch_leaf_extension_txout(
            &groups,
            &state_packets,
            &arkade_script,
            intent.proof.unsigned_tx.compute_txid(),
        )?,
    ];

    Ok(PreparedRenewal {
        intent,
        message,
        message_json,
        input,
        input_expires_at,
        leaf_outputs,
        arkade_script,
    })
}

fn attach_previous_transaction(intent: &mut Intent, previous_tx: &Transaction) -> Result<()> {
    // The fake message input shares the state input's script but has no
    // previous Ark transaction; only the real input needs one.
    let key = bitcoin::psbt::raw::Key {
        type_value: 0xde,
        key: b"prevarktx".to_vec(),
    };
    let proof_input = intent
        .proof
        .inputs
        .get_mut(RENEWAL_STATE_INPUT_INDEX)
        .ok_or_else(|| anyhow!("renewal proof is missing its state input"))?;
    if proof_input.unknown.contains_key(&key) {
        return Err(anyhow!(
            "previous Ark transaction field already exists on input {RENEWAL_STATE_INPUT_INDEX}"
        ));
    }
    proof_input
        .unknown
        .insert(key, bitcoin::consensus::encode::serialize(previous_tx));
    Ok(())
}

/// Merge the emulator response into the exact submitted PSBT, then verify the
/// dedicated rollover and script-tweaked emulator signatures on both inputs.
/// PSBT combination rejects conflicting metadata.
pub fn combine_and_verify_emulator_approval(
    verifier: &Keys,
    expected: &Psbt,
    approved: Psbt,
    emulator_pk: bitcoin::XOnlyPublicKey,
    arkade_script: &ScriptBuf,
) -> Result<Psbt> {
    if expected.unsigned_tx != approved.unsigned_tx
        || expected.inputs.len() != approved.inputs.len()
        || expected.inputs.len() != RENEWAL_INPUT_COUNT
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
    let rollover_pk = verifier.owner_pk();
    // The authorizer signs only when its key is a renewal-leaf signer; a
    // permissionless leaf (operator + tweaked emulator) takes the emulator
    // signature alone and arkd completes its own when it enforces.
    let client_sig_expected = expected.inputs[RENEWAL_STATE_INPUT_INDEX]
        .witness_script
        .as_ref()
        .map(ark_core::script::extract_checksig_pubkeys)
        .is_some_and(|signers| signers.contains(&rollover_pk));
    for input_index in 0..RENEWAL_INPUT_COUNT {
        if client_sig_expected {
            crate::txbuild::verified_signature_for_key_with_sighash(
                verifier,
                expected,
                &combined,
                input_index,
                rollover_pk,
                "rollover",
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
        let expected_sigs = 1 + usize::from(client_sig_expected);
        if combined.inputs[input_index].tap_script_sigs.len() != expected_sigs {
            return Err(anyhow!("rollover proof contains an unexpected signature"));
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
            Sequence::from_height(144),
            Network::Regtest,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
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

    fn previous_player_tx(
        contract: &PlayerContract,
        player_asset: AssetId,
        state: PlayerState,
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
        if state.xp.value() > 0 {
            groups.push(assigned_group(contract.log_asset, 2));
            groups.push(assigned_group(contract.xp_asset, state.xp.value()));
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
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .expect("canonical tree record");
        assert_eq!(prepared.input.assets().len(), 3);

        let mut wrong_amount = good.clone();
        wrong_amount.amount_sats = 331;
        assert!(prepare_tree(
            &wrong_amount,
            &previous,
            &contract,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
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
            330,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )
        .unwrap();
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
    fn player_renewal_preserves_player_id_identity_position_and_xp_counter() {
        let contract = player_contract();
        let player_asset = asset(9, 0);
        let identity = crate::player::PlayerIdentity { player_id: [7; 32] };
        let state = PlayerState {
            identity,
            position: crate::player::PlayerPosition { x: 3, y: 17 },
            luck: crate::player::PlayerLuck::initial(identity),
            xp: crate::player::PlayerXp::new(83),
        };
        let previous = previous_player_tx(&contract, player_asset, state);
        let mut good = record(
            9,
            &contract.vtxo.script_pubkey(),
            330,
            vec![
                indexed(player_asset, 1),
                indexed(contract.log_asset, 2),
                indexed(contract.xp_asset, 83),
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
        assert_eq!(prepared.groups.len(), 3);
        assert_eq!(prepared.input.spend_info().0, contract.renewal_spend_script);
        assert_eq!(
            prepared.state_packets,
            [
                (
                    PLAYER_IDENTITY_PACKET_TYPE,
                    state.identity.encode().to_vec()
                ),
                (
                    PLAYER_POSITION_PACKET_TYPE,
                    state.position.encode().to_vec()
                ),
                (PLAYER_ROLL_PACKET_TYPE, state.luck.roll.encode().to_vec()),
                (
                    PLAYER_LUCK_CREDIT_PACKET_TYPE,
                    state.luck.credit.encode().to_vec()
                ),
                (PLAYER_XP_PACKET_TYPE, state.xp.encode().to_vec()),
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

        let zero_state = PlayerState {
            xp: crate::player::PlayerXp::new(0),
            ..state
        };
        let zero_previous = previous_player_tx(&contract, player_asset, zero_state);
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
        let identity = crate::player::PlayerIdentity { player_id: [7; 32] };
        let state = PlayerState {
            identity,
            position: crate::player::PlayerPosition { x: 3, y: 17 },
            luck: crate::player::PlayerLuck::initial(identity),
            xp: crate::player::PlayerXp::new(0),
        };
        let previous = previous_player_tx(&contract, player_asset, state);
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
}
