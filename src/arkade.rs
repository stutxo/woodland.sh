//! Minimal Arkade and emulator REST clients used by the tree harness.

use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use ark_core::Asset;
use bitcoin::{OutPoint, ScriptBuf, Transaction, Txid, XOnlyPublicKey};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::JsFuture;

const REQUEST_TIMEOUT_MS: i32 = 15_000;
const MAX_INDEX_PAGES: usize = 128;
#[cfg(not(target_arch = "wasm32"))]
const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_COMMITMENT_BATCH_VOUTS: usize = 1_024;
const MAX_INDEX_RECORDS: usize = 100_000;
const MAX_VIRTUAL_TXS_PER_REQUEST: usize = 50;
const VIRTUAL_TX_REQUEST_CONCURRENCY: usize = 8;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const MAX_VTXO_OUTPOINTS_PER_REQUEST: usize = 50;
const EXACT_VTXO_REQUEST_CONCURRENCY: usize = 2;

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Default)]
struct PlatformClient;

#[cfg(not(target_arch = "wasm32"))]
type PlatformClient = reqwest::Client;

#[cfg(target_arch = "wasm32")]
fn platform_client() -> PlatformClient {
    PlatformClient
}

#[cfg(not(target_arch = "wasm32"))]
fn platform_client() -> PlatformClient {
    reqwest::Client::new()
}

#[derive(Clone, Debug)]
pub struct ServerParams {
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub version: String,
    pub signer_pk: XOnlyPublicKey,
    pub forfeit_pk: bitcoin::PublicKey,
    pub network: bitcoin::Network,
    pub dust_sats: u64,
    pub vtxo_min_sats: u64,
    pub unilateral_exit_delay: bitcoin::Sequence,
    pub max_tx_weight: i64,
    pub max_op_return_outputs: i64,
    pub zero_offchain_fees: bool,
    pub checkpoint_tapscript: bitcoin::ScriptBuf,
    pub forfeit_address: bitcoin::Address,
}

#[derive(Clone, Debug)]
pub struct VtxoRecord {
    pub outpoint: OutPoint,
    /// Typed PkScript of the VTXO (P2TR, or OP_RETURN for sub-dust).
    pub script: ScriptBuf,
    pub amount_sats: u64,
    pub assets: Vec<Asset>,
    /// Server wall-clock creation time in Unix seconds. Zero is normalized to
    /// `None`; a missing value fails closed in [`Self::ensure_live`].
    pub created_at: Option<i64>,
    /// Projected expiry in Unix seconds. Offchain descendants inherit the
    /// earliest expiry of their inputs, so every cooperative input must pass
    /// [`Self::ensure_live`] before a transition is attempted.
    pub expires_at: Option<i64>,
    pub is_preconfirmed: bool,
    pub is_swept: bool,
    pub spent_by: Option<Txid>,
    pub settled_by: Option<Txid>,
    pub is_unrolled: bool,
    pub is_spent: bool,
}

/// Default safety margin applied before a VTXO's expiry. A cooperative
/// transaction started inside this window could inherit an expiry the current
/// batch can no longer outlive, so clients fail closed instead.
pub const DEFAULT_EXPIRY_MARGIN_SECS: i64 = 300;
pub const MIN_ROLLOVER_MARGIN_SECS: i64 = DEFAULT_EXPIRY_MARGIN_SECS * 2;
/// The window must cover shard drain time plus operator failover, so a
/// correlated world-scale renewal wave or a multi-hour maintenance outage
/// still fits inside the margin on long-lived mainnet batches.
pub const MAX_ROLLOVER_MARGIN_SECS: i64 = 43_200;

impl VtxoRecord {
    /// Remaining lifetime in seconds, if the indexer reported an expiry.
    pub fn expires_in(&self, now_unix: i64) -> Option<i64> {
        self.expires_at.map(|expires| expires - now_unix)
    }
    /// Renew around halfway through the observed VTXO lifetime, bounded so
    /// short-lived public test networks do not loop continuously and long-lived
    /// mainnet batches retain an operationally useful margin.
    pub fn rollover_margin_seconds(&self) -> i64 {
        match (self.created_at, self.expires_at) {
            (Some(created), Some(expires)) if expires > created => {
                ((expires - created) / 2).clamp(MIN_ROLLOVER_MARGIN_SECS, MAX_ROLLOVER_MARGIN_SECS)
            }
            _ => MAX_ROLLOVER_MARGIN_SECS,
        }
    }

    pub fn asset_amount(&self, asset_id: AssetId) -> Option<u64> {
        self.assets
            .iter()
            .find(|asset| asset.asset_id == asset_id)
            .map(|asset| asset.amount)
    }

    /// Verify the indexer's script, sats, and asset summary against the exact
    /// creating transaction before this record is displayed or spent.
    pub fn validate_creating_transaction(&self, transaction: &Transaction) -> Result<()> {
        if transaction.compute_txid() != self.outpoint.txid {
            return Err(anyhow!(
                "creating transaction ID does not match indexed VTXO"
            ));
        }
        let output = transaction
            .output
            .get(self.outpoint.vout as usize)
            .ok_or_else(|| anyhow!("creating transaction omitted indexed VTXO output"))?;
        if output.script_pubkey != self.script || output.value.to_sat() != self.amount_sats {
            return Err(anyhow!(
                "creating transaction output does not match indexed VTXO"
            ));
        }
        let packet_assets = crate::asset_packet::output_assets(transaction, self.outpoint.vout)
            .context("decode creating transaction Asset V1 assignments")?;
        if !crate::asset_packet::equal_asset_sets(&self.assets, &packet_assets) {
            return Err(anyhow!(
                "indexed VTXO assets do not match creating transaction assignments"
            ));
        }
        Ok(())
    }

    /// Fail closed unless this record is a live cooperative input with more
    /// than `margin_secs` of lifetime remaining at `now_unix`.
    pub fn ensure_live(&self, now_unix: i64, margin_secs: i64) -> Result<()> {
        if self.is_spent {
            return Err(anyhow!("VTXO {} is already spent", self.outpoint));
        }
        if self.is_swept {
            return Err(anyhow!("VTXO {} was swept", self.outpoint));
        }
        if self.is_unrolled {
            return Err(anyhow!("VTXO {} is being unrolled", self.outpoint));
        }
        if margin_secs < 0 {
            return Err(anyhow!("expiry margin cannot be negative"));
        }
        let created_at = self
            .created_at
            .ok_or_else(|| anyhow!("VTXO {} has no creation time", self.outpoint))?;
        let expires_at = self
            .expires_at
            .ok_or_else(|| anyhow!("VTXO {} has no expiry", self.outpoint))?;
        if expires_at <= created_at {
            return Err(anyhow!(
                "VTXO {} has a malformed expiry {expires_at} (created {created_at})",
                self.outpoint
            ));
        }
        let deadline = now_unix
            .checked_add(margin_secs)
            .ok_or_else(|| anyhow!("expiry deadline overflow"))?;
        if expires_at <= deadline {
            return Err(anyhow!(
                "VTXO {} expires too soon ({expires_at} <= {deadline})",
                self.outpoint
            ));
        }
        Ok(())
    }
}

/// Current Unix time for liveness checks on either target.
pub fn now_unix() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        (js_sys::Date::now() / 1000.0) as i64
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
pub struct VtxoPage {
    pub vtxos: Vec<VtxoRecord>,
    pub current: i32,
    pub next: i32,
    pub total: i32,
}
fn next_vtxo_page(
    page: &VtxoPage,
    requested: i32,
    visited: &mut std::collections::HashSet<i32>,
) -> Result<Option<i32>> {
    if page.current != requested {
        return Err(anyhow!(
            "indexer returned page {} for requested page {requested}",
            page.current
        ));
    }
    if page.next <= 0
        || (page.next == page.current && (page.total <= 0 || page.current >= page.total))
    {
        return Ok(None);
    }
    if page.next <= page.current || (page.total > 0 && page.next > page.total) {
        return Err(anyhow!(
            "indexer returned invalid page cursor {} -> {} of {}",
            page.current,
            page.next,
            page.total
        ));
    }
    if !visited.insert(page.next) || visited.len() > MAX_INDEX_PAGES {
        return Err(anyhow!(
            "indexer pagination repeated or exceeded its page limit"
        ));
    }
    Ok(Some(page.next))
}

fn merge_vtxo_records(
    records: &mut Vec<VtxoRecord>,
    seen: &mut std::collections::HashSet<OutPoint>,
    page: Vec<VtxoRecord>,
) -> Result<()> {
    for record in page {
        if seen.insert(record.outpoint) {
            if records.len() >= MAX_INDEX_RECORDS {
                return Err(anyhow!("indexer result exceeds the VTXO safety limit"));
            }
            records.push(record);
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct ArkadeRest {
    base: String,
    client: PlatformClient,
}

#[derive(Clone)]
pub struct EmulatorRest {
    base: String,
    client: PlatformClient,
}

#[derive(Clone, Debug)]
pub struct EmulatorParams {
    pub version: String,
    pub signer_pk: XOnlyPublicKey,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
struct InfoResponse {
    version: String,
    #[serde(rename = "signerPubkey")]
    signer_pubkey: String,
    #[serde(rename = "forfeitPubkey")]
    forfeit_pubkey: Option<String>,
    network: String,
    dust: String,
    #[serde(rename = "vtxoMinAmount")]
    vtxo_min_amount: String,
    #[serde(rename = "unilateralExitDelay")]
    unilateral_exit_delay: String,
    #[serde(rename = "maxOpReturnOutputs")]
    max_op_return_outputs: String,
    #[serde(rename = "maxTxWeight")]
    max_tx_weight: String,
    #[serde(rename = "checkpointTapscript")]
    checkpoint_tapscript: String,
    #[serde(rename = "forfeitAddress")]
    forfeit_address: String,
    #[serde(default)]
    fees: Option<InfoFeesResponse>,
    #[serde(default, rename = "scheduledSession")]
    scheduled_session: Option<InfoScheduledSessionResponse>,
}
#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoIntentFeeResponse {
    offchain_input: Option<String>,
    offchain_output: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoFeesResponse {
    intent_fee: InfoIntentFeeResponse,
}

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoScheduledSessionResponse {
    fees: Option<InfoFeesResponse>,
}

#[derive(Deserialize)]
struct GetVtxosResponse {
    vtxos: Option<Vec<IndexerVtxo>>,
    page: Option<IndexerPage>,
}

#[derive(Deserialize)]
struct GetCommitmentTxResponse {
    #[serde(default)]
    batches: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct GetVtxoTreeLeavesResponse {
    leaves: Option<Vec<IndexerOutpoint>>,
    page: Option<IndexerPage>,
}

#[derive(Deserialize)]
struct IndexerPage {
    current: Option<i32>,
    next: Option<i32>,
    total: Option<i32>,
}

#[derive(Deserialize, serde::Serialize)]
struct IndexerVtxo {
    outpoint: Option<IndexerOutpoint>,
    script: Option<String>,
    amount: Option<String>,
    assets: Option<Vec<IndexerAsset>>,
    #[serde(rename = "createdAt")]
    created_at: Option<I64Wire>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<I64Wire>,
    #[serde(rename = "isPreconfirmed", default)]
    is_preconfirmed: bool,
    #[serde(rename = "isSwept", default)]
    is_swept: bool,
    #[serde(rename = "isUnrolled", default)]
    is_unrolled: bool,
    #[serde(rename = "isSpent", default)]
    is_spent: bool,
    #[serde(rename = "arkTxid")]
    spent_by: Option<String>,
    #[serde(rename = "settledBy")]
    settled_by: Option<String>,
}

/// Protobuf JSON encodes int64 as a decimal string, while the generated
/// OpenAPI document declares an integer. Accept both, rejecting fractions,
/// exponents, and out-of-range values.
#[derive(Clone, Copy, Debug)]
struct I64Wire(i64);

impl serde::Serialize for I64Wire {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for I64Wire {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = I64Wire;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a decimal string or integer")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<I64Wire, E>
            where
                E: serde::de::Error,
            {
                value
                    .parse::<i64>()
                    .map(I64Wire)
                    .map_err(serde::de::Error::custom)
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<I64Wire, E>
            where
                E: serde::de::Error,
            {
                Ok(I64Wire(value))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<I64Wire, E>
            where
                E: serde::de::Error,
            {
                i64::try_from(value)
                    .map(I64Wire)
                    .map_err(serde::de::Error::custom)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Deserialize, serde::Serialize)]
struct IndexerOutpoint {
    txid: String,
    vout: u32,
}

#[derive(Deserialize, serde::Serialize)]
struct IndexerAsset {
    #[serde(rename = "assetId")]
    asset_id: String,
    amount: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetDetailsResponse {
    asset_id: String,
    supply: String,
    metadata: String,
    control_asset: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetDetails {
    pub asset_id: AssetId,
    pub supply: u64,
    pub metadata: Vec<u8>,
    pub control_asset: Option<AssetId>,
}

#[derive(Deserialize)]
struct GetVirtualTxsResponse {
    txs: Option<Vec<String>>,
}

#[derive(serde::Serialize)]
struct SubmitTxRequest<'a> {
    #[serde(rename = "signedArkTx")]
    signed_ark_tx: &'a str,
    #[serde(rename = "checkpointTxs")]
    checkpoint_txs: Vec<String>,
}

#[derive(Deserialize)]
struct SubmitTxResponse {
    #[serde(rename = "arkTxid")]
    ark_txid: Option<String>,
    #[serde(rename = "finalArkTx")]
    final_ark_tx: Option<String>,
    #[serde(rename = "signedCheckpointTxs")]
    signed_checkpoint_txs: Option<Vec<String>>,
}

#[derive(serde::Serialize)]
struct EmulatorSubmitTxRequest<'a> {
    #[serde(rename = "arkTx")]
    ark_tx: &'a str,
    #[serde(rename = "checkpointTxs")]
    checkpoint_txs: Vec<String>,
}

#[derive(Deserialize)]
struct EmulatorSubmitTxResponse {
    #[serde(rename = "signedArkTx")]
    signed_ark_tx: Option<String>,
    #[serde(rename = "signedCheckpointTxs")]
    signed_checkpoint_txs: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct EmulatorInfoResponse {
    version: Option<String>,
    #[serde(rename = "signerPubkey")]
    signer_pubkey: String,
}

#[derive(serde::Serialize)]
struct EmulatorIntentWire<'a> {
    proof: &'a str,
    message: &'a str,
}

#[derive(serde::Serialize)]
struct EmulatorSubmitIntentRequest<'a> {
    intent: EmulatorIntentWire<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmulatorSubmitIntentResponse {
    signed_proof: Option<String>,
}

/// One flat connector-tree node in the emulator's finalization wire format.
#[derive(Clone, Debug, serde::Serialize)]
pub struct EmulatorTxTreeNode {
    pub txid: String,
    pub tx: String,
    pub children: std::collections::BTreeMap<u32, String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EmulatorSubmitFinalizationRequest<'a> {
    signed_intent: EmulatorIntentWire<'a>,
    forfeits: Vec<String>,
    connector_tree: Vec<EmulatorTxTreeNode>,
    commitment_tx: &'a str,
}

/// The emulator's finalization countersignatures for one approved intent.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmulatorFinalization {
    #[serde(default)]
    pub signed_forfeits: Vec<String>,
    #[serde(default)]
    pub signed_commitment_tx: Option<String>,
}

#[derive(serde::Serialize)]
struct FinalizeTxRequest<'a> {
    #[serde(rename = "arkTxid")]
    ark_txid: &'a str,
    #[serde(rename = "finalCheckpointTxs")]
    final_checkpoint_txs: Vec<String>,
}

#[derive(Clone, Copy)]
enum FetchCache {
    Default,
    NoStore,
}

#[derive(Debug)]
pub(crate) enum HttpFailure {
    Transport {
        method: String,
        url: String,
        message: String,
    },
    Status {
        method: String,
        url: String,
        status: u16,
        body: String,
    },
}

impl HttpFailure {
    fn transport(method: &str, url: &str, message: impl Into<String>) -> Self {
        Self::Transport {
            method: method.to_owned(),
            url: url.to_owned(),
            message: message.into(),
        }
    }

    fn status(method: &str, url: &str, status: u16, body: String) -> Self {
        Self::Status {
            method: method.to_owned(),
            url: url.to_owned(),
            status,
            body,
        }
    }

    pub(crate) fn status_code(&self) -> Option<u16> {
        match self {
            Self::Transport { .. } => None,
            Self::Status { status, .. } => Some(*status),
        }
    }
}

impl std::fmt::Display for HttpFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport {
                method,
                url,
                message,
            } => write!(formatter, "{method} {url} transport failed: {message}"),
            Self::Status {
                method,
                url,
                status,
                body,
            } => write!(formatter, "{method} {url} failed ({status}): {body}"),
        }
    }
}

impl std::error::Error for HttpFailure {}

#[cfg(target_arch = "wasm32")]
async fn fetch_text(
    _client: &PlatformClient,
    method: &str,
    url: &str,
    body: Option<String>,
    cache: FetchCache,
) -> std::result::Result<String, HttpFailure> {
    let window = web_sys::window()
        .ok_or_else(|| HttpFailure::transport(method, url, "no browser window"))?;
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if matches!(cache, FetchCache::NoStore) {
        init.set_cache(web_sys::RequestCache::NoStore);
    }
    let signal = web_sys::AbortSignal::timeout_with_u32(REQUEST_TIMEOUT_MS as u32);
    init.set_signal(Some(&signal));
    if let Some(body) = body {
        let headers = web_sys::Headers::new()
            .map_err(|error| HttpFailure::transport(method, url, format!("headers: {error:?}")))?;
        headers
            .set("content-type", "application/json")
            .map_err(|error| HttpFailure::transport(method, url, format!("headers: {error:?}")))?;
        init.set_headers(&headers);
        init.set_body(&wasm_bindgen::JsValue::from_str(&body));
    }
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(|error| {
        HttpFailure::transport(method, url, format!("build request: {error:?}"))
    })?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|error| HttpFailure::transport(method, url, format!("{error:?}")))?;
    let resp: web_sys::Response = resp_value.dyn_into().map_err(|error| {
        HttpFailure::transport(method, url, format!("not a response: {error:?}"))
    })?;
    let status = resp.status();
    let text = JsFuture::from(
        resp.text()
            .map_err(|error| HttpFailure::transport(method, url, format!("body: {error:?}")))?,
    )
    .await
    .map_err(|error| HttpFailure::transport(method, url, format!("body: {error:?}")))?
    .as_string()
    .ok_or_else(|| HttpFailure::transport(method, url, "non-text body"))?;
    if !resp.ok() {
        return Err(HttpFailure::status(method, url, status, text));
    }
    Ok(text)
}

/// Native transport, used by tests and tooling. Same REST wire format.
#[cfg(not(target_arch = "wasm32"))]
async fn fetch_text(
    client: &PlatformClient,
    method: &str,
    url: &str,
    body: Option<String>,
    cache: FetchCache,
) -> std::result::Result<String, HttpFailure> {
    let req = match method {
        "POST" => client.post(url),
        _ => client.get(url),
    };
    let req = req.timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS as u64));
    let req = if matches!(cache, FetchCache::NoStore) {
        req.header(reqwest::header::CACHE_CONTROL, "no-cache")
    } else {
        req
    };
    let req = match body {
        Some(body) => req.header("content-type", "application/json").body(body),
        None => req,
    };
    let request = req
        .build()
        .map_err(|error| HttpFailure::transport(method, url, error.to_string()))?;
    let resp = client
        .execute(request)
        .await
        .map_err(|error| HttpFailure::transport(method, url, error.to_string()))?;
    let status = resp.status();
    // Read the body as a stream so a hostile or broken server cannot make the
    // client buffer an unbounded response; the cap stays well above any
    // legitimate indexer page.
    let mut body = Vec::new();
    let mut chunks = resp.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk =
            chunk.map_err(|error| HttpFailure::transport(method, url, error.to_string()))?;
        if body.len() + chunk.len() > MAX_RESPONSE_BODY_BYTES {
            return Err(HttpFailure::transport(
                method,
                url,
                "response body exceeds the safety limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(body)
        .map_err(|error| HttpFailure::transport(method, url, error.to_string()))?;
    if !status.is_success() {
        return Err(HttpFailure::status(method, url, status.as_u16(), text));
    }
    Ok(text)
}

/// Mirrors the SDK's `parse_sequence_number`: < 512 is blocks, >= 512 seconds.
fn parse_sequence(value: i64) -> Result<bitcoin::Sequence> {
    if value < 0 {
        return Err(anyhow!("invalid negative sequence {value}"));
    }
    if value < 512 {
        Ok(bitcoin::Sequence::from_height(value as u16))
    } else {
        bitcoin::Sequence::from_seconds_ceil(value as u32)
            .map_err(|e| anyhow!("invalid sequence {value}: {e}"))
    }
}

fn parse_indexer_vtxo(vtxo: IndexerVtxo) -> Result<VtxoRecord> {
    let outpoint = vtxo
        .outpoint
        .ok_or_else(|| anyhow!("indexed VTXO is missing its outpoint"))?;
    let script = vtxo
        .script
        .ok_or_else(|| anyhow!("indexed VTXO is missing its script"))?;
    let script: Vec<u8> =
        bitcoin::hex::FromHex::from_hex(&script).context("parse indexed VTXO script hex")?;
    let script = bitcoin::ScriptBuf::from_bytes(script);
    let amount_sats = vtxo
        .amount
        .ok_or_else(|| anyhow!("indexed VTXO is missing its amount"))?
        .parse()
        .context("parse indexed VTXO amount")?;

    let mut seen_assets = std::collections::HashSet::new();
    let assets = vtxo
        .assets
        .unwrap_or_default()
        .into_iter()
        .map(|asset| {
            let parsed = crate::txbuild::parse_asset_id_pub(&asset.asset_id)
                .ok_or_else(|| anyhow!("invalid indexed asset ID {}", asset.asset_id))?;
            if !seen_assets.insert(parsed) {
                return Err(anyhow!("duplicate indexed asset ID {}", asset.asset_id));
            }
            let amount = asset
                .amount
                .ok_or_else(|| anyhow!("indexed asset {} is missing its amount", asset.asset_id))?
                .parse::<u64>()
                .with_context(|| format!("parse indexed asset {} amount", asset.asset_id))?;
            if amount == 0 {
                return Err(anyhow!("indexed asset {} has zero amount", asset.asset_id));
            }
            Ok(Asset {
                asset_id: parsed,
                amount,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let parse_timestamp = |field: Option<I64Wire>, name: &str| -> Result<Option<i64>> {
        let value = field.map(|wire| wire.0);
        if let Some(value) = value {
            if value < 0 {
                return Err(anyhow!("indexed VTXO has negative {name} {value}"));
            }
        }
        // A zero timestamp is an unset sentinel (Badger clears expiry when a
        // VTXO is unrolled), never a usable deadline. Fail closed downstream
        // by normalizing it to `None`.
        Ok(value.filter(|value| *value > 0))
    };

    Ok(VtxoRecord {
        outpoint: OutPoint {
            txid: outpoint.txid.parse().context("parse vtxo txid")?,
            vout: outpoint.vout,
        },
        script,
        amount_sats,
        assets,
        created_at: parse_timestamp(vtxo.created_at, "creation time")?,
        expires_at: parse_timestamp(vtxo.expires_at, "expiry")?,
        is_preconfirmed: vtxo.is_preconfirmed,
        is_swept: vtxo.is_swept,
        spent_by: vtxo
            .spent_by
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse()
                    .context("parse indexed spending Ark transaction")
            })
            .transpose()?,
        settled_by: vtxo
            .settled_by
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse()
                    .context("parse indexed settlement transaction")
            })
            .transpose()?,
        is_unrolled: vtxo.is_unrolled,
        is_spent: vtxo.is_spent,
    })
}

fn fee_expression_is_zero(expression: Option<&str>) -> bool {
    let Some(expression) = expression.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    expression.parse::<f64>().is_ok_and(|value| value == 0.0)
}

fn fee_schedule_is_zero(fees: Option<&InfoFeesResponse>) -> bool {
    fees.is_none_or(|fees| {
        fee_expression_is_zero(fees.intent_fee.offchain_input.as_deref())
            && fee_expression_is_zero(fees.intent_fee.offchain_output.as_deref())
    })
}

fn parse_server_params(info: InfoResponse) -> Result<ServerParams> {
    let version = info.version.trim().to_owned();
    let zero_offchain_fees = fee_schedule_is_zero(info.fees.as_ref())
        && fee_schedule_is_zero(
            info.scheduled_session
                .as_ref()
                .and_then(|session| session.fees.as_ref()),
        );
    let signer_pk: bitcoin::PublicKey =
        info.signer_pubkey.parse().context("parse signer pubkey")?;
    let forfeit_pk = info
        .forfeit_pubkey
        .as_deref()
        .unwrap_or(&info.signer_pubkey)
        .parse()
        .context("parse forfeit pubkey")?;
    let network_name = info.network.to_ascii_lowercase();
    let network = match network_name.as_str() {
        "bitcoin" | "mainnet" => bitcoin::Network::Bitcoin,
        "mutinynet" | "signet" => bitcoin::Network::Signet,
        "regtest" => bitcoin::Network::Regtest,
        "testnet" => bitcoin::Network::Testnet,
        "testnet4" => bitcoin::Network::Testnet4,
        other => return Err(anyhow!("unsupported Arkade network {other}")),
    };
    let delay: i64 = info
        .unilateral_exit_delay
        .parse()
        .context("parse unilateral exit delay")?;
    let checkpoint_tapscript = {
        let raw: Vec<u8> = bitcoin::hex::FromHex::from_hex(&info.checkpoint_tapscript)
            .context("parse checkpoint tapscript hex")?;
        bitcoin::ScriptBuf::from_bytes(raw)
    };
    let forfeit_address = info
        .forfeit_address
        .parse::<bitcoin::Address<_>>()
        .context("parse forfeit address")?
        .require_network(network)
        .context("forfeit address network mismatch")?;
    let dust_sats = info.dust.parse().context("parse dust")?;
    if dust_sats == 0 {
        return Err(anyhow!("dust must be positive"));
    }
    let vtxo_min_sats = info
        .vtxo_min_amount
        .parse()
        .context("parse minimum VTXO amount")?;
    if vtxo_min_sats == 0 {
        return Err(anyhow!("minimum VTXO amount must be positive"));
    }
    let max_tx_weight = info
        .max_tx_weight
        .parse()
        .context("parse max transaction weight")?;
    if max_tx_weight <= 0 {
        return Err(anyhow!("max transaction weight must be positive"));
    }
    let max_op_return_outputs = info
        .max_op_return_outputs
        .parse()
        .context("parse max OP_RETURN outputs")?;
    if max_op_return_outputs < 0 {
        return Err(anyhow!("max OP_RETURN outputs cannot be negative"));
    }
    Ok(ServerParams {
        version,
        signer_pk: signer_pk.inner.x_only_public_key().0,
        forfeit_pk,
        network,
        dust_sats,
        vtxo_min_sats,
        unilateral_exit_delay: parse_sequence(delay)?,
        max_tx_weight,
        zero_offchain_fees,
        max_op_return_outputs,
        checkpoint_tapscript,
        forfeit_address,
    })
}

impl ArkadeRest {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            client: platform_client(),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn get_info(&self) -> Result<ServerParams> {
        let text = fetch_text(
            &self.client,
            "GET",
            &format!("{}/v1/info", self.base),
            None,
            FetchCache::Default,
        )
        .await?;
        let info: InfoResponse = serde_json::from_str(&text).context("parse /v1/info response")?;
        parse_server_params(info)
    }
    pub async fn get_asset_details(&self, asset_id: AssetId) -> Result<AssetDetails> {
        let text = fetch_text(
            &self.client,
            "GET",
            &format!("{}/v1/indexer/asset/{asset_id}", self.base),
            None,
            FetchCache::NoStore,
        )
        .await?;
        let response: AssetDetailsResponse =
            serde_json::from_str(&text).context("parse indexed asset details")?;
        let returned_asset = crate::txbuild::parse_asset_id_pub(&response.asset_id)
            .ok_or_else(|| anyhow!("indexed asset details contain an invalid asset ID"))?;
        if returned_asset != asset_id {
            return Err(anyhow!("indexed asset details returned the wrong asset"));
        }
        let control_asset = if response.control_asset.is_empty() {
            None
        } else {
            Some(
                crate::txbuild::parse_asset_id_pub(&response.control_asset)
                    .ok_or_else(|| anyhow!("indexed asset has an invalid control asset"))?,
            )
        };
        Ok(AssetDetails {
            asset_id,
            supply: response
                .supply
                .parse()
                .context("parse indexed asset supply")?,
            metadata: <Vec<u8> as bitcoin::hex::FromHex>::from_hex(&response.metadata)
                .context("parse indexed asset metadata")?,
            control_asset,
        })
    }

    /// Query one VTXO page for multiple scripts. The REST gateway represents
    /// protobuf repeated fields as repeated query parameters.
    pub async fn get_vtxos_page_many(
        &self,
        script_hexes: &[String],
        filter: &str,
        page_size: i32,
        page_index: i32,
    ) -> Result<VtxoPage> {
        if script_hexes.is_empty() {
            return Ok(VtxoPage {
                vtxos: Vec::new(),
                current: page_index,
                next: 0,
                total: page_index,
            });
        }
        let scripts = script_hexes
            .iter()
            .map(|script| format!("scripts={script}"))
            .collect::<Vec<_>>()
            .join("&");
        let mut url = format!(
            "{}/v1/indexer/vtxos?{scripts}&page.size={page_size}&page.index={page_index}",
            self.base,
        );
        if !filter.is_empty() {
            url.push_str(&format!("&{filter}=true"));
        }
        let text = fetch_text(&self.client, "GET", &url, None, FetchCache::NoStore).await?;
        let resp: GetVtxosResponse = serde_json::from_str(&text).context("parse vtxos")?;
        let out = resp
            .vtxos
            .unwrap_or_default()
            .into_iter()
            .map(parse_indexer_vtxo)
            .collect::<Result<Vec<_>>>()?;
        let page = resp.page.unwrap_or(IndexerPage {
            current: Some(page_index),
            next: Some(0),
            total: Some(page_index),
        });
        Ok(VtxoPage {
            vtxos: out,
            current: page.current.unwrap_or(page_index),
            next: page.next.unwrap_or(0),
            total: page.total.unwrap_or(page_index),
        })
    }

    /// Query every VTXO page and deduplicate shifting page boundaries.
    pub async fn get_vtxos(&self, script_hex: &str, filter: &str) -> Result<Vec<VtxoRecord>> {
        self.get_vtxos_many(&[script_hex.to_string()], filter).await
    }

    /// Query every VTXO page for a set of scripts.
    pub async fn get_vtxos_many(
        &self,
        script_hexes: &[String],
        filter: &str,
    ) -> Result<Vec<VtxoRecord>> {
        let mut index = 1;
        let mut records = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut visited = std::collections::HashSet::from([index]);
        loop {
            let page = self
                .get_vtxos_page_many(script_hexes, filter, 500, index)
                .await?;
            let next = next_vtxo_page(&page, index, &mut visited)?;
            merge_vtxo_records(&mut records, &mut seen, page.vtxos)?;
            let Some(next) = next else {
                break;
            };
            index = next;
        }
        Ok(records)
    }

    /// Query exact VTXO outpoints without scanning a shared script lineage.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub async fn get_vtxos_by_outpoints(&self, outpoints: &[OutPoint]) -> Result<Vec<VtxoRecord>> {
        let mut requested = outpoints
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        requested.sort_unstable();
        let chunks = requested
            .chunks(MAX_VTXO_OUTPOINTS_PER_REQUEST)
            .map(<[OutPoint]>::to_vec)
            .collect::<Vec<_>>();
        let responses = stream::iter(chunks)
            .map(|chunk| async move { self.get_vtxos_by_outpoint_chunk(&chunk).await })
            .buffer_unordered(EXACT_VTXO_REQUEST_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut records = std::collections::HashMap::new();
        for response in responses {
            for record in response? {
                if records.insert(record.outpoint, record).is_some() {
                    return Err(anyhow!("indexer returned a duplicate exact VTXO"));
                }
            }
        }
        Ok(records.into_values().collect())
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    async fn get_vtxos_by_outpoint_chunk(&self, outpoints: &[OutPoint]) -> Result<Vec<VtxoRecord>> {
        let requested = outpoints
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let query = outpoints
            .iter()
            .map(|outpoint| format!("outpoints={outpoint}"))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!(
            "{}/v1/indexer/vtxos?{query}&page.size={}&page.index=1",
            self.base,
            outpoints.len()
        );
        let text = fetch_text(&self.client, "GET", &url, None, FetchCache::NoStore).await?;
        let response: GetVtxosResponse =
            serde_json::from_str(&text).context("parse exact vtxos")?;
        response
            .vtxos
            .unwrap_or_default()
            .into_iter()
            .map(parse_indexer_vtxo)
            .map(|record| {
                let record = record?;
                if !requested.contains(&record.outpoint) {
                    return Err(anyhow!(
                        "indexer returned unexpected exact VTXO {}",
                        record.outpoint
                    ));
                }
                Ok(record)
            })
            .collect()
    }

    /// Fetch exact cooperative successor candidates, including VTXOs recreated
    /// as leaves of a settlement batch. Callers bind candidates to protocol
    /// identities from their creating transactions.
    pub async fn get_vtxo_successor_candidates(
        &self,
        spent: &[VtxoRecord],
        expected_script: &ScriptBuf,
        marker_asset: AssetId,
    ) -> Result<Vec<(VtxoRecord, Transaction)>> {
        let mut candidate_outpoints = Vec::new();
        let mut settlements = Vec::new();
        for record in spent {
            if let Some(txid) = record.spent_by {
                candidate_outpoints.extend([
                    OutPoint {
                        txid,
                        vout: u32::from(crate::protocol::RENEWAL_STATE_OUTPUT_INDEX),
                    },
                    OutPoint {
                        txid,
                        vout: u32::from(crate::protocol::TREE_OUTPUT_INDEX),
                    },
                ]);
            } else if let Some(txid) = record.settled_by {
                settlements.push(txid);
            } else {
                return Err(anyhow!("spent VTXO {} has no successor", record.outpoint));
            }
        }
        candidate_outpoints.extend(self.get_batch_tree_leaves(&settlements).await?);
        let candidates = self
            .get_vtxos_by_outpoints(&candidate_outpoints)
            .await?
            .into_iter()
            .filter(|record| {
                record.script == *expected_script && record.asset_amount(marker_asset) == Some(1)
            })
            .collect::<Vec<_>>();
        let transactions = self
            .get_virtual_txs(
                &candidates
                    .iter()
                    .map(|record| record.outpoint.txid)
                    .collect::<Vec<_>>(),
            )
            .await?;
        candidates
            .into_iter()
            .map(|record| {
                let transaction = transactions
                    .get(&record.outpoint.txid)
                    .cloned()
                    .ok_or_else(|| anyhow!("tree successor transaction is not indexed yet"))?;
                Ok((record, transaction))
            })
            .collect()
    }

    async fn get_batch_tree_leaves(&self, settlements: &[Txid]) -> Result<Vec<OutPoint>> {
        let mut settlements = settlements
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        settlements.sort_unstable();
        let responses = stream::iter(
            settlements
                .into_iter()
                .map(|txid| async move { self.get_commitment_tree_leaves(txid).await }),
        )
        .buffer_unordered(VIRTUAL_TX_REQUEST_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        let mut leaves = std::collections::HashSet::new();
        for response in responses {
            for leaf in response? {
                leaves.insert(leaf);
            }
        }
        Ok(leaves.into_iter().collect())
    }

    async fn get_commitment_tree_leaves(&self, txid: Txid) -> Result<Vec<OutPoint>> {
        let url = format!("{}/v1/indexer/commitmentTx/{txid}", self.base);
        let text = fetch_text(&self.client, "GET", &url, None, FetchCache::NoStore).await?;
        let response: GetCommitmentTxResponse =
            serde_json::from_str(&text).context("parse commitment transaction")?;
        let mut batch_vouts = response
            .batches
            .keys()
            .map(|value| {
                value
                    .parse::<u32>()
                    .context("parse commitment batch output")
            })
            .collect::<Result<Vec<_>>>()?;
        batch_vouts.sort_unstable();
        // Cap the walk so a malicious indexer cannot keep the client fetching
        // tree pages forever.
        if batch_vouts.len() > MAX_COMMITMENT_BATCH_VOUTS {
            return Err(anyhow!(
                "commitment transaction {txid} reports more than {MAX_COMMITMENT_BATCH_VOUTS} batch outputs"
            ));
        }
        let mut leaves = Vec::new();
        for vout in batch_vouts {
            let mut index = 1;
            let mut visited = std::collections::HashSet::from([index]);
            loop {
                let url = format!(
                    "{}/v1/indexer/batch/{txid}/{vout}/tree/leaves?page.size=500&page.index={index}",
                    self.base
                );
                let text = fetch_text(&self.client, "GET", &url, None, FetchCache::NoStore).await?;
                let response: GetVtxoTreeLeavesResponse =
                    serde_json::from_str(&text).context("parse commitment tree leaves")?;
                for leaf in response.leaves.unwrap_or_default() {
                    leaves.push(OutPoint {
                        txid: leaf
                            .txid
                            .parse()
                            .context("parse commitment tree leaf txid")?,
                        vout: leaf.vout,
                    });
                }
                let page = response.page.unwrap_or(IndexerPage {
                    current: Some(index),
                    next: Some(0),
                    total: Some(index),
                });
                let cursor = VtxoPage {
                    vtxos: Vec::new(),
                    current: page.current.unwrap_or(index),
                    next: page.next.unwrap_or(0),
                    total: page.total.unwrap_or(0),
                };
                let Some(next) = next_vtxo_page(&cursor, index, &mut visited)? else {
                    break;
                };
                index = next;
            }
        }
        Ok(leaves)
    }

    /// Fetch full virtual transactions and key them by their computed txid.
    pub async fn get_virtual_txs(
        &self,
        txids: &[Txid],
    ) -> Result<std::collections::HashMap<Txid, Transaction>> {
        let mut requested = txids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        requested.sort_unstable();
        let chunks = requested
            .chunks(MAX_VIRTUAL_TXS_PER_REQUEST)
            .map(<[Txid]>::to_vec)
            .collect::<Vec<_>>();
        let responses = stream::iter(chunks)
            .map(|chunk| async move { self.get_virtual_txs_chunk(&chunk).await })
            .buffer_unordered(VIRTUAL_TX_REQUEST_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut transactions = std::collections::HashMap::with_capacity(requested.len());
        for response in responses {
            for (txid, transaction) in response? {
                if transactions.insert(txid, transaction).is_some() {
                    return Err(anyhow!(
                        "indexer returned duplicate virtual transaction {txid}"
                    ));
                }
            }
        }
        Ok(transactions)
    }

    async fn get_virtual_txs_chunk(
        &self,
        txids: &[Txid],
    ) -> Result<std::collections::HashMap<Txid, Transaction>> {
        use base64::Engine;
        if txids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let requested = txids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let joined = txids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!("{}/v1/indexer/virtualTx/{joined}", self.base);
        let text = fetch_text(&self.client, "GET", &url, None, FetchCache::NoStore).await?;
        let response: GetVirtualTxsResponse =
            serde_json::from_str(&text).context("parse virtual transactions")?;
        let mut transactions = std::collections::HashMap::new();
        for encoded in response.txs.unwrap_or_default() {
            let mut decoded = None;
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&encoded) {
                if let Ok(psbt) = bitcoin::Psbt::deserialize(&raw) {
                    decoded = Some(psbt.unsigned_tx);
                } else if let Ok(transaction) = bitcoin::consensus::encode::deserialize(&raw) {
                    decoded = Some(transaction);
                }
            }
            let transaction = match decoded {
                Some(transaction) => transaction,
                None => {
                    let raw: Vec<u8> = bitcoin::hex::FromHex::from_hex(&encoded)
                        .context("decode virtual transaction as base64 or hex")?;
                    bitcoin::consensus::encode::deserialize(&raw)
                        .context("decode raw virtual transaction")?
                }
            };
            let txid = transaction.compute_txid();
            if !requested.contains(&txid) {
                return Err(anyhow!(
                    "indexer returned unexpected virtual transaction {txid}"
                ));
            }
            if transactions.insert(txid, transaction).is_some() {
                return Err(anyhow!(
                    "indexer returned duplicate virtual transaction {txid}"
                ));
            }
        }
        if transactions.len() != requested.len() {
            let missing = requested
                .difference(&transactions.keys().copied().collect())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!("indexer omitted virtual transaction(s): {missing}"));
        }
        Ok(transactions)
    }

    /// Submit a signed ark tx + unsigned checkpoints; returns the server's
    /// cosigned ark tx PSBT and partially-signed checkpoint PSBTs.
    pub async fn submit_tx(
        &self,
        ark_psbt: &bitcoin::Psbt,
        checkpoint_psbts: &[bitcoin::Psbt],
    ) -> Result<(bitcoin::Txid, bitcoin::Psbt, Vec<bitcoin::Psbt>)> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let body = serde_json::to_string(&SubmitTxRequest {
            signed_ark_tx: &b64.encode(ark_psbt.serialize()),
            checkpoint_txs: checkpoint_psbts
                .iter()
                .map(|p| b64.encode(p.serialize()))
                .collect(),
        })?;
        let text = fetch_text(
            &self.client,
            "POST",
            &format!("{}/v1/tx/submit", self.base),
            Some(body),
            FetchCache::Default,
        )
        .await?;
        let resp: SubmitTxResponse = serde_json::from_str(&text).context("parse submit resp")?;
        let final_ark = resp
            .final_ark_tx
            .ok_or_else(|| anyhow!("submit response missing finalArkTx"))?;
        let signed_ark = bitcoin::Psbt::deserialize(
            &b64.decode(&final_ark).context("decode finalArkTx base64")?,
        )
        .context("decode finalArkTx psbt")?;
        // The ark txid is the txid of the (cosigned) ark tx itself; the
        // server's arkTxid field may be empty over REST, so compute it.
        let txid = match resp.ark_txid.as_deref().filter(|s| s.len() == 64) {
            Some(s) => s
                .parse()
                .unwrap_or_else(|_| signed_ark.unsigned_tx.compute_txid()),
            None => signed_ark.unsigned_tx.compute_txid(),
        };
        let checkpoints = resp
            .signed_checkpoint_txs
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                let raw = b64.decode(&s).context("decode checkpoint base64")?;
                bitcoin::Psbt::deserialize(&raw).context("decode checkpoint psbt")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((txid, signed_ark, checkpoints))
    }

    pub async fn finalize_tx(
        &self,
        txid: bitcoin::Txid,
        checkpoints: &[bitcoin::Psbt],
    ) -> Result<()> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let body = serde_json::to_string(&FinalizeTxRequest {
            ark_txid: &txid.to_string(),
            final_checkpoint_txs: checkpoints
                .iter()
                .map(|p| b64.encode(p.serialize()))
                .collect(),
        })?;
        fetch_text(
            &self.client,
            "POST",
            &format!("{}/v1/tx/finalize", self.base),
            Some(body),
            FetchCache::Default,
        )
        .await?;
        Ok(())
    }
}

impl EmulatorRest {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            client: platform_client(),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn get_info(&self) -> Result<EmulatorParams> {
        let text = fetch_text(
            &self.client,
            "GET",
            &format!("{}/v1/info", self.base),
            None,
            FetchCache::NoStore,
        )
        .await?;
        let info: EmulatorInfoResponse =
            serde_json::from_str(&text).context("parse emulator /v1/info response")?;
        let signer: bitcoin::PublicKey = info
            .signer_pubkey
            .parse()
            .context("parse emulator signer pubkey")?;
        Ok(EmulatorParams {
            version: info.version.unwrap_or_else(|| "unknown".to_string()),
            signer_pk: signer.inner.x_only_public_key().0,
        })
    }

    /// Execute the Arkade scripts in an unsigned intent proof and return the
    /// emulator-countersigned proof PSBT. Must be called before registration.
    pub async fn submit_intent(&self, proof_b64: &str, message: &str) -> Result<bitcoin::Psbt> {
        use base64::Engine;
        let body = serde_json::to_string(&EmulatorSubmitIntentRequest {
            intent: EmulatorIntentWire {
                proof: proof_b64,
                message,
            },
        })?;
        let text = fetch_text(
            &self.client,
            "POST",
            &format!("{}/v1/intent", self.base),
            Some(body),
            FetchCache::Default,
        )
        .await?;
        let response: EmulatorSubmitIntentResponse =
            serde_json::from_str(&text).context("parse emulator intent response")?;
        let signed = response
            .signed_proof
            .ok_or_else(|| anyhow!("emulator intent response missing signedProof"))?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&signed)
            .context("decode emulator intent proof base64")?;
        bitcoin::Psbt::deserialize(&raw).context("decode emulator intent proof PSBT")
    }

    /// Countersign the forfeits of a previously approved intent. The emulator
    /// only signs inputs whose valid signature is present in that proof.
    pub async fn submit_finalization(
        &self,
        proof_b64: &str,
        message: &str,
        forfeits: Vec<String>,
        connector_tree: Vec<EmulatorTxTreeNode>,
        commitment_tx_b64: &str,
    ) -> Result<EmulatorFinalization> {
        let body = serde_json::to_string(&EmulatorSubmitFinalizationRequest {
            signed_intent: EmulatorIntentWire {
                proof: proof_b64,
                message,
            },
            forfeits,
            connector_tree,
            commitment_tx: commitment_tx_b64,
        })?;
        let text = fetch_text(
            &self.client,
            "POST",
            &format!("{}/v1/finalization", self.base),
            Some(body),
            FetchCache::Default,
        )
        .await?;
        serde_json::from_str(&text).context("parse emulator finalization response")
    }

    pub async fn submit_tx(
        &self,
        ark_psbt: &bitcoin::Psbt,
        checkpoint_psbts: &[bitcoin::Psbt],
    ) -> Result<(bitcoin::Psbt, Vec<bitcoin::Psbt>)> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let body = serde_json::to_string(&EmulatorSubmitTxRequest {
            ark_tx: &b64.encode(ark_psbt.serialize()),
            checkpoint_txs: checkpoint_psbts
                .iter()
                .map(|checkpoint| b64.encode(checkpoint.serialize()))
                .collect(),
        })?;
        let text = fetch_text(
            &self.client,
            "POST",
            &format!("{}/v1/tx", self.base),
            Some(body),
            FetchCache::Default,
        )
        .await?;
        let response: EmulatorSubmitTxResponse =
            serde_json::from_str(&text).context("parse emulator submit response")?;
        let signed_ark = response
            .signed_ark_tx
            .ok_or_else(|| anyhow!("emulator response missing signedArkTx"))?;
        let signed_ark = bitcoin::Psbt::deserialize(
            &b64.decode(signed_ark)
                .context("decode emulator Ark PSBT base64")?,
        )
        .context("decode emulator Ark PSBT")?;
        let checkpoints = response
            .signed_checkpoint_txs
            .unwrap_or_default()
            .into_iter()
            .map(|encoded| {
                bitcoin::Psbt::deserialize(
                    &b64.decode(encoded)
                        .context("decode emulator checkpoint base64")?,
                )
                .context("decode emulator checkpoint PSBT")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((signed_ark, checkpoints))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};

    fn asset_id(byte: u8, group_index: u16) -> String {
        let group = group_index.to_le_bytes();
        format!(
            "{}{:02x}{:02x}",
            format!("{byte:02x}").repeat(32),
            group[0],
            group[1]
        )
    }

    fn indexed_vtxo(assets: serde_json::Value) -> IndexerVtxo {
        serde_json::from_value(serde_json::json!({
            "outpoint": {
                "txid": format!("{:02x}", 7).repeat(32),
                "vout": 1
            },
            "script": format!("5120{}", format!("{:02x}", 8).repeat(32)),
            "amount": "330",
            "assets": assets
        }))
        .unwrap()
    }

    fn info_response() -> InfoResponse {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[7; 32]).unwrap());
        let public_key = bitcoin::PublicKey::new(keypair.public_key()).to_string();
        let address = bitcoin::Address::p2tr(
            &secp,
            keypair.x_only_public_key().0,
            None,
            bitcoin::Network::Regtest,
        );
        InfoResponse {
            version: "test".to_owned(),
            signer_pubkey: public_key,
            forfeit_pubkey: None,
            network: "regtest".to_owned(),
            dust: "330".to_owned(),
            vtxo_min_amount: "1".to_owned(),
            unilateral_exit_delay: "144".to_owned(),
            max_op_return_outputs: "3".to_owned(),
            max_tx_weight: "40000".to_owned(),
            checkpoint_tapscript: String::new(),
            forfeit_address: address.to_string(),
            fees: None,
            scheduled_session: None,
        }
    }
    #[test]
    fn server_fee_policy_accepts_empty_zero_and_missing_expressions_only() {
        let mut info = info_response();
        info.fees = Some(InfoFeesResponse {
            intent_fee: InfoIntentFeeResponse {
                offchain_input: Some(String::new()),
                offchain_output: Some("0.0".to_string()),
            },
        });
        assert!(
            parse_server_params(info.clone())
                .unwrap()
                .zero_offchain_fees
        );

        info.fees.as_mut().unwrap().intent_fee.offchain_input = Some("0.01".to_string());
        assert!(
            !parse_server_params(info.clone())
                .unwrap()
                .zero_offchain_fees
        );

        info.fees = None;
        info.scheduled_session = Some(InfoScheduledSessionResponse {
            fees: Some(InfoFeesResponse {
                intent_fee: InfoIntentFeeResponse {
                    offchain_input: None,
                    offchain_output: Some("fee_expression".to_string()),
                },
            }),
        });
        assert!(!parse_server_params(info).unwrap().zero_offchain_fees);
    }
    #[test]
    fn rollover_margin_tracks_short_network_lifetimes_without_looping() {
        let mut record = parse_indexer_vtxo(indexed_vtxo(serde_json::json!([]))).unwrap();
        record.created_at = Some(1_000);
        record.expires_at = Some(3_048);
        assert_eq!(record.rollover_margin_seconds(), 1_024);

        record.expires_at = Some(200_000);
        assert_eq!(record.rollover_margin_seconds(), MAX_ROLLOVER_MARGIN_SECS);

        record.created_at = None;
        assert_eq!(record.rollover_margin_seconds(), MAX_ROLLOVER_MARGIN_SECS);
    }

    #[test]
    fn indexer_vtxo_parser_accepts_only_complete_canonical_records() {
        let id = asset_id(9, 1);
        let record = parse_indexer_vtxo(indexed_vtxo(serde_json::json!([{
            "assetId": id,
            "amount": "5"
        }])))
        .unwrap();
        assert_eq!(record.amount_sats, 330);
        assert_eq!(record.assets.len(), 1);
        assert_eq!(record.assets[0].asset_id.to_string(), asset_id(9, 1));
        assert_eq!(record.assets[0].amount, 5);

        let mut uppercase_script = indexed_vtxo(serde_json::json!([]));
        uppercase_script.script = uppercase_script.script.map(|script| script.to_uppercase());
        assert_eq!(
            parse_indexer_vtxo(uppercase_script)
                .unwrap()
                .script
                .to_hex_string(),
            format!("5120{}", format!("{:02x}", 8).repeat(32))
        );
    }

    #[test]
    fn indexer_vtxo_parser_rejects_missing_or_malformed_fields() {
        let id = asset_id(9, 1);
        let split_multibyte_boundary = format!("{}é{}", "0".repeat(63), "0".repeat(3));
        assert_eq!(split_multibyte_boundary.len(), 68);
        let invalid_assets = [
            serde_json::json!([{"assetId": "bad", "amount": "1"}]),
            serde_json::json!([{"assetId": asset_id(0xab, 1).to_uppercase(), "amount": "1"}]),
            serde_json::json!([{"assetId": split_multibyte_boundary, "amount": "1"}]),
            serde_json::json!([{"assetId": id, "amount": "0"}]),
            serde_json::json!([{"assetId": id, "amount": "bad"}]),
            serde_json::json!([{"assetId": id}]),
            serde_json::json!([
                {"assetId": id, "amount": "1"},
                {"assetId": id, "amount": "1"}
            ]),
        ];
        for assets in invalid_assets {
            assert!(parse_indexer_vtxo(indexed_vtxo(assets)).is_err());
        }

        let mut missing_script = serde_json::to_value(indexed_vtxo(serde_json::json!([]))).unwrap();
        missing_script.as_object_mut().unwrap().remove("script");
        assert!(parse_indexer_vtxo(serde_json::from_value(missing_script).unwrap()).is_err());

        let mut bad_script = serde_json::to_value(indexed_vtxo(serde_json::json!([]))).unwrap();
        bad_script["script"] = serde_json::json!("not-hex");
        assert!(parse_indexer_vtxo(serde_json::from_value(bad_script).unwrap()).is_err());

        let mut missing_amount = serde_json::to_value(indexed_vtxo(serde_json::json!([]))).unwrap();
        missing_amount.as_object_mut().unwrap().remove("amount");
        assert!(parse_indexer_vtxo(serde_json::from_value(missing_amount).unwrap()).is_err());
    }

    #[test]
    fn server_info_requires_strict_policy_fields() {
        let params = parse_server_params(info_response()).unwrap();

        assert_eq!(params.version, "test");
        assert_eq!(params.dust_sats, 330);
        assert_eq!(params.vtxo_min_sats, 1);
        assert_eq!(params.max_tx_weight, 40_000);
        assert_eq!(params.max_op_return_outputs, 3);

        for field in [
            "version",
            "vtxoMinAmount",
            "maxTxWeight",
            "maxOpReturnOutputs",
        ] {
            let mut json = serde_json::to_value(info_response()).unwrap();
            json.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<InfoResponse>(json).is_err(),
                "accepted missing {field}"
            );
        }

        let mut malformed = info_response();
        malformed.vtxo_min_amount = "not-a-number".to_owned();
        assert!(parse_server_params(malformed).is_err());

        let mut zero_minimum = info_response();
        zero_minimum.vtxo_min_amount = "0".to_owned();
        assert!(parse_server_params(zero_minimum).is_err());

        let mut zero_weight = info_response();
        zero_weight.max_tx_weight = "0".to_owned();
        assert!(parse_server_params(zero_weight).is_err());

        let mut negative_outputs = info_response();
        negative_outputs.max_op_return_outputs = "-1".to_owned();
        assert!(parse_server_params(negative_outputs).is_err());
    }
    #[test]
    fn server_info_accepts_empty_version_from_regtest_services() {
        let mut info = info_response();
        info.version = " \t ".to_owned();

        assert_eq!(parse_server_params(info).unwrap().version, "");
    }

    #[test]
    fn pagination_rejects_bad_cursors_and_deduplicates_shifting_pages() {
        let mut visited = std::collections::HashSet::from([1]);
        let page = VtxoPage {
            vtxos: Vec::new(),
            current: 1,
            next: 2,
            total: 3,
        };
        assert_eq!(next_vtxo_page(&page, 1, &mut visited).unwrap(), Some(2));
        assert!(next_vtxo_page(&page, 1, &mut visited).is_err());
        let terminal = VtxoPage {
            vtxos: Vec::new(),
            current: 1,
            next: 1,
            total: 1,
        };
        assert_eq!(next_vtxo_page(&terminal, 1, &mut visited).unwrap(), None);
        let mut wrong = page;
        wrong.current = 2;
        assert!(next_vtxo_page(&wrong, 1, &mut visited).is_err());
        wrong.current = 1;
        wrong.next = 4;
        assert!(next_vtxo_page(&wrong, 1, &mut visited).is_err());

        let first = live_record();
        let mut second = first.clone();
        second.outpoint.vout += 1;
        let mut records = Vec::new();
        let mut seen = std::collections::HashSet::new();
        merge_vtxo_records(&mut records, &mut seen, vec![first.clone(), second.clone()]).unwrap();
        merge_vtxo_records(&mut records, &mut seen, vec![second, first]).unwrap();
        assert_eq!(records.len(), 2);
    }

    fn live_indexed_vtxo() -> serde_json::Value {
        serde_json::json!({
            "outpoint": {
                "txid": format!("{:02x}", 7).repeat(32),
                "vout": 1
            },
            "script": format!("5120{}", format!("{:02x}", 8).repeat(32)),
            "amount": "330",
            "createdAt": "1700000000",
            "expiresAt": "1700008192",
            "isPreconfirmed": false,
            "isSwept": false,
            "isUnrolled": false,
            "isSpent": false
        })
    }

    #[test]
    fn indexer_vtxo_parser_reads_lifecycle_fields() {
        let record = parse_indexer_vtxo(serde_json::from_value(live_indexed_vtxo()).unwrap())
            .expect("parse live record");
        assert_eq!(record.created_at, Some(1_700_000_000));
        assert_eq!(record.expires_at, Some(1_700_008_192));
        assert!(!record.is_preconfirmed && !record.is_swept && !record.is_unrolled);
        assert!(!record.is_spent);

        // The OpenAPI document declares integers rather than strings.
        let mut numeric = live_indexed_vtxo();
        numeric["createdAt"] = serde_json::json!(1_700_000_000_i64);
        numeric["expiresAt"] = serde_json::json!(1_700_008_192_i64);
        let record = parse_indexer_vtxo(serde_json::from_value(numeric).unwrap())
            .expect("parse numeric timestamps");
        assert_eq!(record.expires_at, Some(1_700_008_192));

        // Zero timestamps and omitted lifecycle fields are normalized to
        // unknown, which fails closed in the liveness guard.
        let mut zeroed = live_indexed_vtxo();
        zeroed["expiresAt"] = serde_json::json!("0");
        let record = parse_indexer_vtxo(serde_json::from_value(zeroed).unwrap()).unwrap();
        assert_eq!(record.expires_at, None);
        let record = parse_indexer_vtxo(indexed_vtxo(serde_json::json!([]))).unwrap();
        assert_eq!(record.created_at, None);
        assert_eq!(record.expires_at, None);
        assert!(!record.is_spent);

        let mut spent = live_indexed_vtxo();
        spent["isSpent"] = serde_json::json!(true);
        spent["spentBy"] = serde_json::json!(format!("{:02x}", 9).repeat(32));
        spent["arkTxid"] = serde_json::json!(format!("{:02x}", 10).repeat(32));
        spent["settledBy"] = serde_json::json!(format!("{:02x}", 11).repeat(32));
        let record = parse_indexer_vtxo(serde_json::from_value(spent).unwrap()).unwrap();
        assert_eq!(
            record.spent_by,
            Some(format!("{:02x}", 10).repeat(32).parse().unwrap())
        );
        assert_eq!(
            record.settled_by,
            Some(format!("{:02x}", 11).repeat(32).parse().unwrap())
        );

        for bad in [
            serde_json::json!(-5),
            serde_json::json!("-5"),
            serde_json::json!(1.5),
            serde_json::json!("1e6"),
            serde_json::json!("bad"),
        ] {
            let mut json = live_indexed_vtxo();
            json["expiresAt"] = bad.clone();
            let rejected = serde_json::from_value::<IndexerVtxo>(json)
                .map(parse_indexer_vtxo)
                .map(|parsed| parsed.is_err())
                .unwrap_or(true);
            assert!(rejected, "accepted malformed expiry {bad}");
        }
    }

    fn live_record() -> VtxoRecord {
        parse_indexer_vtxo(serde_json::from_value(live_indexed_vtxo()).unwrap()).unwrap()
    }

    #[test]
    fn liveness_guard_enforces_margin_and_status() {
        let now = 1_700_005_000;
        let record = live_record();
        record.ensure_live(now, 300).expect("live record");
        assert_eq!(record.expires_in(now), Some(3_192));

        record
            .ensure_live(now, 3_191)
            .expect("strictly inside the remaining lifetime");
        assert!(
            record.ensure_live(now, 3_192).is_err(),
            "boundary fails closed"
        );
        assert!(record.ensure_live(now, 10_000).is_err());
        assert!(record.ensure_live(now, -1).is_err());

        let mut spent = live_record();
        spent.is_spent = true;
        assert!(spent.ensure_live(now, 300).is_err());
        let mut swept = live_record();
        swept.is_swept = true;
        assert!(swept.ensure_live(now, 300).is_err());
        let mut unrolled = live_record();
        unrolled.is_unrolled = true;
        assert!(unrolled.ensure_live(now, 300).is_err());

        let mut unknown_expiry = live_record();
        unknown_expiry.expires_at = None;
        assert!(unknown_expiry.ensure_live(now, 300).is_err());
        let mut unknown_creation = live_record();
        unknown_creation.created_at = None;
        assert!(unknown_creation.ensure_live(now, 300).is_err());
        let mut inverted = live_record();
        inverted.expires_at = inverted.created_at;
        assert!(inverted.ensure_live(now, 300).is_err());

        // A record well past expiry never passes regardless of the margin.
        assert!(live_record().ensure_live(1_800_000_000, 0).is_err());
    }
}
