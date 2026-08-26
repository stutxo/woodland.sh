//! Transaction helpers shared by tree deployment and recursive chops.
//!
//! The flow mirrors the SDK client: build -> sign ark inputs -> submit ->
//! (server cosigns) -> sign checkpoints -> finalize.

use crate::arkade::ArkadeRest;
use crate::arkade::{ServerParams, VtxoRecord};
use crate::keys::Keys;
use anyhow::{anyhow, Result};
#[cfg(test)]
use ark_core::asset::AssetId;
use ark_core::send::{sign_ark_transaction, sign_checkpoint_transaction};
use ark_core::server;
use ark_core::Vtxo;
use bitcoin::hashes::Hash;
use bitcoin::psbt;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::{Amount, Psbt, TapLeafHash, TapSighashType, Transaction, Txid, XOnlyPublicKey};
use std::str::FromStr;

const ARK_PSBT_FIELD_TYPE: u8 = 0xde;
const PREVIOUS_ARK_TX_FIELD: &[u8] = b"prevarktx";

#[derive(Debug, Clone)]
pub struct PendingFinalize {
    pub txid: Txid,
    pub checkpoints: Vec<Psbt>,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[derive(Debug, Clone)]
pub struct UnknownSubmission {
    pub txid: Txid,
    pub last_error: String,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[derive(Debug, Clone)]
pub enum RunTxStatus {
    Finalized(Txid),
    Pending(PendingFinalize),
    SubmissionUnknown(UnknownSubmission),
}

/// Everything the builders need from `server::Info`, reconstructed from the
/// REST `/v1/info` payload. Fields the builders never read are placeholders.
pub fn server_info(params: &ServerParams) -> server::Info {
    let signer_pk = params
        .signer_pk
        .public_key(bitcoin::secp256k1::Parity::Even);
    server::Info {
        version: String::new(),
        signer_pk,
        forfeit_pk: params.forfeit_pk.inner,
        forfeit_address: params.forfeit_address.clone(),
        checkpoint_tapscript: params.checkpoint_tapscript.clone(),
        network: params.network,
        session_duration: 60,
        unilateral_exit_delay: params.unilateral_exit_delay,
        boarding_exit_delay: params.unilateral_exit_delay,
        utxo_min_amount: None,
        utxo_max_amount: None,
        vtxo_min_amount: Some(Amount::from_sat(params.vtxo_min_sats)),
        vtxo_max_amount: None,
        dust: Amount::from_sat(params.dust_sats),
        fees: None,
        scheduled_session: None,
        deprecated_signers: Vec::new(),
        service_status: Default::default(),
        digest: String::new(),
        max_tx_weight: params.max_tx_weight,
        max_op_return_outputs: params.max_op_return_outputs,
    }
}

/// The player's default VTXO contract (2-of-2 forfeit + CSV exit leaves).
pub fn player_vtxo(keys: &Keys, params: &ServerParams) -> Result<Vtxo> {
    Vtxo::new_default(
        &keys.secp,
        params.signer_pk,
        keys.owner_pk(),
        params.unilateral_exit_delay,
        params.network,
    )
    .map_err(|e| anyhow!("build vtxo: {e}"))
}

/// Convert an indexer VTXO record into a spendable [`VtxoInput`].
/// Only valid for VTXOs on the player's own default contract.
pub fn vtxo_input(record: &VtxoRecord, vtxo: &Vtxo) -> Result<ark_core::send::VtxoInput> {
    if record.script != vtxo.script_pubkey() {
        return Err(anyhow!(
            "indexed VTXO script does not match the wallet contract"
        ));
    }
    let (spend_script, control_block) = vtxo
        .forfeit_spend_info()
        .map_err(|e| anyhow!("forfeit spend info: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    let assets = record
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
        .collect::<Result<Vec<_>>>()?;
    Ok(ark_core::send::VtxoInput::new(
        spend_script,
        None,
        control_block,
        vtxo.tapscripts(),
        vtxo.script_pubkey(),
        Amount::from_sat(record.amount_sats),
        record.outpoint,
        assets,
    ))
}

pub fn parse_asset_id_pub(s: &str) -> Option<ark_core::asset::AssetId> {
    parse_asset_id_inner(s)
}

fn parse_asset_id_inner(s: &str) -> Option<ark_core::asset::AssetId> {
    if s.len() != 68 || !s.is_ascii() {
        return None;
    }
    let txid = Txid::from_str(&s[..64]).ok()?;
    let bytes: [u8; 2] = bitcoin::hex::FromHex::from_hex(&s[64..]).ok()?;
    let asset = ark_core::asset::AssetId {
        txid,
        group_index: u16::from_le_bytes(bytes),
    };
    (asset.to_string() == s).then_some(asset)
}

/// Validate every Ark input's checkpoint and creating transaction, then attach
/// the creating transactions atomically as Ark PSBT input fields.
pub(crate) fn attach_previous_ark_transactions<'a>(
    psbt: &mut Psbt,
    checkpoints: &[Psbt],
    previous_input_txs: impl IntoIterator<Item = &'a Transaction>,
) -> Result<()> {
    let previous_input_txs = previous_input_txs.into_iter().collect::<Vec<_>>();
    let input_count = psbt.unsigned_tx.input.len();
    if psbt.inputs.len() != input_count
        || checkpoints.len() != input_count
        || previous_input_txs.len() != input_count
    {
        return Err(anyhow!(
            "spend context input lengths do not match: transaction {}, PSBT {}, checkpoints {}, previous transactions {}",
            input_count,
            psbt.inputs.len(),
            checkpoints.len(),
            previous_input_txs.len(),
        ));
    }
    if psbt.outputs.len() != psbt.unsigned_tx.output.len() {
        return Err(anyhow!(
            "Ark PSBT output metadata does not match its transaction"
        ));
    }

    let previous_key = bitcoin::psbt::raw::Key {
        type_value: ARK_PSBT_FIELD_TYPE,
        key: PREVIOUS_ARK_TX_FIELD.to_vec(),
    };
    let mut updated = psbt.clone();
    for (input_index, (checkpoint, previous_tx)) in
        checkpoints.iter().zip(previous_input_txs).enumerate()
    {
        if checkpoint.unsigned_tx.input.len() != 1 || checkpoint.inputs.len() != 1 {
            return Err(anyhow!(
                "checkpoint {input_index} must contain exactly one input"
            ));
        }
        if checkpoint.outputs.len() != checkpoint.unsigned_tx.output.len() {
            return Err(anyhow!(
                "checkpoint {input_index} output metadata does not match its transaction"
            ));
        }

        let source = checkpoint.unsigned_tx.input[0].previous_output;
        if source.txid != previous_tx.compute_txid() {
            return Err(anyhow!(
                "previous Ark transaction does not create input {input_index}"
            ));
        }
        let source_output = previous_tx
            .output
            .get(source.vout as usize)
            .ok_or_else(|| anyhow!("previous Ark transaction omits input {input_index}"))?;
        if checkpoint.inputs[0].witness_utxo.as_ref() != Some(source_output) {
            return Err(anyhow!(
                "checkpoint {input_index} witness UTXO does not match its source"
            ));
        }

        let checkpoint_output = checkpoint
            .unsigned_tx
            .output
            .first()
            .ok_or_else(|| anyhow!("checkpoint {input_index} has no output zero"))?;
        let checkpoint_outpoint = bitcoin::OutPoint {
            txid: checkpoint.unsigned_tx.compute_txid(),
            vout: 0,
        };
        if updated.unsigned_tx.input[input_index].previous_output != checkpoint_outpoint {
            return Err(anyhow!(
                "Ark input {input_index} does not spend checkpoint output zero"
            ));
        }
        let ark_input = &mut updated.inputs[input_index];
        if ark_input.witness_utxo.as_ref() != Some(checkpoint_output) {
            return Err(anyhow!(
                "Ark input {input_index} witness UTXO does not match checkpoint output zero"
            ));
        }
        if ark_input.unknown.contains_key(&previous_key) {
            return Err(anyhow!(
                "previous Ark transaction field already exists on input {input_index}"
            ));
        }
        ark_input.unknown.insert(
            previous_key.clone(),
            bitcoin::consensus::encode::serialize(previous_tx),
        );
    }

    *psbt = updated;
    Ok(())
}

fn make_sign_fn<'a>(
    keys: &'a Keys,
) -> impl FnMut(
    &mut psbt::Input,
    Message,
) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error>
       + 'a {
    |_input, msg| Ok(keys.sign_msg(&msg))
}

/// Sign ark inputs, submit, sign returned checkpoints, finalize.
/// Returns the ark txid once preconfirmed.
pub async fn run_tx(
    keys: &Keys,
    rest: &ArkadeRest,
    ark_tx: Psbt,
    checkpoint_txs: Vec<Psbt>,
) -> Result<RunTxStatus> {
    let (signed_ark, checkpoints) = prepare_tx(keys, ark_tx, checkpoint_txs)?;
    submit_prepared(keys, rest, signed_ark, checkpoints).await
}

fn prepare_tx(
    keys: &Keys,
    mut ark_tx: Psbt,
    checkpoint_txs: Vec<Psbt>,
) -> Result<(Psbt, Vec<Psbt>)> {
    for i in 0..checkpoint_txs.len() {
        sign_ark_transaction(make_sign_fn(keys), &mut ark_tx, i)
            .map_err(|e| anyhow!("sign ark input {i}: {e}"))?;
    }
    Ok((ark_tx, checkpoint_txs))
}

async fn submit_prepared(
    keys: &Keys,
    rest: &ArkadeRest,
    ark_tx: Psbt,
    checkpoint_txs: Vec<Psbt>,
) -> Result<RunTxStatus> {
    let expected_txid = ark_tx.unsigned_tx.compute_txid();
    let (txid, returned_ark, returned_checkpoints) =
        match rest.submit_tx(&ark_tx, &checkpoint_txs).await {
            Ok(response) => response,
            Err(error) => {
                let text = format!("{error:#}");
                if submission_error_is_definitive(&error) {
                    return Err(error);
                }
                return Ok(RunTxStatus::SubmissionUnknown(UnknownSubmission {
                    txid: expected_txid,
                    last_error: text,
                }));
            }
        };
    let (_signed_ark, signed_checkpoints) = verify_submit_response(
        keys,
        &ark_tx,
        &checkpoint_txs,
        txid,
        returned_ark,
        returned_checkpoints,
    )?;
    finalize_verified_response(keys, rest, txid, signed_checkpoints).await
}

fn submission_error_is_definitive(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::arkade::HttpFailure>()
        .and_then(crate::arkade::HttpFailure::status_code)
        .is_some_and(is_definitive_submission_rejection_status)
}

fn is_definitive_submission_rejection_status(status: u16) -> bool {
    (400..500).contains(&status) && !matches!(status, 408 | 409 | 425 | 429)
}

async fn finalize_verified_response(
    keys: &Keys,
    rest: &ArkadeRest,
    txid: Txid,
    signed_checkpoints: Vec<Psbt>,
) -> Result<RunTxStatus> {
    let pending = PendingFinalize {
        txid,
        checkpoints: signed_checkpoints,
    };
    match finalize_pending(keys, rest, &pending).await {
        Ok(()) => Ok(RunTxStatus::Finalized(txid)),
        Err(_) => Ok(RunTxStatus::Pending(pending)),
    }
}

fn verify_submit_response(
    keys: &Keys,
    expected_ark: &Psbt,
    expected_checkpoints: &[Psbt],
    response_txid: Txid,
    returned_ark: Psbt,
    returned_checkpoints: Vec<Psbt>,
) -> Result<(Psbt, Vec<Psbt>)> {
    let expected_txid = expected_ark.unsigned_tx.compute_txid();
    if response_txid != expected_txid
        || returned_ark.unsigned_tx.compute_txid() != expected_txid
        || returned_ark.unsigned_tx != expected_ark.unsigned_tx
    {
        return Err(anyhow!("operator changed the submitted Ark transaction"));
    }
    if returned_ark.inputs.len() != expected_ark.inputs.len() {
        return Err(anyhow!("operator returned an invalid Ark PSBT"));
    }

    let mut verified_ark = expected_ark.clone();
    for input_index in 0..expected_ark.inputs.len() {
        let (key, signature) =
            verified_server_signature(keys, expected_ark, &returned_ark, input_index)?;
        verified_ark.inputs[input_index]
            .tap_script_sigs
            .insert(key, signature);
    }

    if returned_checkpoints.len() != expected_checkpoints.len() {
        return Err(anyhow!(
            "operator returned {} checkpoints for {} inputs",
            returned_checkpoints.len(),
            expected_checkpoints.len()
        ));
    }
    let mut returned_by_txid = std::collections::HashMap::new();
    for checkpoint in returned_checkpoints {
        let txid = checkpoint.unsigned_tx.compute_txid();
        if returned_by_txid.insert(txid, checkpoint).is_some() {
            return Err(anyhow!("operator returned duplicate checkpoint {txid}"));
        }
    }

    let mut verified_checkpoints = Vec::with_capacity(expected_checkpoints.len());
    for expected in expected_checkpoints {
        let checkpoint_txid = expected.unsigned_tx.compute_txid();
        let returned = returned_by_txid
            .remove(&checkpoint_txid)
            .ok_or_else(|| anyhow!("operator omitted checkpoint {checkpoint_txid}"))?;
        if returned.unsigned_tx != expected.unsigned_tx
            || returned.inputs.len() != expected.inputs.len()
        {
            return Err(anyhow!(
                "operator changed checkpoint transaction {checkpoint_txid}"
            ));
        }
        let mut verified = expected.clone();
        for input_index in 0..expected.inputs.len() {
            let (key, signature) =
                verified_server_signature(keys, expected, &returned, input_index)?;
            verified.inputs[input_index]
                .tap_script_sigs
                .insert(key, signature);
        }
        verified_checkpoints.push(verified);
    }
    if !returned_by_txid.is_empty() {
        return Err(anyhow!("operator returned unexpected checkpoints"));
    }
    Ok((verified_ark, verified_checkpoints))
}

fn verified_server_signature(
    keys: &Keys,
    expected: &Psbt,
    returned: &Psbt,
    input_index: usize,
) -> Result<((XOnlyPublicKey, TapLeafHash), bitcoin::taproot::Signature)> {
    let expected_input = expected
        .inputs
        .get(input_index)
        .ok_or_else(|| anyhow!("missing expected PSBT input {input_index}"))?;
    let (_, (spend_script, _)) = expected_input
        .tap_scripts
        .first_key_value()
        .ok_or_else(|| anyhow!("expected PSBT input {input_index} has no spend script"))?;
    let server_keys: Vec<_> = ark_core::script::extract_checksig_pubkeys(spend_script)
        .into_iter()
        .filter(|pk| *pk != keys.owner_pk())
        .collect();
    if server_keys.len() != 1 {
        return Err(anyhow!(
            "expected PSBT input {input_index} does not have one server signer"
        ));
    }
    verified_signature_for_key(
        keys,
        expected,
        returned,
        input_index,
        server_keys[0],
        "operator",
    )
}

pub(crate) fn verified_signature_for_key(
    keys: &Keys,
    expected: &Psbt,
    returned: &Psbt,
    input_index: usize,
    signer_key: XOnlyPublicKey,
    signer_name: &str,
) -> Result<((XOnlyPublicKey, TapLeafHash), bitcoin::taproot::Signature)> {
    verified_signature_for_key_with_sighash(
        keys,
        expected,
        returned,
        input_index,
        signer_key,
        signer_name,
        TapSighashType::Default,
    )
}

/// [`verified_signature_for_key`] with an explicit expected sighash. Intent
/// proofs use `SIGHASH_ALL` (arkd builds them that way), while direct Ark and
/// checkpoint transactions use the default sighash.
#[allow(clippy::too_many_arguments)]
pub(crate) fn verified_signature_for_key_with_sighash(
    keys: &Keys,
    expected: &Psbt,
    returned: &Psbt,
    input_index: usize,
    signer_key: XOnlyPublicKey,
    signer_name: &str,
    expected_sighash: TapSighashType,
) -> Result<((XOnlyPublicKey, TapLeafHash), bitcoin::taproot::Signature)> {
    let expected_input = expected
        .inputs
        .get(input_index)
        .ok_or_else(|| anyhow!("missing expected PSBT input {input_index}"))?;
    let (_, (spend_script, leaf_version)) = expected_input
        .tap_scripts
        .first_key_value()
        .ok_or_else(|| anyhow!("expected PSBT input {input_index} has no spend script"))?;
    let leaf_hash = TapLeafHash::from_script(spend_script, *leaf_version);
    let signature = returned
        .inputs
        .get(input_index)
        .and_then(|input| input.tap_script_sigs.get(&(signer_key, leaf_hash)))
        .cloned()
        .ok_or_else(|| anyhow!("{signer_name} signature missing from PSBT input {input_index}"))?;
    if signature.sighash_type != expected_sighash {
        return Err(anyhow!(
            "{signer_name} used an unexpected sighash on PSBT input {input_index}"
        ));
    }

    let prevouts = expected
        .inputs
        .iter()
        .map(|input| {
            input
                .witness_utxo
                .clone()
                .ok_or_else(|| anyhow!("expected PSBT input is missing its witness UTXO"))
        })
        .collect::<Result<Vec<_>>>()?;
    let sighash = SighashCache::new(&expected.unsigned_tx)
        .taproot_script_spend_signature_hash(
            input_index,
            &Prevouts::All(&prevouts),
            leaf_hash,
            signature.sighash_type,
        )
        .map_err(|error| anyhow!("operator signature sighash: {error}"))?;
    let message = Message::from_digest(sighash.to_raw_hash().to_byte_array());
    keys.secp
        .verify_schnorr(&signature.signature, &message, &signer_key)
        .map_err(|error| {
            anyhow!("invalid {signer_name} signature on PSBT input {input_index}: {error}")
        })?;
    Ok(((signer_key, leaf_hash), signature))
}

/// Retry finalization without rebuilding or resubmitting the Ark transaction.
pub async fn finalize_pending(
    keys: &Keys,
    rest: &ArkadeRest,
    pending: &PendingFinalize,
) -> Result<()> {
    let checkpoints = sign_pending_checkpoints(keys, pending)?;
    finalize_checkpoints(rest, pending.txid, &checkpoints).await
}

fn sign_pending_checkpoints(keys: &Keys, pending: &PendingFinalize) -> Result<Vec<Psbt>> {
    let mut checkpoints = pending.checkpoints.clone();
    for checkpoint in &mut checkpoints {
        let input = checkpoint
            .inputs
            .first_mut()
            .ok_or_else(|| anyhow!("pending checkpoint has no input"))?;
        if input.witness_script.is_none() {
            return Err(anyhow!("pending checkpoint missing witness script"));
        }
        sign_checkpoint_transaction(make_sign_fn(keys), checkpoint)
            .map_err(|error| anyhow!("sign checkpoint: {error}"))?;
    }
    Ok(checkpoints)
}

async fn finalize_checkpoints(rest: &ArkadeRest, txid: Txid, checkpoints: &[Psbt]) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            sleep_ms(500 * attempt as u64).await;
        }
        match rest.finalize_tx(txid, checkpoints).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .expect("attempted")
        .context("finalize failed after retries"))
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[cfg(target_arch = "wasm32")]
async fn sleep_ms(ms: u64) {
    use wasm_bindgen::JsCast;

    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .expect("window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                resolve.unchecked_ref(),
                ms as i32,
            )
            .expect("setTimeout");
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
    use bitcoin::{absolute, transaction, Network, OutPoint, ScriptBuf, Sequence, TxIn, TxOut};

    fn transaction(inputs: Vec<TxIn>, outputs: Vec<TxOut>) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs,
            output: outputs,
        }
    }

    #[test]
    fn asset_id_parser_rejects_noncanonical_and_non_ascii_text() {
        let canonical = AssetId {
            txid: Txid::from_byte_array([0xab; 32]),
            group_index: 513,
        }
        .to_string();
        assert_eq!(
            parse_asset_id_pub(&canonical).unwrap().to_string(),
            canonical
        );
        assert!(parse_asset_id_pub(&canonical.to_uppercase()).is_none());

        let split_multibyte_boundary = format!("{}é{}", "0".repeat(63), "0".repeat(3));
        assert_eq!(split_multibyte_boundary.len(), 68);
        assert!(parse_asset_id_pub(&split_multibyte_boundary).is_none());
    }

    #[test]
    fn submission_status_classification_keeps_ambiguous_outcomes_unknown() {
        for status in [400, 401, 403, 404, 405, 413, 415, 422] {
            assert!(
                is_definitive_submission_rejection_status(status),
                "status {status}"
            );
        }
        for status in [408, 409, 425, 429, 500, 502, 503, 504] {
            assert!(
                !is_definitive_submission_rejection_status(status),
                "status {status}"
            );
        }
    }

    #[test]
    fn wallet_vtxo_input_rejects_an_indexer_script_mismatch() {
        let secp = Secp256k1::new();
        let signer = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[1; 32]).unwrap())
            .x_only_public_key()
            .0;
        let owner = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[2; 32]).unwrap())
            .x_only_public_key()
            .0;
        let wallet = Vtxo::new_default(
            &secp,
            signer,
            owner,
            Sequence::from_height(144),
            Network::Regtest,
        )
        .unwrap();
        let record = VtxoRecord {
            outpoint: OutPoint::null(),
            script: ScriptBuf::new(),
            amount_sats: 330,
            assets: Vec::new(),
            created_at: Some(1),
            expires_at: Some(i64::MAX),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
        };

        let error = vtxo_input(&record, &wallet).unwrap_err();
        assert!(error.to_string().contains("wallet contract"));
    }

    fn previous_transaction(byte: u8) -> Transaction {
        transaction(
            vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([byte; 32]),
                    vout: u32::from(byte),
                },
                ..Default::default()
            }],
            vec![TxOut {
                value: Amount::from_sat(330 + u64::from(byte)),
                script_pubkey: ScriptBuf::new(),
            }],
        )
    }

    fn checkpoint(previous: &Transaction) -> Psbt {
        let source = previous.output[0].clone();
        let mut checkpoint = Psbt::from_unsigned_tx(transaction(
            vec![TxIn {
                previous_output: OutPoint {
                    txid: previous.compute_txid(),
                    vout: 0,
                },
                ..Default::default()
            }],
            vec![source.clone(), ark_core::anchor_output()],
        ))
        .unwrap();
        checkpoint.inputs[0].witness_utxo = Some(source);
        checkpoint
    }

    fn context_fixture() -> (Vec<Transaction>, Vec<Psbt>, Psbt) {
        let previous = vec![previous_transaction(1), previous_transaction(2)];
        let checkpoints = previous.iter().map(checkpoint).collect::<Vec<_>>();
        let mut ark = Psbt::from_unsigned_tx(transaction(
            checkpoints
                .iter()
                .map(|checkpoint| TxIn {
                    previous_output: OutPoint {
                        txid: checkpoint.unsigned_tx.compute_txid(),
                        vout: 0,
                    },
                    ..Default::default()
                })
                .collect(),
            vec![ark_core::anchor_output()],
        ))
        .unwrap();
        for (input, checkpoint) in ark.inputs.iter_mut().zip(&checkpoints) {
            input.witness_utxo = Some(checkpoint.unsigned_tx.output[0].clone());
        }
        (previous, checkpoints, ark)
    }

    fn assert_context_rejected(
        previous: &[Transaction],
        checkpoints: &[Psbt],
        ark: &mut Psbt,
        expected_error: &str,
    ) {
        let before = ark.clone();
        let error = attach_previous_ark_transactions(ark, checkpoints, previous.iter())
            .expect_err("invalid spend context was accepted");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected error: {error:#}"
        );
        assert_eq!(ark.unsigned_tx, before.unsigned_tx);
        assert_eq!(ark.inputs, before.inputs);
        assert_eq!(ark.outputs, before.outputs);
    }

    #[test]
    fn previous_transactions_are_attached_after_complete_mapping_validation() {
        let (previous, checkpoints, mut ark) = context_fixture();
        attach_previous_ark_transactions(&mut ark, &checkpoints, previous.iter()).unwrap();

        let key = bitcoin::psbt::raw::Key {
            type_value: ARK_PSBT_FIELD_TYPE,
            key: PREVIOUS_ARK_TX_FIELD.to_vec(),
        };
        for (input, expected) in ark.inputs.iter().zip(&previous) {
            let encoded = input.unknown.get(&key).unwrap();
            let decoded: Transaction = bitcoin::consensus::encode::deserialize(encoded).unwrap();
            assert_eq!(&decoded, expected);
        }
    }

    #[test]
    fn previous_transaction_attachment_rejects_every_invalid_mapping_atomically() {
        let (previous, checkpoints, mut ark) = context_fixture();
        ark.inputs.pop();
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "input lengths do not match",
        );

        let (previous, checkpoints, mut ark) = context_fixture();
        ark.outputs.pop();
        assert_context_rejected(&previous, &checkpoints, &mut ark, "output metadata");

        let (previous, mut checkpoints, mut ark) = context_fixture();
        checkpoints.pop();
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "input lengths do not match",
        );

        let (mut previous, checkpoints, mut ark) = context_fixture();
        previous.pop();
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "input lengths do not match",
        );

        let (previous, mut checkpoints, mut ark) = context_fixture();
        checkpoints[1].unsigned_tx.input.push(TxIn::default());
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "must contain exactly one input",
        );

        let (previous, mut checkpoints, mut ark) = context_fixture();
        checkpoints[1].outputs.pop();
        assert_context_rejected(&previous, &checkpoints, &mut ark, "output metadata");

        let (previous, mut checkpoints, mut ark) = context_fixture();
        checkpoints[1].unsigned_tx.input[0].previous_output.txid =
            Txid::from_byte_array([0x77; 32]);
        assert_context_rejected(&previous, &checkpoints, &mut ark, "does not create input 1");

        let (previous, mut checkpoints, mut ark) = context_fixture();
        checkpoints[1].unsigned_tx.input[0].previous_output.vout = 1;
        assert_context_rejected(&previous, &checkpoints, &mut ark, "omits input 1");

        let (previous, mut checkpoints, mut ark) = context_fixture();
        checkpoints[1].inputs[0].witness_utxo = None;
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "checkpoint 1 witness UTXO",
        );

        let (previous, mut checkpoints, mut ark) = context_fixture();
        checkpoints[1].unsigned_tx.output.clear();
        checkpoints[1].outputs.clear();
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "checkpoint 1 has no output zero",
        );

        let (previous, checkpoints, mut ark) = context_fixture();
        ark.unsigned_tx.input[1].previous_output = OutPoint::null();
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "Ark input 1 does not spend checkpoint output zero",
        );

        let (previous, checkpoints, mut ark) = context_fixture();
        ark.inputs[1].witness_utxo = None;
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "Ark input 1 witness UTXO",
        );

        let (previous, checkpoints, mut ark) = context_fixture();
        ark.inputs[1].unknown.insert(
            bitcoin::psbt::raw::Key {
                type_value: ARK_PSBT_FIELD_TYPE,
                key: PREVIOUS_ARK_TX_FIELD.to_vec(),
            },
            vec![0xaa],
        );
        assert_context_rejected(
            &previous,
            &checkpoints,
            &mut ark,
            "field already exists on input 1",
        );
    }
}
