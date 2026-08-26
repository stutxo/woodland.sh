//! Batch-renewal intent construction for Forest state VTXOs.
//!
//! A fresh VTXO lifetime only comes from a new Ark batch. Each renewal spends
//! one state VTXO through its dedicated renewal leaf in a version-2 intent
//! proof whose sole purpose is an exact self-send: same P2TR, value, assets,
//! and state packets. The emulator executes the renewal covenant and counters
//! the proof; the batch then recreates the VTXO with a new expiry.
//!
//! Shared trees are gated by the world maintenance key and player state by its
//! owner, so only a client that constructed the exact
//! preservation proof can register it. The covenant constrains that signature
//! to the self-send, never to a transfer.

use crate::arkade::{now_unix, VtxoRecord, DEFAULT_EXPIRY_MARGIN_SECS};
use crate::keys::Keys;
use crate::player::{PlayerContract, PlayerState};
use crate::protocol::{
    PLAYER_IDENTITY_PACKET_TYPE, PLAYER_POSITION_PACKET_TYPE, PLAYER_XP_PACKET_TYPE,
    RENEWAL_INPUT_COUNT, RENEWAL_STATE_INPUT_INDEX, RENEWAL_STATE_OUTPUT_INDEX,
    TREE_HEALTH_PACKET_TYPE, TREE_ROLL_PACKET_TYPE, TREE_STATE_PACKET_TYPE,
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
pub fn prepare_tree(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &TreeContract,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_fuel_asset: AssetId,
    dust_sats: u64,
) -> Result<RenewalIntent> {
    record.validate_creating_transaction(previous_tx)?;
    let state = crate::tree::tree_state_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous tree transaction has no state packet"))?;
    let roll = crate::tree::tree_roll_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous tree transaction has no roll packet"))?;
    let health = crate::tree::tree_health_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous tree transaction has no health packet"))?;
    let assets = record_assets(record)?;
    let tree_count = asset_amount(&assets, tree_asset);
    let log_count = asset_amount(&assets, log_asset);
    let fuel_count = asset_amount(&assets, xp_fuel_asset);
    let other_assets = assets.iter().any(|asset| {
        asset.asset_id != tree_asset
            && asset.asset_id != log_asset
            && asset.asset_id != xp_fuel_asset
    });
    if tree_count != 1
        || other_assets
        || log_count != fuel_count
        || health.value() > log_count
        || record.amount_sats != crate::tree::full_tree_value_sats(dust_sats)?
    {
        return Err(anyhow!("indexed tree VTXO holdings are not canonical"));
    }

    let mut groups = vec![renewal_group(tree_asset, 1)];
    if log_count > 0 {
        groups.push(renewal_group(log_asset, log_count));
    }
    if fuel_count > 0 {
        groups.push(renewal_group(xp_fuel_asset, fuel_count));
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
            (TREE_ROLL_PACKET_TYPE, roll.encode().to_vec()),
            (TREE_HEALTH_PACKET_TYPE, health.encode().to_vec()),
        ],
    )
}

/// Read and validate one player-state VTXO for owner-authorized renewal.
pub fn prepare_player(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &PlayerContract,
    player_asset: AssetId,
) -> Result<RenewalIntent> {
    prepare_player_for_path(record, previous_tx, contract, player_asset, false)
}

/// Prepare the same exact self-send through the optional watchtower leaf.
pub fn prepare_player_watchtower(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &PlayerContract,
    player_asset: AssetId,
) -> Result<RenewalIntent> {
    prepare_player_for_path(record, previous_tx, contract, player_asset, true)
}

fn prepare_player_for_path(
    record: &VtxoRecord,
    previous_tx: &Transaction,
    contract: &PlayerContract,
    player_asset: AssetId,
    watchtower: bool,
) -> Result<RenewalIntent> {
    record.validate_creating_transaction(previous_tx)?;
    let state = crate::player::player_state_from_tx(previous_tx)?
        .ok_or_else(|| anyhow!("previous player transaction has no state packets"))?;
    crate::player::validate_player_state_record(record, contract, player_asset)?;
    let assets = record_assets(record)?;
    let log_count = asset_amount(&assets, contract.log_asset);
    let fuel_count = asset_amount(&assets, contract.xp_fuel_asset);
    if state.xp.value() != fuel_count {
        return Err(anyhow!("player XP is not backed by its XP_FUEL balance"));
    }
    let mut groups = vec![renewal_group(player_asset, 1)];
    if log_count > 0 {
        groups.push(renewal_group(contract.log_asset, log_count));
    }
    if fuel_count > 0 {
        groups.push(renewal_group(contract.xp_fuel_asset, fuel_count));
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
            (PLAYER_XP_PACKET_TYPE, state.xp.encode().to_vec()),
        ],
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
) -> Result<RenewalIntent> {
    record.ensure_live(now_unix(), DEFAULT_EXPIRY_MARGIN_SECS)?;
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
                         message: bitcoin::secp256k1::Message|
     -> Result<
        Vec<(
            bitcoin::secp256k1::schnorr::Signature,
            bitcoin::XOnlyPublicKey,
        )>,
        ark_core::Error,
    > {
        let script = psbt_input.witness_script.clone().ok_or_else(|| {
            ark_core::Error::ad_hoc("missing witness script when signing rollover intent")
        })?;
        if ark_core::script::extract_checksig_pubkeys(&script).contains(&authorizer_keys.owner_pk())
        {
            Ok(authorizer_keys.sign_msg(&message))
        } else {
            Ok(Vec::new())
        }
    };
    let sign_for_onchain = |_: &mut bitcoin::psbt::Input,
                            _: bitcoin::secp256k1::Message|
     -> Result<
        (
            bitcoin::secp256k1::schnorr::Signature,
            bitcoin::XOnlyPublicKey,
        ),
        ark_core::Error,
    > {
        Err(ark_core::Error::ad_hoc(
            "renewal intents have no onchain inputs",
        ))
    };

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
    for input_index in 0..RENEWAL_INPUT_COUNT {
        crate::txbuild::verified_signature_for_key_with_sighash(
            verifier,
            expected,
            &combined,
            input_index,
            rollover_pk,
            "rollover",
            bitcoin::TapSighashType::Default,
        )?;
        crate::txbuild::verified_signature_for_key_with_sighash(
            verifier,
            expected,
            &combined,
            input_index,
            tweaked_emulator,
            "emulator",
            bitcoin::TapSighashType::All,
        )?;
        if combined.inputs[input_index].tap_script_sigs.len() != 2 {
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
            xonly(5),
            xonly(7),
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

    fn previous_tree_tx(contract: &TreeContract, state: TreeState) -> Transaction {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::from_sat(1_980),
                    script_pubkey: contract.vtxo.script_pubkey(),
                },
                ark_core::anchor_output(),
            ],
        })
        .unwrap();
        ark_core::asset::packet::add_asset_packet_to_psbt(
            &mut psbt,
            &AssetPacket {
                groups: vec![
                    assigned_group(asset(2, 0), 1),
                    assigned_group(asset(2, 1), 5),
                    assigned_group(asset(2, 2), 5),
                ],
            },
        )
        .unwrap();
        crate::tree::attach_tree_state_packet(&mut psbt, state).unwrap();
        crate::tree::attach_tree_roll_packet(&mut psbt, advanced_tree_roll(state)).unwrap();
        crate::tree::attach_tree_health_packet(&mut psbt, crate::tree::TreeHealth::new(5).unwrap())
            .unwrap();
        psbt.unsigned_tx
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
            groups.push(assigned_group(contract.xp_fuel_asset, state.xp.value()));
        }
        ark_core::asset::packet::add_asset_packet_to_psbt(&mut psbt, &AssetPacket { groups })
            .unwrap();
        crate::player::attach_player_state_packets(&mut psbt, state).unwrap();
        psbt.unsigned_tx
    }

    fn advanced_tree_roll(state: TreeState) -> crate::tree::TreeRoll {
        let mut roll = crate::tree::TreeRoll::initial(state);
        for _ in 0..7 {
            roll = roll.next();
        }
        roll
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
            1_980,
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
            330
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
            330
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
            330
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
            330
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
            1_980,
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
        )
        .unwrap();
        assert_eq!(
            prepared.state_packets,
            [
                (TREE_STATE_PACKET_TYPE, state.encode().to_vec()),
                (
                    TREE_ROLL_PACKET_TYPE,
                    advanced_tree_roll(state).encode().to_vec(),
                ),
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
            [
                0,
                TREE_STATE_PACKET_TYPE,
                TREE_ROLL_PACKET_TYPE,
                TREE_HEALTH_PACKET_TYPE,
                1,
            ]
        );
        assert_eq!(extension.value, Amount::ZERO);
    }

    #[test]
    fn player_renewal_preserves_player_id_identity_position_and_xp_counter() {
        let contract = player_contract();
        let player_asset = asset(9, 0);
        let state = PlayerState {
            identity: crate::player::PlayerIdentity { player_id: [7; 32] },
            position: crate::player::PlayerPosition { x: 3, y: 17 },
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
                indexed(contract.xp_fuel_asset, 83),
            ],
        );
        good.outpoint.txid = previous.compute_txid();
        let prepared = prepare_player(&good, &previous, &contract, player_asset).unwrap();
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
                (PLAYER_XP_PACKET_TYPE, state.xp.encode().to_vec()),
            ]
        );

        let mut leaked = good.clone();
        leaked.assets.push(indexed(asset(2, 0), 1));
        assert!(prepare_player(&leaked, &previous, &contract, player_asset).is_err());

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
        let prepared =
            prepare_player(&zero_record, &zero_previous, &contract, player_asset).unwrap();
        assert_eq!(prepared.groups.len(), 1);
        assert_eq!(prepared.input.spend_info().0, contract.renewal_spend_script);
        let watchtower =
            prepare_player_watchtower(&zero_record, &zero_previous, &contract, player_asset)
                .unwrap();
        assert_eq!(
            watchtower.input.spend_info().0,
            contract.watchtower_renewal_spend_script
        );
    }
}
