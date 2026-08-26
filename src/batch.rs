//! Ark batch orchestration for woodland.sh renewal intents.
//!
//! This is a Forest-specific reduction of the pinned SDK's `join_next_batch`
//! state machine: no wallet or coin selection, exactly one covenant intent,
//! and the emulator's intent/finalization cosigning that Forest's Arkade
//! covenant VTXOs require. The pinned `ark-rest` SSE parser assumes one event
//! per transport chunk, so this module uses its typed unary client plus a
//! small buffered SSE reader instead.

use crate::arkade::{EmulatorRest, EmulatorTxTreeNode, ServerParams};
use crate::keys::Keys;
use anyhow::{anyhow, Context, Result};
use ark_core::batch::{
    aggregate_nonces, create_and_sign_forfeit_txs, generate_nonce_tree, sign_batch_tree_tx,
    NonceKps,
};
use ark_core::server::{BatchTreeEventType, PartialSigTree, StreamEvent, TreeTxNoncePks};
use ark_core::{TxGraph, TxGraphChunk};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::taproot::{LeafVersion, TaprootBuilder};
use bitcoin::{Amount, OutPoint, Psbt, ScriptBuf, TapSighashType, TxOut, Txid, XOnlyPublicKey};
#[cfg(target_arch = "wasm32")]
use futures::future::{select, Either};
use futures::StreamExt;
use musig::musig;
use std::collections::{HashMap, HashSet};
#[cfg(target_arch = "wasm32")]
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

/// Batches are frequent on regtest; a stalled stream or skipped intent must
/// still fail with a clear error instead of hanging a maintenance loop.
const BATCH_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const MAX_BATCH_GRAPH_NODES: usize = 512;
const MAX_SSE_FRAME_BYTES: usize = 1_048_576;

pub struct BatchServices {
    pub ark: ark_rest::Client,
    pub emulator: EmulatorRest,
    pub params: ServerParams,
    ark_base: String,
    digest: String,
}

impl BatchServices {
    pub async fn connect(
        ark_url: &str,
        emulator: EmulatorRest,
        params: ServerParams,
    ) -> Result<Self> {
        let ark_base = ark_url.trim_end_matches('/').to_string();
        let ark = ark_rest::Client::new(ark_base.clone())
            .map_err(|error| anyhow!("build arkd REST client: {error}"))?;
        // The typed client tracks the server digest for guarded calls; keep a
        // copy for the raw SSE request, which must present the same headers.
        let info = ark
            .get_info()
            .await
            .map_err(|error| anyhow!("read arkd REST info: {error}"))?;
        require_matching_server_params(&info, &params)?;
        require_zero_renewal_fees(&info)?;
        Ok(Self {
            ark,
            emulator,
            params,
            ark_base,
            digest: info.digest,
        })
    }
}

pub struct RenewalOutcome {
    pub commitment_txid: Txid,
    /// Outpoint of the renewed state VTXO in the new batch leaf.
    pub outpoint: OutPoint,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Step {
    #[default]
    Start,
    BatchStarted,
    BatchSigningStarted,
    FinalizationSubmitted,
}

#[derive(Debug, Default)]
struct BatchProgress {
    step: Step,
    batch_id: Option<String>,
    tree_signatures_submitted: bool,
    expected_commitment_txid: Option<Txid>,
}

impl BatchProgress {
    fn is_start(&self) -> bool {
        self.step == Step::Start
    }

    fn matches_batch(&self, id: &str) -> bool {
        self.batch_id.as_deref() == Some(id)
    }

    fn begin_batch(&mut self, id: String) -> Result<()> {
        if !self.is_start() {
            return Err(anyhow!("batch started after the renewal had advanced"));
        }
        self.batch_id = Some(id);
        self.step = Step::BatchStarted;
        Ok(())
    }

    fn accepts_tree_chunk(&self, id: &str) -> bool {
        matches!(self.step, Step::BatchStarted | Step::BatchSigningStarted)
            && self.matches_batch(id)
    }

    fn accepts_tree_signature(&self, id: &str) -> bool {
        self.step == Step::BatchSigningStarted && self.matches_batch(id)
    }

    fn accepts_signing_start(&self, id: &str) -> bool {
        self.step == Step::BatchStarted && self.matches_batch(id)
    }

    fn mark_signing_started(&mut self, id: &str) -> Result<()> {
        if !self.accepts_signing_start(id) {
            return Err(anyhow!("tree signing started out of order"));
        }
        self.step = Step::BatchSigningStarted;
        Ok(())
    }

    fn accepts_nonces(&self, id: &str) -> bool {
        self.step == Step::BatchSigningStarted
            && self.matches_batch(id)
            && !self.tree_signatures_submitted
    }

    fn mark_tree_signatures_submitted(&mut self, id: &str) -> Result<()> {
        if !self.accepts_nonces(id) {
            return Err(anyhow!("tree signatures were submitted out of order"));
        }
        self.tree_signatures_submitted = true;
        Ok(())
    }

    fn accepts_finalization(&self, id: &str) -> bool {
        self.step == Step::BatchSigningStarted
            && self.matches_batch(id)
            && self.tree_signatures_submitted
    }

    fn mark_finalization_submitted(&mut self, id: &str, commitment_txid: Txid) -> Result<()> {
        if !self.accepts_finalization(id) {
            return Err(anyhow!("batch finalization arrived out of order"));
        }
        self.expected_commitment_txid = Some(commitment_txid);
        self.step = Step::FinalizationSubmitted;
        Ok(())
    }

    fn finalize(&self, id: &str, commitment_txid: Txid) -> Result<bool> {
        if self.step != Step::FinalizationSubmitted || !self.matches_batch(id) {
            return Ok(false);
        }
        if self.expected_commitment_txid != Some(commitment_txid) {
            return Err(anyhow!(
                "batch finalized an unexpected commitment transaction {commitment_txid}"
            ));
        }
        Ok(true)
    }
}

/// Register one emulator-approved exact-self-send rollover and ride its batch
/// to finalization. `rollover_keys` authorizes liveness without holding player
/// funds; `cosigner` is ephemeral and used only for the batch tree MuSig2.
#[cfg(not(target_arch = "wasm32"))]
pub async fn join_batch_with_intent(
    services: &BatchServices,
    rollover_keys: &Keys,
    cosigner: &Keys,
    emulator_pk: bitcoin::XOnlyPublicKey,
    renewal: &crate::renewal::ApprovedRenewal,
) -> Result<RenewalOutcome> {
    let deadline = tokio::time::Instant::now() + BATCH_JOIN_TIMEOUT;
    let registration = tokio::time::timeout_at(
        deadline,
        services
            .ark
            .register_intent(&renewal.message, &renewal.proof),
    )
    .await;
    let intent_id = match registration {
        Ok(Ok(intent_id)) => intent_id,
        Ok(Err(error)) => return Err(anyhow!("register renewal intent: {error:?}")),
        Err(_) => return Err(anyhow!(
            "timed out registering the renewal intent; arkd may retain it and require operator cleanup before retry"
        )),
    };
    let forfeits_released = AtomicBool::new(false);
    let result = match tokio::time::timeout_at(
        deadline,
        join_registered_batch(
            services,
            rollover_keys,
            cosigner,
            emulator_pk,
            renewal,
            intent_id.clone(),
            &forfeits_released,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow!("timed out waiting for the renewal batch")),
    };

    result.map_err(|error| {
        if forfeits_released.load(Ordering::SeqCst) {
            error.context(
                "the signed forfeits reached the emulator; reconcile the input's indexed lineage before retrying",
            )
        } else {
            error
        }
    })
}
#[cfg(target_arch = "wasm32")]
async fn browser_timeout<F: Future>(future: F) -> Option<F::Output> {
    let timeout_ms = u32::try_from(BATCH_JOIN_TIMEOUT.as_millis()).unwrap_or(u32::MAX);
    match select(
        Box::pin(future),
        Box::pin(gloo_timers::future::TimeoutFuture::new(timeout_ms)),
    )
    .await
    {
        Either::Left((output, _)) => Some(output),
        Either::Right(_) => None,
    }
}

#[cfg(target_arch = "wasm32")]
pub async fn join_batch_with_intent(
    services: &BatchServices,
    authorizer_keys: &Keys,
    cosigner: &Keys,
    emulator_pk: bitcoin::XOnlyPublicKey,
    renewal: &crate::renewal::ApprovedRenewal,
) -> Result<RenewalOutcome> {
    let registration = browser_timeout(
        services
            .ark
            .register_intent(&renewal.message, &renewal.proof),
    )
    .await
    .ok_or_else(|| anyhow!("timed out registering the renewal intent"))?;
    let intent_id = registration.map_err(|error| anyhow!("register renewal intent: {error:?}"))?;
    let forfeits_released = AtomicBool::new(false);
    let result = browser_timeout(join_registered_batch(
        services,
        authorizer_keys,
        cosigner,
        emulator_pk,
        renewal,
        intent_id,
        &forfeits_released,
    ))
    .await
    .unwrap_or_else(|| Err(anyhow!("timed out waiting for the renewal batch")));
    result.map_err(|error| {
        if forfeits_released.load(Ordering::SeqCst) {
            error.context(
                "the signed forfeits reached the emulator; reconcile the input's indexed lineage before retrying",
            )
        } else {
            error
        }
    })
}

async fn join_registered_batch(
    services: &BatchServices,
    rollover_keys: &Keys,
    cosigner: &Keys,
    emulator_pk: bitcoin::XOnlyPublicKey,
    renewal: &crate::renewal::ApprovedRenewal,
    intent_id: String,
    forfeits_released: &AtomicBool,
) -> Result<RenewalOutcome> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;

    let cosigner_pk = cosigner.keypair.public_key();
    let cosigner_xonly = cosigner.owner_pk();
    let input = renewal.input.clone();
    let arkade_script = &renewal.arkade_script;
    let input_outpoint = input.outpoint();
    let topics = vec![
        input_outpoint.to_string(),
        cosigner_pk.serialize().to_lower_hex_string(),
    ];
    let response = open_event_stream(&services.ark_base, &topics, &services.digest).await?;
    let mut stream = response.bytes_stream();

    let forfeit_xonly = services.params.forfeit_pk.inner.x_only_public_key().0;
    let mut rng = rand::rngs::OsRng;

    let mut progress = BatchProgress::default();
    let mut batch_expiry: Option<bitcoin::Sequence> = None;
    let mut vtxo_chunks: Option<Vec<TxGraphChunk>> = Some(Vec::new());
    let mut vtxo_graph: Option<TxGraph> = None;
    let mut connector_chunks: Option<Vec<TxGraphChunk>> = Some(Vec::new());
    let mut unsigned_commitment_tx: Option<Psbt> = None;
    let mut nonce_kps: Option<NonceKps> = None;
    let mut agg_nonce_pks: HashMap<Txid, musig::AggregatedNonce> = HashMap::new();
    let mut renewed_outpoint: Option<OutPoint> = None;
    let mut buffer = String::new();

    loop {
        let chunk = stream
            .next()
            .await
            .ok_or_else(|| anyhow!("batch event stream closed before finalization"))?
            .map_err(|error| anyhow!("read batch event stream: {error}"))?;
        if chunk.len() > MAX_SSE_FRAME_BYTES {
            return Err(anyhow!(
                "batch event transport chunk exceeds the safety limit"
            ));
        }
        buffer.push_str(std::str::from_utf8(&chunk).context("batch event stream is not UTF-8")?);

        while let Some(frame) = take_sse_frame(&mut buffer) {
            if frame.len() > MAX_SSE_FRAME_BYTES {
                return Err(anyhow!("batch event frame exceeds the safety limit"));
            }
            let Some(event) = parse_sse_event(&frame)? else {
                continue;
            };
            match event {
                StreamEvent::StreamStarted(_) | StreamEvent::Heartbeat => {}
                StreamEvent::BatchStarted(event) => {
                    if !progress.is_start() {
                        continue;
                    }
                    let hash = sha256::Hash::hash(intent_id.as_bytes())
                        .as_byte_array()
                        .to_lower_hex_string();
                    if !event
                        .intent_id_hashes
                        .iter()
                        .any(|candidate| candidate == &hash)
                    {
                        continue;
                    }
                    validate_batch_expiry(
                        event.batch_expiry,
                        renewal.input_expires_at,
                        crate::arkade::now_unix(),
                    )?;
                    services
                        .ark
                        .confirm_registration(intent_id.clone())
                        .await
                        .map_err(|error| anyhow!("confirm renewal intent: {error:?}"))?;
                    progress.begin_batch(event.id)?;
                    batch_expiry = Some(event.batch_expiry);
                }
                StreamEvent::TreeTx(event) => {
                    if !progress.accepts_tree_chunk(&event.id) {
                        continue;
                    }
                    let (chunks, name) = match event.batch_tree_event_type {
                        BatchTreeEventType::Vtxo => (
                            vtxo_chunks
                                .as_mut()
                                .ok_or_else(|| anyhow!("unexpected VTXO tree chunk"))?,
                            "VTXO",
                        ),
                        BatchTreeEventType::Connector => (
                            connector_chunks
                                .as_mut()
                                .ok_or_else(|| anyhow!("unexpected connector tree chunk"))?,
                            "connector",
                        ),
                    };
                    if chunks.len() >= MAX_BATCH_GRAPH_NODES {
                        return Err(anyhow!("{name} tree exceeds the node safety limit"));
                    }
                    chunks.push(event.tx_graph_chunk);
                }
                StreamEvent::TreeSignature(event) => {
                    if !progress.accepts_tree_signature(&event.id)
                        || !matches!(event.batch_tree_event_type, BatchTreeEventType::Vtxo)
                    {
                        continue;
                    }
                    let graph = vtxo_graph
                        .as_mut()
                        .ok_or_else(|| anyhow!("tree signature without VTXO graph"))?;
                    if graph.find(&event.txid).is_none() {
                        return Err(anyhow!(
                            "received a signature for unknown renewal tree transaction {}",
                            event.txid
                        ));
                    }
                    graph
                        .apply(|node| {
                            if node.root().unsigned_tx.compute_txid() == event.txid {
                                node.set_signature(event.signature);
                                Ok(false)
                            } else {
                                Ok(true)
                            }
                        })
                        .map_err(|error| anyhow!("apply tree signature: {error}"))?;
                }
                StreamEvent::TreeSigningStarted(event) => {
                    if !progress.accepts_signing_start(&event.id) {
                        continue;
                    }
                    if !event.cosigners_pubkeys.contains(&cosigner_pk) {
                        return Err(anyhow!(
                            "renewal cosigner is not in the batch's cosigner set"
                        ));
                    }
                    let chunks = vtxo_chunks
                        .take()
                        .ok_or_else(|| anyhow!("tree signing started without VTXO chunks"))?;
                    let expiry = batch_expiry.ok_or_else(|| anyhow!("missing batch expiry"))?;
                    renewed_outpoint = Some(validate_vtxo_tree(
                        &chunks,
                        &event.unsigned_commitment_tx,
                        expiry,
                        forfeit_xonly,
                        cosigner_pk,
                        &event.cosigners_pubkeys,
                        &renewal.leaf_outputs,
                    )?);
                    let graph = TxGraph::new(chunks)
                        .map_err(|error| anyhow!("build VTXO batch tree: {error}"))?;
                    let nonces = generate_nonce_tree(
                        &mut rng,
                        &graph,
                        cosigner_pk,
                        &event.unsigned_commitment_tx,
                    )
                    .map_err(|error| anyhow!("generate renewal batch nonces: {error}"))?;
                    services
                        .ark
                        .submit_tree_nonces(&event.id, cosigner_pk, nonces.to_nonce_pks())
                        .await
                        .map_err(|error| anyhow!("submit renewal batch nonces: {error:?}"))?;
                    vtxo_graph = Some(graph);
                    nonce_kps = Some(nonces);
                    unsigned_commitment_tx = Some(event.unsigned_commitment_tx);
                    progress.mark_signing_started(&event.id)?;
                }
                StreamEvent::TreeNonces(event) => {
                    if !progress.accepts_nonces(&event.id) {
                        continue;
                    }
                    if !event.nonces.0.contains_key(&cosigner_xonly) {
                        continue;
                    }
                    let graph = vtxo_graph
                        .as_ref()
                        .ok_or_else(|| anyhow!("tree nonces without VTXO graph"))?;
                    if graph.find(&event.txid).is_none() {
                        return Err(anyhow!(
                            "received a renewal nonce for unknown tree transaction {}",
                            event.txid
                        ));
                    }
                    if agg_nonce_pks.contains_key(&event.txid) {
                        return Err(anyhow!(
                            "received duplicate renewal nonces for tree transaction {}",
                            event.txid
                        ));
                    }
                    agg_nonce_pks.insert(event.txid, aggregate_nonces(event.nonces));
                    if agg_nonce_pks.len() != graph.nb_of_nodes() {
                        continue;
                    }
                    let nonces = nonce_kps
                        .as_mut()
                        .ok_or_else(|| anyhow!("missing renewal batch nonce tree"))?;
                    let commitment = unsigned_commitment_tx
                        .as_ref()
                        .ok_or_else(|| anyhow!("missing unsigned commitment transaction"))?;
                    let expiry = batch_expiry.ok_or_else(|| anyhow!("missing batch expiry"))?;
                    let mut partial = PartialSigTree::default();
                    for txid in graph.as_map().keys() {
                        let aggregate = agg_nonce_pks
                            .get(txid)
                            .ok_or_else(|| anyhow!("missing aggregate nonce for {txid}"))?;
                        let sigs = sign_batch_tree_tx(
                            *txid,
                            expiry,
                            forfeit_xonly,
                            &cosigner.keypair,
                            *aggregate,
                            graph,
                            commitment,
                            nonces,
                        )
                        .map_err(|error| anyhow!("sign renewal batch tree: {error}"))?;
                        partial.0.extend(sigs.0);
                    }
                    services
                        .ark
                        .submit_tree_signatures(&event.id, cosigner_pk, partial)
                        .await
                        .map_err(|error| anyhow!("submit renewal batch signatures: {error:?}"))?;
                    progress.mark_tree_signatures_submitted(&event.id)?;
                }
                StreamEvent::TreeNoncesAggregated(_) => {}
                StreamEvent::BatchFinalization(event) => {
                    if !progress.accepts_finalization(&event.id) {
                        continue;
                    }
                    let signing_commitment = unsigned_commitment_tx
                        .as_ref()
                        .ok_or_else(|| anyhow!("missing unsigned commitment transaction"))?;
                    if event.commitment_tx.unsigned_tx != signing_commitment.unsigned_tx {
                        return Err(anyhow!(
                            "batch finalization changed the renewal commitment transaction"
                        ));
                    }
                    let graph = vtxo_graph
                        .as_ref()
                        .ok_or_else(|| anyhow!("batch finalization without a VTXO tree"))?;
                    verify_vtxo_tree_signatures(graph, signing_commitment)?;
                    let chunks = connector_chunks
                        .take()
                        .ok_or_else(|| anyhow!("batch finalization without connector chunks"))?;
                    validate_connector_tree(
                        &chunks,
                        signing_commitment,
                        services.params.dust_sats,
                    )?;
                    let connector_graph = TxGraph::new(chunks.clone())
                        .map_err(|error| anyhow!("build connector tree: {error}"))?;
                    let sign_fn = forfeit_sign_fn(rollover_keys);
                    let forfeits = create_and_sign_forfeit_txs(
                        sign_fn,
                        std::slice::from_ref(&input),
                        &connector_graph.leaves(),
                        &services.params.forfeit_address,
                        Amount::from_sat(services.params.dust_sats),
                    )
                    .map_err(|error| anyhow!("build renewal forfeit: {error}"))?;
                    let connector_tree = chunks.iter().map(emulator_tree_node).collect::<Vec<_>>();
                    let forfeit_b64 = forfeits
                        .iter()
                        .map(|psbt| b64.encode(psbt.serialize()))
                        .collect::<Vec<_>>();
                    forfeits_released.store(true, Ordering::SeqCst);
                    let approved = services
                        .emulator
                        .submit_finalization(
                            &b64.encode(renewal.proof.serialize()),
                            &renewal.message_json,
                            forfeit_b64,
                            connector_tree,
                            &b64.encode(event.commitment_tx.serialize()),
                        )
                        .await?;
                    let complete = combine_forfeits(
                        rollover_keys,
                        &forfeits,
                        approved.signed_forfeits,
                        emulator_pk,
                        arkade_script,
                    )?;
                    services
                        .ark
                        .submit_signed_forfeit_txs(complete, None)
                        .await
                        .map_err(|error| anyhow!("submit renewal forfeit: {error:?}"))?;
                    progress.mark_finalization_submitted(
                        &event.id,
                        signing_commitment.unsigned_tx.compute_txid(),
                    )?;
                }
                StreamEvent::BatchFinalized(event) => {
                    if !progress.finalize(&event.id, event.commitment_txid)? {
                        continue;
                    }
                    return Ok(RenewalOutcome {
                        commitment_txid: event.commitment_txid,
                        outpoint: renewed_outpoint
                            .ok_or_else(|| anyhow!("renewed VTXO outpoint was not located"))?,
                    });
                }
                StreamEvent::BatchFailed(event) => {
                    if progress.matches_batch(&event.id) {
                        return Err(anyhow!("renewal batch failed: {}", event.reason));
                    }
                }
            }
        }
        if buffer.len() > MAX_SSE_FRAME_BYTES {
            return Err(anyhow!("unterminated batch event exceeds the safety limit"));
        }
    }
}

fn require_zero_renewal_fees(info: &ark_core::server::Info) -> Result<()> {
    let schedules = [
        ("current", info.fees.as_ref()),
        (
            "scheduled",
            info.scheduled_session
                .as_ref()
                .and_then(|session| session.fees.as_ref()),
        ),
    ];
    for (name, fees) in schedules {
        let Some(fees) = fees else {
            continue;
        };
        for (kind, expression) in [
            ("offchain-input", fees.intent_fee.offchain_input.as_deref()),
            (
                "offchain-output",
                fees.intent_fee.offchain_output.as_deref(),
            ),
        ] {
            let Some(expression) = expression else {
                continue;
            };
            let expression = expression.trim();
            let is_zero_literal =
                expression.is_empty() || expression.parse::<f64>().is_ok_and(|value| value == 0.0);
            if !is_zero_literal {
                return Err(anyhow!(
                    "woodland.sh renewal requires zero intent fees; arkd's {name} {kind} fee is {expression:?}"
                ));
            }
        }
    }
    Ok(())
}

fn validate_batch_expiry(
    batch_expiry: bitcoin::Sequence,
    input_expires_at: i64,
    now_unix: i64,
) -> Result<()> {
    let remaining = input_expires_at
        .checked_sub(now_unix)
        .ok_or_else(|| anyhow!("renewal input expiry arithmetic overflow"))?;
    if remaining <= 0 {
        return Err(anyhow!("renewal input expired before batch confirmation"));
    }
    let lifetime_secs = match batch_expiry.to_relative_lock_time() {
        Some(bitcoin::relative::LockTime::Time(time)) => i64::from(time.value()) * 512,
        Some(bitcoin::relative::LockTime::Blocks(_)) => {
            return Err(anyhow!(
                "cannot prove a block-based renewal expiry extends a Unix-time input expiry"
            ));
        }
        None => return Err(anyhow!("renewal batch announced a disabled expiry")),
    };
    if lifetime_secs < remaining {
        return Err(anyhow!(
            "renewal batch lifetime {lifetime_secs}s is shorter than the input's {remaining}s remaining lifetime"
        ));
    }
    Ok(())
}

fn require_matching_server_params(
    info: &ark_core::server::Info,
    params: &ServerParams,
) -> Result<()> {
    if info.signer_pk.x_only_public_key().0 != params.signer_pk
        || info.forfeit_pk != params.forfeit_pk.inner
        || info.forfeit_address != params.forfeit_address
        || info.network != params.network
        || info.dust.to_sat() != params.dust_sats
        || info.vtxo_min_amount.map(Amount::to_sat) != Some(params.vtxo_min_sats)
        || info.unilateral_exit_delay != params.unilateral_exit_delay
        || info.checkpoint_tapscript != params.checkpoint_tapscript
        || info.max_tx_weight != params.max_tx_weight
        || info.max_op_return_outputs != params.max_op_return_outputs
    {
        return Err(anyhow!(
            "arkd parameters changed before renewal batch registration"
        ));
    }
    Ok(())
}

struct ValidatedGraph<'a> {
    txs: HashMap<Txid, &'a Psbt>,
}

/// Validate the flat graph independently of arkd's declared child map before
/// any nonce, partial signature, or forfeit signature leaves this process.
fn validate_graph<'a>(
    chunks: &'a [TxGraphChunk],
    commitment: &Psbt,
    commitment_vout: u32,
    name: &str,
) -> Result<ValidatedGraph<'a>> {
    if chunks.is_empty() || chunks.len() > MAX_BATCH_GRAPH_NODES {
        return Err(anyhow!(
            "{name} must contain between 1 and {MAX_BATCH_GRAPH_NODES} received nodes"
        ));
    }

    let mut txs = HashMap::with_capacity(chunks.len());
    let mut child_maps = HashMap::with_capacity(chunks.len());
    for chunk in chunks {
        let txid = chunk.tx.unsigned_tx.compute_txid();
        if chunk.txid.is_some_and(|declared| declared != txid) {
            return Err(anyhow!("{name} chunk declares the wrong transaction ID"));
        }
        if txs.insert(txid, &chunk.tx).is_some() {
            return Err(anyhow!("{name} contains duplicate transaction {txid}"));
        }
        child_maps.insert(txid, &chunk.children);
        let tx = &chunk.tx.unsigned_tx;
        if tx.version.0 != 3
            || tx.lock_time != bitcoin::absolute::LockTime::ZERO
            || tx.input.len() != 1
            || tx.input[0].sequence != bitcoin::Sequence::MAX
            || tx.output.is_empty()
        {
            return Err(anyhow!(
                "{name} transaction {txid} has a non-canonical shape"
            ));
        }
        let anchor = ark_core::anchor_output();
        if tx.output.last() != Some(&anchor)
            || tx.output.iter().filter(|output| **output == anchor).count() != 1
        {
            return Err(anyhow!(
                "{name} transaction {txid} has a non-canonical anchor"
            ));
        }
        if chunk.children.len() > tx.output.len() - 1 {
            return Err(anyhow!("{name} transaction {txid} has too many children"));
        }
    }

    let mut referenced = HashSet::new();
    for chunk in chunks {
        let parent_txid = chunk.tx.unsigned_tx.compute_txid();
        for (vout, child_txid) in &chunk.children {
            if *vout as usize >= chunk.tx.unsigned_tx.output.len() - 1 {
                return Err(anyhow!(
                    "{name} child {child_txid} spends an invalid output of {parent_txid}"
                ));
            }
            // Topic-filtered streams retain the parent's full child map while
            // omitting sibling subtrees. Validate each received child and
            // leave absent sibling branches opaque.
            let Some(child) = txs.get(child_txid) else {
                continue;
            };
            if !referenced.insert(*child_txid) {
                return Err(anyhow!("{name} child {child_txid} has multiple parents"));
            }
            let expected = OutPoint {
                txid: parent_txid,
                vout: *vout,
            };
            if child.unsigned_tx.input[0].previous_output != expected {
                return Err(anyhow!(
                    "{name} child {child_txid} does not spend declared parent output {expected}"
                ));
            }
            let child_value = sum_outputs(&child.unsigned_tx.output, name)?;
            if child_value != chunk.tx.unsigned_tx.output[*vout as usize].value.to_sat() {
                return Err(anyhow!(
                    "{name} child {child_txid} does not preserve its parent output value"
                ));
            }
        }
    }

    let roots = txs
        .keys()
        .copied()
        .filter(|txid| !referenced.contains(txid))
        .collect::<Vec<_>>();
    let [root] = roots.as_slice() else {
        return Err(anyhow!("{name} must have exactly one root"));
    };
    // Prove every received node is reachable before calling the pinned SDK's
    // recursive graph constructor. This rejects cycles without recursing into
    // attacker-controlled child maps.
    let mut visited = HashSet::with_capacity(txs.len());
    let mut pending = vec![*root];
    while let Some(txid) = pending.pop() {
        if !visited.insert(txid) {
            return Err(anyhow!("{name} contains a cycle"));
        }
        let children = child_maps
            .get(&txid)
            .ok_or_else(|| anyhow!("{name} traversal lost transaction {txid}"))?;
        pending.extend(
            children
                .values()
                .filter(|child| txs.contains_key(*child))
                .copied(),
        );
    }
    if visited.len() != txs.len() {
        return Err(anyhow!("{name} contains a disconnected cycle"));
    }
    let commitment_txid = commitment.unsigned_tx.compute_txid();
    let root_outpoint = OutPoint {
        txid: commitment_txid,
        vout: commitment_vout,
    };
    let root_tx = txs[root];
    if root_tx.unsigned_tx.input[0].previous_output != root_outpoint {
        return Err(anyhow!(
            "{name} root does not spend commitment output {root_outpoint}"
        ));
    }
    let commitment_output = commitment
        .unsigned_tx
        .output
        .get(commitment_vout as usize)
        .ok_or_else(|| anyhow!("commitment transaction omits {name} output {commitment_vout}"))?;
    if sum_outputs(&root_tx.unsigned_tx.output, name)? != commitment_output.value.to_sat() {
        return Err(anyhow!(
            "{name} root does not preserve the commitment value"
        ));
    }

    Ok(ValidatedGraph { txs })
}

fn sum_outputs(outputs: &[TxOut], name: &str) -> Result<u64> {
    outputs.iter().try_fold(0_u64, |sum, output| {
        sum.checked_add(output.value.to_sat())
            .ok_or_else(|| anyhow!("{name} output value overflow"))
    })
}

fn graph_prevout<'a>(
    graph: &'a ValidatedGraph<'a>,
    commitment: &'a Psbt,
    tx: &bitcoin::Transaction,
    name: &str,
) -> Result<&'a TxOut> {
    let outpoint = tx.input[0].previous_output;
    let parent = if outpoint.txid == commitment.unsigned_tx.compute_txid() {
        &commitment.unsigned_tx
    } else {
        &graph
            .txs
            .get(&outpoint.txid)
            .ok_or_else(|| anyhow!("{name} parent {} is missing", outpoint.txid))?
            .unsigned_tx
    };
    parent
        .output
        .get(outpoint.vout as usize)
        .ok_or_else(|| anyhow!("{name} previous output {outpoint} is missing"))
}

fn validate_vtxo_tree(
    chunks: &[TxGraphChunk],
    commitment: &Psbt,
    batch_expiry: bitcoin::Sequence,
    forfeit_pk: XOnlyPublicKey,
    own_cosigner: bitcoin::secp256k1::PublicKey,
    batch_cosigners: &[bitcoin::secp256k1::PublicKey],
    leaf_outputs: &[TxOut],
) -> Result<OutPoint> {
    let graph = validate_graph(chunks, commitment, 0, "VTXO tree")?;
    let mut batch_operator = None;
    for (txid, psbt) in &graph.txs {
        let cosigners = vtxo_tree_cosigners(psbt)?;
        if !cosigners.contains(&own_cosigner) {
            return Err(anyhow!(
                "VTXO tree transaction {txid} omits the renewal cosigner"
            ));
        }
        let extras = cosigners
            .iter()
            .filter(|key| !batch_cosigners.contains(key))
            .copied()
            .collect::<Vec<_>>();
        let [operator] = extras.as_slice() else {
            return Err(anyhow!(
                "VTXO tree transaction {txid} must contain exactly one batch operator cosigner"
            ));
        };
        if batch_operator
            .replace(*operator)
            .is_some_and(|key| key != *operator)
        {
            return Err(anyhow!(
                "VTXO tree transactions use inconsistent batch operator cosigners"
            ));
        }
        let prevout = graph_prevout(&graph, commitment, &psbt.unsigned_tx, "VTXO tree")?;
        let expected_script =
            expected_vtxo_tree_script(psbt, batch_expiry, forfeit_pk, own_cosigner)?;
        if prevout.script_pubkey != expected_script {
            return Err(anyhow!(
                "VTXO tree transaction {txid} is not locked to its declared cosigners"
            ));
        }
    }

    let mut expected_outputs = leaf_outputs.to_vec();
    expected_outputs.push(ark_core::anchor_output());
    let matching = chunks
        .iter()
        .filter(|chunk| {
            chunk.children.is_empty() && chunk.tx.unsigned_tx.output == expected_outputs
        })
        .collect::<Vec<_>>();
    let [leaf] = matching.as_slice() else {
        return Err(anyhow!(
            "VTXO tree must contain exactly one byte-exact renewal leaf"
        ));
    };
    Ok(OutPoint {
        txid: leaf.tx.unsigned_tx.compute_txid(),
        vout: 0,
    })
}

fn expected_vtxo_tree_script(
    psbt: &Psbt,
    batch_expiry: bitcoin::Sequence,
    forfeit_pk: XOnlyPublicKey,
    own_cosigner: bitcoin::secp256k1::PublicKey,
) -> Result<ScriptBuf> {
    let mut cosigners = vtxo_tree_cosigners(psbt)?;
    if !cosigners.contains(&own_cosigner) {
        return Err(anyhow!(
            "VTXO tree transaction does not include the renewal cosigner"
        ));
    }
    cosigners.sort_by_key(|key| key.serialize());
    let musig_keys = cosigners
        .iter()
        .map(|key| ark_core::conversions::to_musig_pk(*key))
        .collect::<Vec<_>>();
    let key_refs = musig_keys.iter().collect::<Vec<_>>();
    let key_agg = musig::KeyAggCache::new(&key_refs);
    let aggregate = ark_core::conversions::from_musig_xonly(key_agg.agg_pk());
    let sweep_script = ark_core::script::csv_sig_script(batch_expiry, forfeit_pk);
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let spend_info = TaprootBuilder::new()
        .add_leaf_with_ver(0, sweep_script, LeafVersion::TapScript)
        .map_err(|error| anyhow!("build VTXO tree sweep leaf: {error}"))?
        .finalize(&secp, aggregate)
        .map_err(|_| anyhow!("finalize VTXO tree sweep leaf"))?;
    Ok(ScriptBuf::new_p2tr_tweaked(spend_info.output_key()))
}

fn vtxo_tree_cosigners(psbt: &Psbt) -> Result<Vec<bitcoin::secp256k1::PublicKey>> {
    let input = psbt
        .inputs
        .first()
        .ok_or_else(|| anyhow!("VTXO tree PSBT omits input metadata"))?;
    let mut cosigners = Vec::new();
    let mut seen = HashSet::new();
    for (key, value) in &input.unknown {
        if !key.key.starts_with(&ark_core::VTXO_COSIGNER_PSBT_KEY) {
            continue;
        }
        let public = bitcoin::PublicKey::from_slice(value)
            .context("parse VTXO tree cosigner")?
            .inner;
        if !seen.insert(public) {
            return Err(anyhow!("VTXO tree PSBT contains a duplicate cosigner"));
        }
        cosigners.push(public);
    }
    if cosigners.is_empty() {
        return Err(anyhow!("VTXO tree PSBT has no cosigners"));
    }
    Ok(cosigners)
}

fn verify_vtxo_tree_signatures(graph: &TxGraph, commitment: &Psbt) -> Result<()> {
    let txs = graph.as_map();
    let secp = bitcoin::secp256k1::Secp256k1::verification_only();
    for (txid, psbt) in &txs {
        let signature = psbt.inputs[0]
            .tap_key_sig
            .as_ref()
            .ok_or_else(|| anyhow!("VTXO tree transaction {txid} is not fully signed"))?;
        if signature.sighash_type != TapSighashType::Default {
            return Err(anyhow!(
                "VTXO tree transaction {txid} uses a non-default sighash"
            ));
        }
        let outpoint = psbt.unsigned_tx.input[0].previous_output;
        let parent = if outpoint.txid == commitment.unsigned_tx.compute_txid() {
            &commitment.unsigned_tx
        } else {
            &txs.get(&outpoint.txid)
                .ok_or_else(|| anyhow!("VTXO tree parent {} is missing", outpoint.txid))?
                .unsigned_tx
        };
        let prevout = parent
            .output
            .get(outpoint.vout as usize)
            .ok_or_else(|| anyhow!("VTXO tree previous output {outpoint} is missing"))?;
        let script = prevout.script_pubkey.as_bytes();
        if script.len() != 34 || script[0] != 0x51 || script[1] != 0x20 {
            return Err(anyhow!("VTXO tree previous output {outpoint} is not P2TR"));
        }
        let output_key =
            XOnlyPublicKey::from_slice(&script[2..]).context("parse VTXO tree output key")?;
        let prevouts = [prevout];
        let sighash = SighashCache::new(&psbt.unsigned_tx)
            .taproot_key_spend_signature_hash(0, &Prevouts::All(&prevouts), TapSighashType::Default)
            .context("compute VTXO tree signature hash")?;
        let message = bitcoin::secp256k1::Message::from_digest(sighash.to_byte_array());
        secp.verify_schnorr(&signature.signature, &message, &output_key)
            .with_context(|| format!("verify VTXO tree signature {txid}"))?;
    }
    Ok(())
}

fn validate_connector_tree(
    chunks: &[TxGraphChunk],
    commitment: &Psbt,
    dust_sats: u64,
) -> Result<()> {
    validate_graph(chunks, commitment, 1, "connector tree")?;
    let mut leaf_count = 0_usize;
    for chunk in chunks {
        for output in chunk.unsigned_outputs_without_anchor()? {
            if output.value.to_sat() == 0 || !output.script_pubkey.is_p2tr() {
                return Err(anyhow!(
                    "connector tree contains a non-canonical spendable output"
                ));
            }
        }
        if chunk.children.is_empty() {
            leaf_count += 1;
            if chunk.tx.unsigned_tx.output.len() != 2
                || chunk.tx.unsigned_tx.output[0].value.to_sat() != dust_sats
                || !chunk.tx.unsigned_tx.output[0].script_pubkey.is_p2tr()
            {
                return Err(anyhow!("connector leaf is not one canonical dust output"));
            }
        }
    }
    if leaf_count != 1 {
        return Err(anyhow!(
            "renewal connector subtree must expose exactly one connector leaf"
        ));
    }
    Ok(())
}

trait TxGraphChunkOutputs {
    fn unsigned_outputs_without_anchor(&self) -> Result<&[TxOut]>;
}

impl TxGraphChunkOutputs for TxGraphChunk {
    fn unsigned_outputs_without_anchor(&self) -> Result<&[TxOut]> {
        self.tx
            .unsigned_tx
            .output
            .split_last()
            .map(|(_, outputs)| outputs)
            .ok_or_else(|| anyhow!("tree transaction has no outputs"))
    }
}

fn forfeit_sign_fn(
    rollover_keys: &Keys,
) -> impl Fn(
    &mut bitcoin::psbt::Input,
    bitcoin::secp256k1::Message,
) -> Result<
    Vec<(
        bitcoin::secp256k1::schnorr::Signature,
        bitcoin::XOnlyPublicKey,
    )>,
    ark_core::Error,
> + '_ {
    move |psbt_input, message| {
        let script = psbt_input.witness_script.clone().ok_or_else(|| {
            ark_core::Error::ad_hoc("missing witness script when signing rollover forfeit")
        })?;
        if ark_core::script::extract_checksig_pubkeys(&script).contains(&rollover_keys.owner_pk()) {
            Ok(rollover_keys.sign_msg(&message))
        } else {
            Ok(Vec::new())
        }
    }
}

/// Merge locally signed forfeits with the emulator's covenant signatures,
/// keyed by transaction so positional order is never trusted.
fn combine_forfeits(
    rollover_keys: &Keys,
    local: &[Psbt],
    emulator_signed: Vec<String>,
    emulator_pk: bitcoin::XOnlyPublicKey,
    arkade_script: &bitcoin::ScriptBuf,
) -> Result<Vec<Psbt>> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, arkade_script)
            .context("derive renewal emulator signer")?;

    let mut emulator_by_txid = HashMap::new();
    for encoded in emulator_signed {
        let psbt = Psbt::deserialize(&b64.decode(encoded).context("decode emulator forfeit")?)
            .context("parse emulator forfeit")?;
        let txid = psbt.unsigned_tx.compute_txid();
        if emulator_by_txid.insert(txid, psbt).is_some() {
            return Err(anyhow!("emulator returned duplicate forfeit {txid}"));
        }
    }

    let mut complete = Vec::with_capacity(local.len());
    for ours in local {
        let txid = ours.unsigned_tx.compute_txid();
        let theirs = emulator_by_txid
            .remove(&txid)
            .ok_or_else(|| anyhow!("emulator omitted renewal forfeit {txid}"))?;
        if ours.unsigned_tx != theirs.unsigned_tx {
            return Err(anyhow!("emulator changed renewal forfeit {txid}"));
        }
        let mut combined = ours.clone();
        combined
            .combine(theirs)
            .map_err(|error| anyhow!("combine renewal forfeit {txid}: {error}"))?;
        crate::txbuild::verified_signature_for_key(
            rollover_keys,
            ours,
            &combined,
            1,
            tweaked_emulator,
            "emulator",
        )?;
        crate::txbuild::verified_signature_for_key(
            rollover_keys,
            ours,
            &combined,
            1,
            rollover_keys.owner_pk(),
            "rollover",
        )?;
        if combined.inputs[1].tap_script_sigs.len() != 2 {
            return Err(anyhow!("rollover forfeit contains an unexpected signature"));
        }
        complete.push(combined);
    }
    if !emulator_by_txid.is_empty() {
        return Err(anyhow!("emulator returned unexpected forfeits"));
    }
    Ok(complete)
}

fn emulator_tree_node(chunk: &TxGraphChunk) -> EmulatorTxTreeNode {
    EmulatorTxTreeNode {
        txid: chunk
            .txid
            .unwrap_or_else(|| chunk.tx.unsigned_tx.compute_txid())
            .to_string(),
        tx: base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            chunk.tx.serialize(),
        ),
        children: chunk
            .children
            .iter()
            .map(|(vout, txid)| (*vout, txid.to_string()))
            .collect(),
    }
}

/// Open the arkd batch event stream with the same version/digest headers the
/// typed client uses, and return the raw response for buffered parsing.
async fn open_event_stream(
    ark_base: &str,
    topics: &[String],
    digest: &str,
) -> Result<reqwest::Response> {
    let response = reqwest::Client::new()
        .get(format!("{ark_base}/v1/batch/events"))
        .query(
            &topics
                .iter()
                .map(|topic| ("topics", topic))
                .collect::<Vec<_>>(),
        )
        .header("Accept", "text/event-stream")
        .header("X-Build-Version", ark_core::server::TARGET_ARKD_VERSION)
        .header("X-SDK-Version", ark_core::server::SDK_VERSION)
        .header("X-Digest", digest)
        .send()
        .await
        .context("connect to the arkd batch event stream")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("batch event stream rejected ({status}): {body}"));
    }
    Ok(response)
}

/// Pop one complete SSE frame (blank-line delimited) from the buffer. Both
/// LF and CRLF delimiters are accepted; the earliest one wins so frames of
/// mixed styles are never merged.
fn take_sse_frame(buffer: &mut String) -> Option<String> {
    let crlf = buffer.find("\r\n\r\n").map(|index| (index, 4));
    let lf = buffer.find("\n\n").map(|index| (index, 2));
    let (index, delimiter_len) = match (crlf, lf) {
        (Some(crlf), Some(lf)) => {
            if crlf.0 <= lf.0 {
                crlf
            } else {
                lf
            }
        }
        (Some(crlf), None) => crlf,
        (None, Some(lf)) => lf,
        (None, None) => return None,
    };
    let frame = buffer[..index].to_string();
    buffer.drain(..index + delimiter_len);
    Some(frame)
}

/// Decode one SSE frame into a batch event. Returns `None` for comments and
/// frames without data. The pinned arkd emits `treeNonces`, which the SDK's
/// own converter drops, so it is special-cased here.
fn parse_sse_event(frame: &str) -> Result<Option<StreamEvent>> {
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    let model: ark_rest::models::GetEventStreamResponse =
        serde_json::from_str(&data).context("parse batch event frame")?;
    if let Some(tree_nonces) = model.tree_nonces.clone() {
        let nonces = tree_nonces
            .nonces
            .ok_or_else(|| anyhow!("tree nonces event is missing its nonce map"))?;
        return Ok(Some(StreamEvent::TreeNonces(
            ark_core::server::TreeNoncesEvent {
                id: tree_nonces.id.unwrap_or_default(),
                topic: tree_nonces.topic.unwrap_or_default(),
                txid: tree_nonces
                    .txid
                    .ok_or_else(|| anyhow!("tree nonces event is missing its txid"))?
                    .parse()
                    .context("parse tree nonces txid")?,
                nonces: TreeTxNoncePks::decode(nonces)
                    .map_err(|error| anyhow!("decode tree nonces: {error}"))?,
            },
        )));
    }
    let event = StreamEvent::try_from(model)
        .map_err(|error| anyhow!("convert batch event frame: {error:?}"))?;
    Ok(Some(event))
}

#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_sse_frame(data: &[u8]) {
    if let Ok(frame) = std::str::from_utf8(data) {
        let _ = parse_sse_event(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_progress_reduces_only_valid_ordered_events() {
        let mut progress = BatchProgress::default();
        let commitment = Txid::from_byte_array([7; 32]);
        assert!(progress.is_start());
        assert!(!progress.accepts_tree_chunk("batch"));
        assert!(!progress.accepts_signing_start("batch"));
        assert!(progress.mark_tree_signatures_submitted("batch").is_err());

        progress.begin_batch("batch".to_owned()).unwrap();
        assert!(progress.begin_batch("batch".to_owned()).is_err());
        assert!(progress.accepts_tree_chunk("batch"));
        assert!(!progress.accepts_tree_chunk("other"));
        assert!(progress.accepts_signing_start("batch"));
        assert!(progress.mark_signing_started("other").is_err());
        progress.mark_signing_started("batch").unwrap();
        assert!(progress.accepts_tree_signature("batch"));
        assert!(progress.accepts_nonces("batch"));
        assert!(!progress.accepts_finalization("batch"));

        progress.mark_tree_signatures_submitted("batch").unwrap();
        assert!(progress.mark_tree_signatures_submitted("batch").is_err());
        assert!(!progress.accepts_nonces("batch"));
        assert!(progress.accepts_finalization("batch"));
        progress
            .mark_finalization_submitted("batch", commitment)
            .unwrap();
        assert!(progress
            .mark_finalization_submitted("batch", commitment)
            .is_err());
        assert!(!progress.finalize("other", commitment).unwrap());
        assert!(progress
            .finalize("batch", Txid::from_byte_array([8; 32]))
            .is_err());
        assert!(progress.finalize("batch", commitment).unwrap());
    }

    fn key(byte: u8) -> bitcoin::secp256k1::Keypair {
        let secp = bitcoin::secp256k1::Secp256k1::new();
        bitcoin::secp256k1::Keypair::from_secret_key(
            &secp,
            &bitcoin::secp256k1::SecretKey::from_slice(&[byte; 32]).unwrap(),
        )
    }

    fn p2tr_script(byte: u8) -> ScriptBuf {
        let secp = bitcoin::secp256k1::Secp256k1::new();
        ScriptBuf::new_p2tr(&secp, key(byte).x_only_public_key().0, None)
    }

    fn tree_psbt(previous_output: OutPoint, outputs: Vec<TxOut>, cosigners: &[u8]) -> Psbt {
        let mut psbt = Psbt::from_unsigned_tx(bitcoin::Transaction {
            version: bitcoin::transaction::Version::non_standard(3),
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output,
                sequence: bitcoin::Sequence::MAX,
                ..Default::default()
            }],
            output: outputs,
        })
        .unwrap();
        for (index, byte) in cosigners.iter().enumerate() {
            let mut raw_key = ark_core::VTXO_COSIGNER_PSBT_KEY.to_vec();
            raw_key.push(index as u8);
            psbt.inputs[0].unknown.insert(
                bitcoin::psbt::raw::Key {
                    type_value: 0xfc,
                    key: raw_key,
                },
                key(*byte).public_key().serialize().to_vec(),
            );
        }
        psbt
    }

    fn commitment(outputs: Vec<TxOut>) -> Psbt {
        Psbt::from_unsigned_tx(bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn::default()],
            output: outputs,
        })
        .unwrap()
    }

    #[test]
    fn batch_expiry_must_preserve_the_inputs_remaining_lifetime() {
        let expiry = bitcoin::Sequence::from_512_second_intervals(16);
        validate_batch_expiry(expiry, 17_999, 10_000).unwrap();
        validate_batch_expiry(expiry, 18_192, 10_000).unwrap();
        assert!(validate_batch_expiry(expiry, 18_193, 10_000).is_err());
        assert!(validate_batch_expiry(expiry, 10_000, 10_000).is_err());
        assert!(
            validate_batch_expiry(bitcoin::Sequence::from_height(144), 17_000, 10_000,).is_err()
        );
    }

    #[test]
    fn vtxo_tree_validation_binds_commitment_cosigners_and_exact_leaf() {
        let expiry = bitcoin::Sequence::from_height(144);
        let own_cosigner = key(7).public_key();
        let forfeit = key(9).x_only_public_key().0;
        let leaf_outputs = vec![
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: p2tr_script(10),
            },
            ark_core::extension::packet_txout(7, &[1, 2, 3]),
        ];
        let template = tree_psbt(
            OutPoint::null(),
            leaf_outputs
                .iter()
                .cloned()
                .chain([ark_core::anchor_output()])
                .collect(),
            &[7, 8],
        );
        let tree_script =
            expected_vtxo_tree_script(&template, expiry, forfeit, own_cosigner).unwrap();
        let commitment = commitment(vec![
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: tree_script,
            },
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: p2tr_script(11),
            },
        ]);
        let mut leaf = template;
        leaf.unsigned_tx.input[0].previous_output = OutPoint {
            txid: commitment.unsigned_tx.compute_txid(),
            vout: 0,
        };
        let chunk = TxGraphChunk {
            txid: Some(leaf.unsigned_tx.compute_txid()),
            tx: leaf,
            children: HashMap::new(),
        };

        let outpoint = validate_vtxo_tree(
            std::slice::from_ref(&chunk),
            &commitment,
            expiry,
            forfeit,
            own_cosigner,
            &[own_cosigner],
            &leaf_outputs,
        )
        .unwrap();
        assert_eq!(outpoint.vout, 0);

        let mut changed_leaf = chunk.clone();
        changed_leaf.tx.unsigned_tx.output[1] = ark_core::extension::packet_txout(7, &[1, 2, 4]);
        changed_leaf.txid = Some(changed_leaf.tx.unsigned_tx.compute_txid());
        assert!(validate_vtxo_tree(
            &[changed_leaf],
            &commitment,
            expiry,
            forfeit,
            own_cosigner,
            &[own_cosigner],
            &leaf_outputs,
        )
        .is_err());

        let mut changed_commitment = commitment.clone();
        changed_commitment.unsigned_tx.output[0].value = Amount::from_sat(331);
        assert!(validate_vtxo_tree(
            &[chunk],
            &changed_commitment,
            expiry,
            forfeit,
            own_cosigner,
            &[own_cosigner],
            &leaf_outputs,
        )
        .is_err());
    }

    #[test]
    fn graph_validation_accepts_filtered_siblings_and_rejects_cycles() {
        let commitment = commitment(vec![TxOut {
            value: Amount::from_sat(660),
            script_pubkey: p2tr_script(20),
        }]);
        let mut root = tree_psbt(
            OutPoint {
                txid: commitment.unsigned_tx.compute_txid(),
                vout: 0,
            },
            vec![
                TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: p2tr_script(21),
                },
                TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: p2tr_script(22),
                },
                ark_core::anchor_output(),
            ],
            &[],
        );
        let root_txid = root.unsigned_tx.compute_txid();
        let child = tree_psbt(
            OutPoint {
                txid: root_txid,
                vout: 0,
            },
            vec![
                TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: p2tr_script(23),
                },
                ark_core::anchor_output(),
            ],
            &[],
        );
        let child_txid = child.unsigned_tx.compute_txid();
        let missing_sibling = Txid::from_byte_array([42; 32]);
        let chunks = vec![
            TxGraphChunk {
                txid: Some(root_txid),
                tx: root.clone(),
                children: HashMap::from([(0, child_txid), (1, missing_sibling)]),
            },
            TxGraphChunk {
                txid: Some(child_txid),
                tx: child,
                children: HashMap::new(),
            },
        ];
        validate_graph(&chunks, &commitment, 0, "partial tree").unwrap();
        TxGraph::new(chunks).unwrap();

        root.unsigned_tx.output = vec![
            TxOut {
                value: Amount::from_sat(660),
                script_pubkey: p2tr_script(24),
            },
            ark_core::anchor_output(),
        ];
        let self_txid = root.unsigned_tx.compute_txid();
        let cyclic = TxGraphChunk {
            txid: Some(self_txid),
            tx: root,
            children: HashMap::from([(0, self_txid)]),
        };
        assert!(validate_graph(&[cyclic], &commitment, 0, "cyclic tree").is_err());
    }

    #[test]
    fn connector_validation_requires_commitment_output_one_and_one_dust_leaf() {
        let commitment = commitment(vec![
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: p2tr_script(12),
            },
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: p2tr_script(13),
            },
        ]);
        let connector = tree_psbt(
            OutPoint {
                txid: commitment.unsigned_tx.compute_txid(),
                vout: 1,
            },
            vec![
                TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: p2tr_script(14),
                },
                ark_core::anchor_output(),
            ],
            &[],
        );
        let chunk = TxGraphChunk {
            txid: Some(connector.unsigned_tx.compute_txid()),
            tx: connector,
            children: HashMap::new(),
        };
        validate_connector_tree(std::slice::from_ref(&chunk), &commitment, 330).unwrap();

        let mut wrong_root = chunk;
        wrong_root.tx.unsigned_tx.input[0].previous_output.vout = 0;
        wrong_root.txid = Some(wrong_root.tx.unsigned_tx.compute_txid());
        assert!(validate_connector_tree(&[wrong_root], &commitment, 330).is_err());
    }

    #[test]
    fn sse_frames_split_on_blank_lines_across_chunk_boundaries() {
        let mut buffer = "data: {\"heartbea".to_string();
        assert!(take_sse_frame(&mut buffer).is_none());
        buffer.push_str("t\":{}}\n\ndata: {\"streamStarted\":{\"id\":\"s\"}}\r\n\r\n");
        assert_eq!(
            take_sse_frame(&mut buffer).as_deref(),
            Some("data: {\"heartbeat\":{}}")
        );
        assert_eq!(
            take_sse_frame(&mut buffer).as_deref(),
            Some("data: {\"streamStarted\":{\"id\":\"s\"}}")
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn sse_parser_handles_heartbeat_and_stream_started() {
        assert!(parse_sse_event("data: {\"heartbeat\":{}}")
            .unwrap()
            .is_some());
        assert!(parse_sse_event(": comment\n\ndata: {\"heartbeat\":{}}")
            .unwrap()
            .is_some());
        let event = parse_sse_event("data: {\"streamStarted\":{\"id\":\"abc\"}}")
            .unwrap()
            .unwrap();
        assert!(matches!(event, StreamEvent::StreamStarted(_)));
    }

    #[test]
    fn sse_parser_decodes_tree_nonces_the_sdk_converter_drops() {
        let mut nonces = std::collections::HashMap::new();
        let secp = bitcoin::secp256k1::Secp256k1::new();
        let keypair = bitcoin::secp256k1::Keypair::from_secret_key(
            &secp,
            &bitcoin::secp256k1::SecretKey::from_slice(&[7; 32]).unwrap(),
        );
        let xonly = keypair.x_only_public_key().0.to_string();
        // A syntactically valid 66-byte public nonce is enough for decode.
        nonces.insert(xonly, "02".repeat(66));
        let frame = format!(
            "data: {{\"treeNonces\":{{\"id\":\"b\",\"txid\":\"{}\",\"topic\":[\"t\"],\"nonces\":{}}}}}",
            "11".repeat(32),
            serde_json::to_string(&nonces).unwrap()
        );
        let event = parse_sse_event(&frame).unwrap().unwrap();
        let StreamEvent::TreeNonces(event) = event else {
            panic!("expected tree nonces event");
        };
        assert_eq!(event.id, "b");
        assert_eq!(event.nonces.0.len(), 1);
    }
}
