//! Block-aware policy gate in front of the pinned Arkade emulator.
//!
//! The upstream emulator remains unmodified and binds only to loopback. This
//! public gate validates Woodland block-attestation witnesses against a local
//! Bitcoin Core node before forwarding requests to the signer. The witness is
//! serialized in the transaction's introspector extension, so the emulator
//! signature commits the height validated by the gate.

use crate::arkade::BlockTip;
use crate::tree::BLOCK_ATTESTATION_MARKER;
use anyhow::{anyhow, bail, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use bitcoin::script::{read_scriptint, write_scriptint};
use bitcoin::{BlockHash, Psbt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

const DEFAULT_BIND: &str = "127.0.0.1:7074";
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_ATTESTATION_STALENESS: u32 = 0;

#[derive(Clone)]
struct BitcoinRpc {
    inner: Arc<BitcoinRpcInner>,
}

struct BitcoinRpcInner {
    client: reqwest::Client,
    url: reqwest::Url,
    user: String,
    password: String,
    next_id: AtomicU64,
}

#[derive(Clone)]
struct GateState {
    client: reqwest::Client,
    upstream: reqwest::Url,
    bitcoin: BitcoinRpc,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmitTxRequest {
    ark_tx: String,
    checkpoint_txs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IntentWire {
    proof: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmitIntentRequest {
    intent: IntentWire,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    ready: bool,
    block_tip: BlockTip,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

struct GateError {
    status: StatusCode,
    message: String,
}

impl GateError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn upstream(error: impl std::fmt::Display) -> Self {
        eprintln!("woodland.sh emulator gate upstream: {error}");
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: "emulator gate upstream unavailable".to_owned(),
        }
    }
}

impl IntoResponse for GateError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: &'a [serde_json::Value],
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<serde_json::Value>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

impl BitcoinRpc {
    fn new(url: reqwest::Url, user: String, password: String) -> Result<Self> {
        if user.is_empty() || password.is_empty() {
            bail!("Bitcoin RPC credentials must not be empty");
        }
        Ok(Self {
            inner: Arc::new(BitcoinRpcInner {
                client: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .context("build Bitcoin RPC client")?,
                url,
                user,
                password,
                next_id: AtomicU64::new(1),
            }),
        })
    }

    async fn call(&self, method: &str, params: &[serde_json::Value]) -> Result<serde_json::Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::to_vec(&RpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        })?;
        let response = self
            .inner
            .client
            .post(self.inner.url.clone())
            .basic_auth(&self.inner.user, Some(&self.inner.password))
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| format!("call Bitcoin RPC {method}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("read Bitcoin RPC {method} response"))?;
        if !status.is_success() {
            bail!("Bitcoin RPC {method} returned HTTP {status}");
        }
        let response: RpcResponse = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode Bitcoin RPC {method} response"))?;
        if let Some(error) = response.error {
            bail!(
                "Bitcoin RPC {method} failed with {}: {}",
                error.code,
                error.message
            );
        }
        response
            .result
            .ok_or_else(|| anyhow!("Bitcoin RPC {method} omitted its result"))
    }

    async fn block_tip(&self) -> Result<BlockTip> {
        let value = self.call("getblockchaininfo", &[]).await?;
        let height = value
            .get("blocks")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow!("Bitcoin RPC getblockchaininfo omitted blocks"))?;
        let height = u32::try_from(height).context("Bitcoin block height exceeds u32")?;
        let block_hash = value
            .get("bestblockhash")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("Bitcoin RPC getblockchaininfo omitted bestblockhash"))?;
        Ok(BlockTip {
            height,
            block_hash: BlockHash::from_str(block_hash).context("parse Bitcoin best block hash")?,
        })
    }
}

fn decode_psbt(encoded: &str) -> Result<Psbt> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("decode PSBT base64")?;
    Psbt::deserialize(&raw).context("decode PSBT")
}

fn marked_height(witness: &bitcoin::Witness) -> Result<Option<u32>> {
    let items = witness.iter().collect::<Vec<_>>();
    if items.last().copied() != Some(BLOCK_ATTESTATION_MARKER.as_slice()) {
        return Ok(None);
    }
    if items.len() != 2 {
        bail!("block attestation witness must contain height and marker");
    }
    let height = read_scriptint(items[0]).context("decode block attestation height")?;
    if height <= 0 {
        bail!("block attestation height must be positive");
    }
    let height = u32::try_from(height).context("block attestation height exceeds u32")?;
    let mut canonical = [0_u8; 8];
    let len = write_scriptint(&mut canonical, i64::from(height));
    if canonical[..len] != *items[0] {
        bail!("block attestation height is not minimally encoded");
    }
    Ok(Some(height))
}

fn validate_attested_psbt(psbt: &Psbt, tip_height: u32) -> Result<()> {
    let Some(packet) = ark_core::introspector::packet::find_packet(&psbt.unsigned_tx)
        .context("decode emulator packet")?
    else {
        return Ok(());
    };
    let mut marked = 0_usize;
    for entry in packet.entries {
        let Some(height) = marked_height(&entry.witness)? else {
            continue;
        };
        marked += 1;
        let age = tip_height
            .checked_sub(height)
            .ok_or_else(|| anyhow!("block attestation height is ahead of Bitcoin tip"))?;
        if age > MAX_ATTESTATION_STALENESS {
            bail!("block attestation height {height} is stale at Bitcoin tip {tip_height}");
        }
    }
    if marked > 1 {
        bail!("transaction contains multiple block attestation witnesses");
    }
    Ok(())
}

impl GateState {
    async fn forward(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<Response, GateError> {
        let url = self
            .upstream
            .join(path.trim_start_matches('/'))
            .map_err(GateError::upstream)?;
        let mut request = self.client.request(method, url);
        if !body.is_empty() {
            request = request.header(CONTENT_TYPE, "application/json").body(body);
        }
        let response = request.send().await.map_err(GateError::upstream)?;
        let status =
            StatusCode::from_u16(response.status().as_u16()).map_err(GateError::upstream)?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .cloned();
        let bytes = response.bytes().await.map_err(GateError::upstream)?;
        let mut response = Response::builder().status(status);
        if let Some(content_type) = content_type {
            response = response.header(CONTENT_TYPE, content_type);
        }
        response
            .body(Body::from(bytes))
            .map_err(GateError::upstream)
    }
}

async fn block_tip(State(state): State<Arc<GateState>>) -> Result<Json<BlockTip>, GateError> {
    state
        .bitcoin
        .block_tip()
        .await
        .map(Json)
        .map_err(GateError::upstream)
}

async fn health(State(state): State<Arc<GateState>>) -> Result<Json<HealthResponse>, GateError> {
    let tip = state
        .bitcoin
        .block_tip()
        .await
        .map_err(GateError::upstream)?;
    let url = state
        .upstream
        .join("v1/info")
        .map_err(GateError::upstream)?;
    let response = state
        .client
        .get(url)
        .send()
        .await
        .map_err(GateError::upstream)?;
    if !response.status().is_success() {
        return Err(GateError::upstream(format!(
            "emulator /v1/info returned {}",
            response.status()
        )));
    }
    Ok(Json(HealthResponse {
        ready: true,
        block_tip: tip,
    }))
}

async fn info(State(state): State<Arc<GateState>>) -> Result<Response, GateError> {
    state.forward(Method::GET, "/v1/info", Bytes::new()).await
}

async fn submit_tx(
    State(state): State<Arc<GateState>>,
    body: Bytes,
) -> Result<Response, GateError> {
    let request: SubmitTxRequest = serde_json::from_slice(&body).map_err(GateError::bad_request)?;
    let _ = request.checkpoint_txs.len();
    let psbt = decode_psbt(&request.ark_tx).map_err(GateError::bad_request)?;
    let tip = state
        .bitcoin
        .block_tip()
        .await
        .map_err(GateError::upstream)?;
    validate_attested_psbt(&psbt, tip.height).map_err(GateError::bad_request)?;
    state.forward(Method::POST, "/v1/tx", body).await
}

async fn submit_intent(
    State(state): State<Arc<GateState>>,
    body: Bytes,
) -> Result<Response, GateError> {
    let request: SubmitIntentRequest =
        serde_json::from_slice(&body).map_err(GateError::bad_request)?;
    let _ = request.intent.message.len();
    let psbt = decode_psbt(&request.intent.proof).map_err(GateError::bad_request)?;
    let tip = state
        .bitcoin
        .block_tip()
        .await
        .map_err(GateError::upstream)?;
    validate_attested_psbt(&psbt, tip.height).map_err(GateError::bad_request)?;
    state.forward(Method::POST, "/v1/intent", body).await
}

async fn finalization(
    State(state): State<Arc<GateState>>,
    body: Bytes,
) -> Result<Response, GateError> {
    state.forward(Method::POST, "/v1/finalization", body).await
}

async fn onchain_tx(
    State(state): State<Arc<GateState>>,
    body: Bytes,
) -> Result<Response, GateError> {
    state.forward(Method::POST, "/v1/onchain-tx", body).await
}

fn setting(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set"))
}

fn canonical_url(value: &str, name: &str, loopback_only: bool) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).with_context(|| format!("parse {name}"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "::1"));
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || (loopback_only && !loopback)
        || (url.scheme() == "http" && !loopback)
    {
        bail!("{name} must be a canonical loopback HTTP or authenticated HTTPS origin");
    }
    Ok(url)
}

async fn no_store(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn run_cli() -> Result<()> {
    let bind = std::env::var("WOODLAND_EMULATOR_GATE_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_owned())
        .parse::<SocketAddr>()
        .context("parse WOODLAND_EMULATOR_GATE_BIND")?;
    let upstream = canonical_url(
        &setting("WOODLAND_EMULATOR_UPSTREAM_URL")?,
        "WOODLAND_EMULATOR_UPSTREAM_URL",
        true,
    )?;
    let bitcoin_url = canonical_url(
        &setting("WOODLAND_BITCOIN_RPC_URL")?,
        "WOODLAND_BITCOIN_RPC_URL",
        false,
    )?;
    let origin = canonical_url(
        &setting("WOODLAND_EMULATOR_GATE_ORIGIN")?,
        "WOODLAND_EMULATOR_GATE_ORIGIN",
        false,
    )?;
    let origin = HeaderValue::from_str(origin.origin().ascii_serialization().as_str())
        .context("encode emulator gate CORS origin")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build emulator upstream client")?;
    let state = Arc::new(GateState {
        client,
        upstream,
        bitcoin: BitcoinRpc::new(
            bitcoin_url,
            setting("WOODLAND_BITCOIN_RPC_USER")?,
            setting("WOODLAND_BITCOIN_RPC_PASSWORD")?,
        )?,
    });

    let app = Router::new()
        .route("/health.json", get(health))
        .route("/v1/block-tip", get(block_tip))
        .route("/v1/info", get(info))
        .route("/v1/tx", post(submit_tx))
        .route("/v1/intent", post(submit_intent))
        .route("/v1/finalization", post(finalization))
        .route("/v1/onchain-tx", post(onchain_tx))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(
            CorsLayer::new()
                .allow_origin(origin)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([CONTENT_TYPE]),
        )
        .layer(axum::middleware::from_fn(no_store))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind woodland.sh emulator gate at {bind}"))?;
    eprintln!("woodland.sh emulator gate ready at http://{bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve woodland.sh emulator gate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_core::introspector::packet::{add_packet_to_psbt, IntrospectorEntry, Packet};
    use bitcoin::transaction::Version;
    use bitcoin::{Amount, ScriptBuf, Transaction, TxIn, TxOut, Witness};

    fn psbt_with_witnesses(witnesses: Vec<Witness>) -> Psbt {
        let inputs = witnesses
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let byte = u8::try_from(index + 1).unwrap();
                let txid = bitcoin::Txid::from_str(&format!("{byte:02x}").repeat(32)).unwrap();
                TxIn {
                    previous_output: bitcoin::OutPoint::new(txid, 0),
                    ..TxIn::default()
                }
            })
            .collect();
        let tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: inputs,
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let entries = witnesses
            .into_iter()
            .enumerate()
            .map(|(vin, witness)| IntrospectorEntry {
                vin: u16::try_from(vin).unwrap(),
                script: ScriptBuf::from_bytes(vec![0x51]),
                witness,
            })
            .collect();
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        add_packet_to_psbt(&mut psbt, &Packet::new(entries).unwrap()).unwrap();
        psbt
    }

    fn psbt(witness: Witness) -> Psbt {
        psbt_with_witnesses(vec![witness])
    }

    #[test]
    fn accepts_only_current_tip_attestations() {
        let current = psbt(crate::tree::block_attestation_witness(100).unwrap());
        assert_eq!(
            current.unsigned_tx.lock_time,
            bitcoin::absolute::LockTime::ZERO
        );
        validate_attested_psbt(&current, 100).unwrap();
        let previous = psbt(crate::tree::block_attestation_witness(99).unwrap());
        assert!(validate_attested_psbt(&previous, 100).is_err());
    }

    #[test]
    fn rejects_future_and_stale_attestations() {
        let future = psbt(crate::tree::block_attestation_witness(101).unwrap());
        assert!(validate_attested_psbt(&future, 100).is_err());
        let stale = psbt(crate::tree::block_attestation_witness(98).unwrap());
        assert!(validate_attested_psbt(&stale, 100).is_err());
    }

    #[test]
    fn rejects_malformed_marked_witness() {
        let malformed = psbt(Witness::from_slice(&[BLOCK_ATTESTATION_MARKER]));
        assert!(validate_attested_psbt(&malformed, 100).is_err());
    }

    #[test]
    fn rejects_nonminimal_and_duplicate_attestations() {
        let nonminimal = psbt(Witness::from_slice(&[
            vec![100_u8, 0],
            BLOCK_ATTESTATION_MARKER.to_vec(),
        ]));
        assert!(validate_attested_psbt(&nonminimal, 100).is_err());

        let witness = crate::tree::block_attestation_witness(100).unwrap();
        let duplicate = psbt_with_witnesses(vec![witness.clone(), witness]);
        assert!(validate_attested_psbt(&duplicate, 100).is_err());
    }

    #[test]
    fn allows_unmarked_non_woodland_requests() {
        validate_attested_psbt(&psbt(Witness::new()), 100).unwrap();
    }
}
