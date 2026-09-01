//! Native setup for woodland.sh's shared local-regtest world.

use crate::arkade::{ArkadeRest, EmulatorParams, EmulatorRest, ServerParams, VtxoRecord};
use crate::keys::Keys;
use crate::tree;
use crate::txbuild;
use crate::world::{
    asset_metadata_entries, tree_states, WorldManifest, ACTIVE_LOGS_PER_TREE, GAME_ID,
    LOG_RESERVE_PER_TREE, MANIFEST_SCHEMA_VERSION, PROTOCOL_DUST_SATS, PROTOCOL_VERSION,
    TREE_COUNT, XP_PER_TREE,
};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::packet::{AssetGroup, AssetInput, AssetOutput, Packet};
use ark_core::asset::AssetId;
use ark_core::send::{build_offchain_transactions, SendReceiver};
use ark_core::Asset;
use bitcoin::{Amount, OutPoint, Psbt, Txid, XOnlyPublicKey};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const ROLLOVER_SECRET_ENV: &str = "WOODLAND_ROLLOVER_SECRET";
const DEPLOYER_SECRET_ENV: &str = "WOODLAND_DEPLOYER_SECRET";
const ARKADE_SERVICE_URL_ENV: &str = "WOODLAND_ARKADE_SERVICE_URL";
const EMULATOR_URL_ENV: &str = "WOODLAND_EMULATOR_URL";
const NETWORK_ENV: &str = "WOODLAND_NETWORK";
const EXPECTED_ARKADE_SIGNER_ENV: &str = "WOODLAND_EXPECTED_ARKADE_SIGNER";
const EXPECTED_ARKADE_VERSION_ENV: &str = "WOODLAND_EXPECTED_ARKADE_VERSION";
const EXPECTED_EMULATOR_SIGNER_ENV: &str = "WOODLAND_EXPECTED_EMULATOR_SIGNER";
const EXPECTED_EMULATOR_VERSION_ENV: &str = "WOODLAND_EXPECTED_EMULATOR_VERSION";
const FORCE_ROLLOVER_ENV: &str = "WOODLAND_FORCE_ROLLOVER";
const STARTUP_RENEWAL_ENV: &str = "WOODLAND_RENEWAL_STARTUP";
const INDEX_ATTEMPTS: usize = 80;
const INDEX_POLL_MS: u64 = 250;
/// Renew a tree once its remaining batch lifetime drops below this margin,
/// so the exact self-send settles long before the fail-closed input check
/// would refuse it.
const TREE_ROLLOVER_CHECK_SECS: u64 = 60;
const RENEWAL_CONCURRENCY: usize = 8;
const DEPLOYMENT_SHARD_SIZE: usize = 50;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannedDeployment {
    state: tree::TreeState,
    source_outpoint: String,
    ark: String,
    checkpoints: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannedShard {
    source_outpoint: String,
    tree_count: u64,
    deployments: Vec<PlannedDeployment>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BootstrapPlan {
    manifest: WorldManifest,
    deployer_script: String,
    funding_outpoints: Vec<String>,
    issuance_ark: String,
    issuance_checkpoints: Vec<String>,
    distribution_ark: String,
    distribution_checkpoints: Vec<String>,
    shards: Vec<PlannedShard>,
}

struct Services {
    arkade_url: String,
    emulator_url: String,
    rest: ArkadeRest,
    emulator_rest: EmulatorRest,
    params: ServerParams,
    emulator: EmulatorParams,
}

struct CurrentTree {
    state: tree::TreeState,
    health: tree::TreeHealth,
    record: VtxoRecord,
    previous_tx: bitcoin::Transaction,
}
fn require_mode_flag(name: &str, operation: &str) -> Result<()> {
    if std::env::var(name).as_deref() == Ok("1") {
        Ok(())
    } else {
        Err(anyhow!(
            "{operation} is disabled without {name}=1; stop interactive gameplay first"
        ))
    }
}
fn load_keys(name: &str) -> Result<Keys> {
    let secret = std::env::var(name).with_context(|| format!("{name} is not set"))?;
    Keys::from_hex(&secret)
}
fn optional_setting(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value.trim().to_owned())),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(anyhow!("{name} is not valid UTF-8")),
    }
}

fn parse_signer_pin(name: &str, value: &str) -> Result<XOnlyPublicKey> {
    if let Ok(key) = value.parse::<XOnlyPublicKey>() {
        return Ok(key);
    }
    let key = value
        .parse::<bitcoin::PublicKey>()
        .with_context(|| format!("parse {name} as an x-only or compressed public key"))?;
    Ok(key.inner.x_only_public_key().0)
}

#[derive(Clone, Copy)]
struct PinSetting<'a> {
    name: &'a str,
    value: Option<&'a str>,
}

fn validate_service_pin(
    mainnet: bool,
    label: &str,
    actual_signer: XOnlyPublicKey,
    actual_version: &str,
    signer: PinSetting<'_>,
    version: PinSetting<'_>,
) -> Result<()> {
    if mainnet && (signer.value.is_none() || version.value.is_none()) {
        return Err(anyhow!(
            "mainnet {label} requires explicit {} and {} pins",
            signer.name,
            version.name
        ));
    }
    if let Some(expected) = signer.value {
        let expected = parse_signer_pin(signer.name, expected)?;
        if expected != actual_signer {
            return Err(anyhow!(
                "{label} signer {actual_signer} does not match {}={expected}",
                signer.name
            ));
        }
    }
    if let Some(expected) = version.value {
        if expected != actual_version {
            return Err(anyhow!(
                "{label} version {actual_version} does not match {}={expected}",
                version.name
            ));
        }
    }
    Ok(())
}

fn require_service_pin(
    mainnet: bool,
    label: &str,
    actual_signer: XOnlyPublicKey,
    actual_version: &str,
    signer_env: &str,
    version_env: &str,
) -> Result<()> {
    let expected_signer = optional_setting(signer_env)?;
    let expected_version = optional_setting(version_env)?;
    validate_service_pin(
        mainnet,
        label,
        actual_signer,
        actual_version,
        PinSetting {
            name: signer_env,
            value: expected_signer.as_deref(),
        },
        PinSetting {
            name: version_env,
            value: expected_version.as_deref(),
        },
    )
}

pub async fn run_cli() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or_else(|| {
        anyhow!(
            "usage: woodland-operator <status|ensure|renew-once|watch> <manifest>\n       woodland-operator renewal-address <manifest>\n       woodland-operator renew <manifest> <tree <tree_id>|player <owner_pubkey> <player_asset>>"
        )
    })?;
    let manifest_path = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing world manifest path"))?;
    let services = connect_services().await?;

    match command.as_str() {
        "status" => {
            if args.next().is_some() {
                return Err(anyhow!("unexpected bootstrap argument"));
            }
            let deployer = load_keys(DEPLOYER_SECRET_ENV)?;
            print_status(&manifest_path, &deployer, &services).await
        }
        "ensure" => {
            if args.next().is_some() {
                return Err(anyhow!("unexpected bootstrap argument"));
            }
            let deployer = load_keys(DEPLOYER_SECRET_ENV)?;
            let rollover = load_keys(ROLLOVER_SECRET_ENV)?;
            ensure_world(&manifest_path, &deployer, rollover.owner_pk(), &services).await
        }
        "renewal-address" => {
            if args.next().is_some() {
                return Err(anyhow!("unexpected renewal-address argument"));
            }
            let rollover = load_keys(ROLLOVER_SECRET_ENV)?;
            let manifest = read_manifest(&manifest_path)?;
            let world = manifest.validate(&rollover.secp, &services.params, &services.emulator)?;
            if rollover.owner_pk() != world.rollover_signer {
                return Err(anyhow!(
                    "{ROLLOVER_SECRET_ENV} does not match the manifest rollover signer"
                ));
            }
            let wallet = txbuild::player_vtxo(&rollover, &services.params)?;
            println!("{}", wallet.to_ark_address().encode());
            Ok(())
        }
        "renew" => {
            let target = args.next().ok_or_else(|| {
                anyhow!(
                    "missing renew target: expected tree <tree_id> or player <owner_pubkey> <player_asset>"
                )
            })?;
            match (target.as_str(), args.next(), args.next(), args.next()) {
                ("tree", Some(tree_id), None, None) => {
                    let tree_id = tree_id.parse::<u32>().context("parse tree id")?;
                    let rollover = load_keys(ROLLOVER_SECRET_ENV)?;
                    renew_tree(&manifest_path, &rollover, &services, tree_id).await
                }
                ("player", Some(owner), Some(player_asset), None) => {
                    let rollover = load_keys(ROLLOVER_SECRET_ENV)?;
                    let owner = owner
                        .parse::<XOnlyPublicKey>()
                        .context("parse player owner public key")?;
                    let player_asset = player_asset
                        .parse::<AssetId>()
                        .context("parse PLAYER_ID asset")?;
                    renew_player(
                        &manifest_path,
                        &rollover,
                        &services,
                        owner,
                        player_asset,
                    )
                    .await
                }
                _ => Err(anyhow!(
                    "invalid renew target: expected tree <tree_id> or player <owner_pubkey> <player_asset>"
                )),
            }
        }
        "renew-once" => {
            require_mode_flag(STARTUP_RENEWAL_ENV, "automatic tree renewal")?;
            if args.next().is_some() {
                return Err(anyhow!("unexpected renew-once argument"));
            }
            let rollover = load_keys(ROLLOVER_SECRET_ENV)?;
            let (renewed, missing_expiry) =
                renew_world(&manifest_path, &rollover, &services).await?;
            println!("{{\"renewed\":{renewed},\"missingExpiry\":{missing_expiry}}}");
            Ok(())
        }
        "watch" => {
            if args.next().is_some() {
                return Err(anyhow!("unexpected watch argument"));
            }
            let rollover = load_keys(ROLLOVER_SECRET_ENV)?;
            let mut last_error = match renew_world(&manifest_path, &rollover, &services).await {
                Ok((initial_renewed, _)) => {
                    if initial_renewed > 0 {
                        eprintln!("woodland.sh rolled over {initial_renewed} tree(s)");
                    }
                    None
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    eprintln!("woodland.sh renewal watcher: {message}");
                    Some(message)
                }
            };
            eprintln!("woodland.sh renewal watcher ready");
            let mut next_rollover = tokio::time::Instant::now()
                + std::time::Duration::from_secs(TREE_ROLLOVER_CHECK_SECS);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let current_services = match connect_services().await {
                    Ok(services) => services,
                    Err(error) => {
                        let message = format!("{error:#}");
                        if last_error.as_deref() != Some(&message) {
                            eprintln!("woodland.sh renewal reconnect: {message}");
                        }
                        last_error = Some(message);
                        continue;
                    }
                };
                if tokio::time::Instant::now() < next_rollover {
                    continue;
                }
                next_rollover = tokio::time::Instant::now()
                    + std::time::Duration::from_secs(TREE_ROLLOVER_CHECK_SECS);
                let result =
                    renew_expiring_trees(&manifest_path, &rollover, &current_services).await;
                match result {
                    Ok((renewed, _)) => {
                        if last_error.take().is_some() {
                            eprintln!("woodland.sh renewal watcher recovered");
                        }
                        if renewed > 0 {
                            eprintln!("woodland.sh rolled over {renewed} tree(s)");
                        }
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        if last_error.as_deref() != Some(&message) {
                            eprintln!("woodland.sh renewal watcher: {message}");
                        }
                        last_error = Some(message);
                    }
                }
            }
        }
        _ => Err(anyhow!("unknown bootstrap command {command}")),
    }
}

fn validate_service_urls(
    network: bitcoin::Network,
    arkade_url: &str,
    emulator_url: &str,
) -> Result<()> {
    for (label, value) in [("Arkade", arkade_url), ("emulator", emulator_url)] {
        let parsed =
            reqwest::Url::parse(value).with_context(|| format!("parse {label} service URL"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || parsed.path() != "/"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(anyhow!(
                "{label} service URL is not a canonical HTTP origin"
            ));
        }
        if network == bitcoin::Network::Bitcoin && parsed.scheme() != "https" {
            return Err(anyhow!("mainnet {label} service URL must use HTTPS"));
        }
    }
    Ok(())
}

async fn connect_services() -> Result<Services> {
    let arkade_url =
        std::env::var(ARKADE_SERVICE_URL_ENV).unwrap_or_else(|_| crate::REGTEST_SERVER.to_string());
    let emulator_url =
        std::env::var(EMULATOR_URL_ENV).unwrap_or_else(|_| crate::REGTEST_EMULATOR.to_string());
    let expected_network = match std::env::var(NETWORK_ENV)
        .unwrap_or_else(|_| "regtest".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "bitcoin" | "mainnet" => bitcoin::Network::Bitcoin,
        "mutinynet" | "signet" => bitcoin::Network::Signet,
        "testnet" => bitcoin::Network::Testnet,
        "testnet4" => bitcoin::Network::Testnet4,
        "regtest" => bitcoin::Network::Regtest,
        network => return Err(anyhow!("unsupported {NETWORK_ENV} value {network}")),
    };
    validate_service_urls(expected_network, &arkade_url, &emulator_url)?;
    let rest = ArkadeRest::new(&arkade_url);
    let params = rest.get_info().await.context("read Arkade service info")?;
    if params.network != expected_network {
        return Err(anyhow!(
            "Arkade service network {} does not match configured {}",
            params.network,
            expected_network
        ));
    }
    if params.dust_sats != PROTOCOL_DUST_SATS || params.vtxo_min_sats > PROTOCOL_DUST_SATS {
        return Err(anyhow!(
            "woodland.sh requires {PROTOCOL_DUST_SATS}-sat outputs supported by the Arkade service"
        ));
    }
    if params.max_op_return_outputs < 1 {
        return Err(anyhow!(
            "Arkade service does not permit the required extension output"
        ));
    }
    let emulator_rest = EmulatorRest::new(&emulator_url);
    let emulator = emulator_rest
        .get_info()
        .await
        .context("read emulator info")?;
    if emulator.version.trim().is_empty() {
        return Err(anyhow!("emulator did not report a version"));
    }
    let mainnet = expected_network == bitcoin::Network::Bitcoin;
    require_service_pin(
        mainnet,
        "Arkade service",
        params.signer_pk,
        &params.version,
        EXPECTED_ARKADE_SIGNER_ENV,
        EXPECTED_ARKADE_VERSION_ENV,
    )?;
    require_service_pin(
        mainnet,
        "emulator",
        emulator.signer_pk,
        &emulator.version,
        EXPECTED_EMULATOR_SIGNER_ENV,
        EXPECTED_EMULATOR_VERSION_ENV,
    )?;
    Ok(Services {
        arkade_url,
        emulator_url,
        rest,
        emulator_rest,
        params,
        emulator,
    })
}

async fn print_status(path: &Path, keys: &Keys, services: &Services) -> Result<()> {
    if path.is_file() {
        let manifest = read_manifest(path)?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION
            || manifest.protocol_version != PROTOCOL_VERSION
            || manifest.game_id != GAME_ID
        {
            println!("reset-required\t-\t0");
            return Ok(());
        }
        let world = manifest.validate(&keys.secp, &services.params, &services.emulator)?;
        world.verify_indexed_assets(&services.rest).await?;
        require_current_trees(&services.rest, &manifest, &world).await?;
        println!("ready\t-\t0");
        return Ok(());
    }
    let pending_path = plan_path(path);
    if pending_path.is_file() {
        let plan = read_plan(&pending_path)?;
        if plan.manifest.schema_version != MANIFEST_SCHEMA_VERSION
            || plan.manifest.protocol_version != PROTOCOL_VERSION
            || plan.manifest.game_id != GAME_ID
        {
            println!("reset-required\t-\t0");
            return Ok(());
        }
        println!("resume\t-\t0");
        return Ok(());
    }

    let deployer = txbuild::player_vtxo(keys, &services.params)?;
    let script = deployer.script_pubkey().to_hex_string();
    let funding_sats = world_funding_sats(&services.params)?;
    let history = services.rest.get_vtxos(&script, "").await?;
    if history.iter().any(|record| !record.assets.is_empty()) {
        return Err(anyhow!(
            "deployer asset history exists without a world manifest; recover it or run ./scripts/regtest.sh clean --force"
        ));
    }
    let records = services.rest.get_vtxos(&script, "spendableOnly").await?;
    if select_funding(&records, funding_sats).is_some() {
        println!("funded\t-\t0");
    } else {
        let balance = clean_funding_balance(&records)?;
        let missing = funding_sats.checked_sub(balance).ok_or_else(|| {
            anyhow!("deployer has {balance} clean sats but no exact {funding_sats}-sat funding set")
        })?;
        println!(
            "needs-funding\t{}\t{}",
            deployer.to_ark_address().encode(),
            missing
        );
    }
    Ok(())
}

async fn ensure_world(
    path: &Path,
    deployer_keys: &Keys,
    rollover_signer: XOnlyPublicKey,
    services: &Services,
) -> Result<()> {
    if path.is_file() {
        let manifest = read_manifest(path)?;
        let world = manifest.validate(&deployer_keys.secp, &services.params, &services.emulator)?;
        world.verify_indexed_assets(&services.rest).await?;
        require_current_trees(&services.rest, &manifest, &world).await?;
        println!("woodland.sh world ready: {}", manifest.genesis_txid);
        return Ok(());
    }

    let pending_path = plan_path(path);
    let plan = if pending_path.is_file() {
        read_plan(&pending_path)?
    } else {
        let deployer = txbuild::player_vtxo(deployer_keys, &services.params)?;
        let script = deployer.script_pubkey().to_hex_string();
        let funding = wait_for_funding(
            &services.rest,
            &script,
            world_funding_sats(&services.params)?,
        )
        .await?;
        let plan = build_plan(deployer_keys, services, &deployer, rollover_signer, funding)?;
        write_json_atomic(&pending_path, &serde_json::to_string_pretty(&plan)?)?;
        plan
    };

    execute_plan(path, &pending_path, deployer_keys, services, plan).await
}

/// Funding covers one dust per recursive tree.
fn world_funding_sats(params: &ServerParams) -> Result<u64> {
    params
        .dust_sats
        .checked_mul(TREE_COUNT as u64)
        .ok_or_else(|| anyhow!("world tree funding amount overflow"))
}

fn build_plan(
    keys: &Keys,
    services: &Services,
    deployer: &ark_core::Vtxo,
    rollover_signer: XOnlyPublicKey,
    funding: Vec<VtxoRecord>,
) -> Result<BootstrapPlan> {
    let address = deployer.to_ark_address();
    let tree_states = tree_states();
    let tree_count = tree_states.len();
    let funding_sats = world_funding_sats(&services.params)?;
    if LOG_RESERVE_PER_TREE
        .checked_mul(tree_count as u64)
        .filter(|total| *total == crate::world::LOG_SUPPLY)
        .is_none()
        || XP_PER_TREE
            .checked_mul(tree_count as u64)
            .filter(|total| *total == crate::world::XP_SUPPLY)
            .is_none()
    {
        return Err(anyhow!("per-tree reserves do not exhaust the fixed supply"));
    }
    let funding_total = funding.iter().try_fold(0_u64, |total, record| {
        total
            .checked_add(record.amount_sats)
            .ok_or_else(|| anyhow!("world funding amount overflow"))
    })?;
    if funding_total != funding_sats {
        return Err(anyhow!(
            "world funding inputs do not equal the required amount"
        ));
    }
    let receivers = [SendReceiver::bitcoin(
        address,
        Amount::from_sat(funding_sats),
    )];
    let inputs = funding
        .iter()
        .map(|record| txbuild::vtxo_input(record, deployer))
        .collect::<Result<Vec<_>>>()?;
    let mut issuance = build_offchain_transactions(
        &receivers,
        &address,
        &inputs,
        &txbuild::server_info(&services.params),
    )
    .map_err(|error| anyhow!("build world asset genesis: {error}"))?;
    let genesis_group = |label: &str, amount: u64| AssetGroup {
        asset_id: None,
        control_asset: None,
        metadata: Some(asset_metadata_entries(
            label,
            keys.owner_pk(),
            rollover_signer,
        )),
        inputs: Vec::new(),
        outputs: vec![AssetOutput {
            output_index: 0,
            amount,
        }],
    };
    ark_core::asset::packet::add_asset_packet_to_psbt(
        &mut issuance.ark_tx,
        &Packet {
            groups: vec![
                genesis_group("TREE", tree_count as u64),
                genesis_group("LOG", crate::world::LOG_SUPPLY),
                genesis_group("XP", crate::world::XP_SUPPLY),
            ],
        },
    )
    .map_err(|error| anyhow!("attach world asset genesis packet: {error}"))?;

    let genesis_txid = issuance.ark_tx.unsigned_tx.compute_txid();
    let tree_asset = AssetId {
        txid: genesis_txid,
        group_index: 0,
    };
    let log_asset = AssetId {
        txid: genesis_txid,
        group_index: 1,
    };
    let xp_asset = AssetId {
        txid: genesis_txid,
        group_index: 2,
    };
    let contract = tree::build_tree_contract(
        &keys.secp,
        services.params.signer_pk,
        services.emulator.signer_pk,
        rollover_signer,
        services.params.unilateral_exit_delay,
        services.params.network,
        tree_asset,
        log_asset,
        xp_asset,
        services.params.dust_sats,
    )?;
    let tree_value = services.params.dust_sats;
    let treasury_assets = |remaining_trees: u64| {
        [
            Asset {
                asset_id: tree_asset,
                amount: remaining_trees,
            },
            Asset {
                asset_id: log_asset,
                amount: LOG_RESERVE_PER_TREE * remaining_trees,
            },
            Asset {
                asset_id: xp_asset,
                amount: XP_PER_TREE * remaining_trees,
            },
        ]
        .to_vec()
    };

    let shard_counts = tree_states
        .chunks(DEPLOYMENT_SHARD_SIZE)
        .map(|states| states.len() as u64)
        .collect::<Vec<_>>();
    let issuance_record = VtxoRecord {
        outpoint: OutPoint {
            txid: genesis_txid,
            vout: 0,
        },
        script: deployer.script_pubkey(),
        amount_sats: funding_sats,
        assets: treasury_assets(tree_count as u64),
        created_at: Some(1),
        expires_at: Some(i64::MAX),
        is_preconfirmed: false,
        is_swept: false,
        spent_by: None,
        settled_by: None,
        is_unrolled: false,
        is_spent: false,
    };
    let distribution_receivers = shard_counts
        .iter()
        .map(|count| {
            tree_value
                .checked_mul(*count)
                .map(|sats| SendReceiver::bitcoin(address, Amount::from_sat(sats)))
                .ok_or_else(|| anyhow!("deployment shard amount overflow"))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut distribution = build_offchain_transactions(
        &distribution_receivers,
        &address,
        &[txbuild::vtxo_input(&issuance_record, deployer)?],
        &txbuild::server_info(&services.params),
    )
    .map_err(|error| anyhow!("build world treasury distribution: {error}"))?;
    let shard_outputs = |scale: u64| {
        shard_counts
            .iter()
            .enumerate()
            .map(|(index, count)| (index as u16, scale * count))
            .collect::<Vec<_>>()
    };
    ark_core::asset::packet::add_asset_packet_to_psbt(
        &mut distribution.ark_tx,
        &Packet {
            groups: vec![
                transfer_group(tree_asset, vec![(0, tree_count as u64)], shard_outputs(1)),
                transfer_group(
                    log_asset,
                    vec![(0, crate::world::LOG_SUPPLY)],
                    shard_outputs(LOG_RESERVE_PER_TREE),
                ),
                transfer_group(
                    xp_asset,
                    vec![(0, crate::world::XP_SUPPLY)],
                    shard_outputs(XP_PER_TREE),
                ),
            ],
        },
    )
    .map_err(|error| anyhow!("attach world treasury distribution assets: {error}"))?;
    let distribution_txid = distribution.ark_tx.unsigned_tx.compute_txid();

    let mut shards = Vec::with_capacity(shard_counts.len());
    let mut manifest_deployments = Vec::with_capacity(tree_count);
    for (shard_index, states) in tree_states.chunks(DEPLOYMENT_SHARD_SIZE).enumerate() {
        let shard_count = states.len();
        let shard_sats = tree_value
            .checked_mul(shard_count as u64)
            .ok_or_else(|| anyhow!("deployment shard amount overflow"))?;
        let shard_source = OutPoint {
            txid: distribution_txid,
            vout: shard_index as u32,
        };
        let mut treasury_record = Some(VtxoRecord {
            outpoint: shard_source,
            script: deployer.script_pubkey(),
            amount_sats: shard_sats,
            assets: treasury_assets(shard_count as u64),
            created_at: Some(1),
            expires_at: Some(i64::MAX),
            is_preconfirmed: false,
            is_swept: false,
            spent_by: None,
            settled_by: None,
            is_unrolled: false,
            is_spent: false,
        });
        let mut deployments = Vec::with_capacity(shard_count);
        for (index, state) in states.iter().copied().enumerate() {
            let remaining_before = (shard_count - index) as u64;
            let remaining_after = remaining_before - 1;
            let current = treasury_record
                .take()
                .ok_or_else(|| anyhow!("deployment shard exhausted before final tree"))?;
            let source_outpoint = current.outpoint;
            let treasury_sats = tree_value
                .checked_mul(remaining_after)
                .ok_or_else(|| anyhow!("treasury amount overflow"))?;
            let mut receivers = vec![SendReceiver::bitcoin(
                contract.vtxo.to_ark_address(),
                Amount::from_sat(tree_value),
            )];
            if remaining_after > 0 {
                receivers.push(SendReceiver::bitcoin(
                    address,
                    Amount::from_sat(treasury_sats),
                ));
            }
            let mut deployment = build_offchain_transactions(
                &receivers,
                &address,
                &[txbuild::vtxo_input(&current, deployer)?],
                &txbuild::server_info(&services.params),
            )
            .map_err(|error| anyhow!("build world tree {} deployment: {error}", state.tree_id))?;
            let mut tree_outputs = vec![(0, 1)];
            let mut log_outputs = vec![(0, LOG_RESERVE_PER_TREE)];
            let mut xp_outputs = vec![(0, XP_PER_TREE)];
            if remaining_after > 0 {
                tree_outputs.push((1, remaining_after));
                log_outputs.push((1, LOG_RESERVE_PER_TREE * remaining_after));
                xp_outputs.push((1, XP_PER_TREE * remaining_after));
            }
            let groups = vec![
                transfer_group(tree_asset, vec![(0, remaining_before)], tree_outputs),
                transfer_group(
                    log_asset,
                    vec![(0, LOG_RESERVE_PER_TREE * remaining_before)],
                    log_outputs,
                ),
                transfer_group(
                    xp_asset,
                    vec![(0, XP_PER_TREE * remaining_before)],
                    xp_outputs,
                ),
            ];
            ark_core::asset::packet::add_asset_packet_to_psbt(
                &mut deployment.ark_tx,
                &Packet { groups },
            )
            .map_err(|error| anyhow!("attach world tree {} assets: {error}", state.tree_id))?;
            tree::attach_tree_state_packet(&mut deployment.ark_tx, state)?;
            tree::attach_tree_health_packet(
                &mut deployment.ark_tx,
                tree::TreeHealth::new(ACTIVE_LOGS_PER_TREE)?,
            )?;
            let deployment_txid = deployment.ark_tx.unsigned_tx.compute_txid();
            manifest_deployments.push((state, deployment_txid));
            deployments.push(PlannedDeployment {
                state,
                source_outpoint: source_outpoint.to_string(),
                ark: encode_psbt(&deployment.ark_tx),
                checkpoints: deployment.checkpoint_txs.iter().map(encode_psbt).collect(),
            });
            if remaining_after > 0 {
                treasury_record = Some(VtxoRecord {
                    outpoint: OutPoint {
                        txid: deployment_txid,
                        vout: 1,
                    },
                    script: deployer.script_pubkey(),
                    amount_sats: treasury_sats,
                    assets: treasury_assets(remaining_after),
                    created_at: Some(1),
                    expires_at: Some(i64::MAX),
                    is_preconfirmed: false,
                    is_swept: false,
                    spent_by: None,
                    settled_by: None,
                    is_unrolled: false,
                    is_spent: false,
                });
            }
        }
        if treasury_record.is_some() {
            return Err(anyhow!("deployment shard retains a treasury output"));
        }
        shards.push(PlannedShard {
            source_outpoint: shard_source.to_string(),
            tree_count: shard_count as u64,
            deployments,
        });
    }
    let manifest = WorldManifest::new(
        &services.params,
        &services.emulator,
        keys,
        &services.arkade_url,
        &services.emulator_url,
        rollover_signer,
        tree_asset,
        log_asset,
        xp_asset,
        &contract,
        genesis_txid,
        &manifest_deployments,
    )?;
    Ok(BootstrapPlan {
        manifest,
        deployer_script: deployer.script_pubkey().to_hex_string(),
        funding_outpoints: funding
            .iter()
            .map(|record| record.outpoint.to_string())
            .collect(),
        issuance_ark: encode_psbt(&issuance.ark_tx),
        issuance_checkpoints: issuance.checkpoint_txs.iter().map(encode_psbt).collect(),
        distribution_ark: encode_psbt(&distribution.ark_tx),
        distribution_checkpoints: distribution
            .checkpoint_txs
            .iter()
            .map(encode_psbt)
            .collect(),
        shards,
    })
}

#[allow(clippy::too_many_arguments)]
async fn execute_plan(
    manifest_path: &Path,
    pending_path: &Path,
    deployer_keys: &Keys,
    services: &Services,
    plan: BootstrapPlan,
) -> Result<()> {
    let world =
        plan.manifest
            .validate(&deployer_keys.secp, &services.params, &services.emulator)?;
    let deployer = txbuild::player_vtxo(deployer_keys, &services.params)?;
    let planned_count = plan
        .shards
        .iter()
        .map(|shard| shard.deployments.len())
        .sum::<usize>();
    if plan.deployer_script != deployer.script_pubkey().to_hex_string()
        || planned_count != world.trees.len()
    {
        return Err(anyhow!("bootstrap plan keys or deployment count mismatch"));
    }
    if decode_psbt(&plan.issuance_ark)?.unsigned_tx.compute_txid() != world.genesis_txid {
        return Err(anyhow!(
            "bootstrap issuance transaction does not match its world manifest"
        ));
    }
    let distribution = decode_psbt(&plan.distribution_ark)?;
    let distribution_txid = distribution.unsigned_tx.compute_txid();
    let distribution_checkpoints = decode_psbts(&plan.distribution_checkpoints)?;
    let genesis_outpoint = OutPoint {
        txid: world.genesis_txid,
        vout: 0,
    };
    if !distribution_checkpoints.iter().any(|checkpoint| {
        checkpoint
            .unsigned_tx
            .input
            .first()
            .is_some_and(|input| input.previous_output == genesis_outpoint)
    }) {
        return Err(anyhow!(
            "bootstrap distribution does not spend world genesis"
        ));
    }
    let mut world_index = 0;
    for (shard_index, shard) in plan.shards.iter().enumerate() {
        let shard_source =
            OutPoint::from_str(&shard.source_outpoint).context("parse planned shard source")?;
        if shard_source
            != (OutPoint {
                txid: distribution_txid,
                vout: shard_index as u32,
            })
            || shard.tree_count != shard.deployments.len() as u64
        {
            return Err(anyhow!("bootstrap shard does not match its distribution"));
        }
        let mut expected_source = shard_source;
        for planned in &shard.deployments {
            let expected = world
                .trees
                .get(world_index)
                .ok_or_else(|| anyhow!("bootstrap plan has too many trees"))?;
            world_index += 1;
            let txid = decode_psbt(&planned.ark)?.unsigned_tx.compute_txid();
            let source = OutPoint::from_str(&planned.source_outpoint)
                .context("parse planned treasury source")?;
            if planned.state != expected.state
                || txid != expected.deployment_txid
                || source != expected_source
            {
                return Err(anyhow!("bootstrap plan does not match its world manifest"));
            }
            expected_source = OutPoint { txid, vout: 1 };
        }
    }

    let deployment_outpoints = world
        .trees
        .iter()
        .map(|tree| OutPoint {
            txid: tree.deployment_txid,
            vout: 0,
        })
        .collect::<Vec<_>>();
    let deployed_records = services
        .rest
        .get_vtxos_by_outpoints(&deployment_outpoints)
        .await?;
    // A deployment already spent by a valid mid-genesis chop still counts as
    // deployed: the chopped lineage is valid, and re-submitting the planned
    // deployment would hard-error on the consumed treasury outpoint. The
    // final tree reconciliation judges whether every tree is live.
    let deployed = deployed_records
        .iter()
        .filter(|record| {
            record.script == world.contract.vtxo.script_pubkey()
                && record.asset_amount(world.tree_asset) == Some(1)
        })
        .map(|record| record.outpoint.txid)
        .collect::<std::collections::HashSet<_>>();

    let issuance_outpoint = OutPoint {
        txid: world.genesis_txid,
        vout: 0,
    };
    let mut issuance = services
        .rest
        .get_vtxos_by_outpoints(&[issuance_outpoint])
        .await?
        .into_iter()
        .next();
    if issuance.is_none() && deployed.is_empty() {
        let funding_outpoints = plan
            .funding_outpoints
            .iter()
            .map(|value| OutPoint::from_str(value).context("parse bootstrap funding outpoint"))
            .collect::<Result<Vec<_>>>()?;
        let funding = services
            .rest
            .get_vtxos_by_outpoints(&funding_outpoints)
            .await?;
        if funding.len() != funding_outpoints.len()
            || funding.iter().any(|record| {
                record.is_spent
                    || !record.assets.is_empty()
                    || record.script.to_hex_string() != plan.deployer_script
            })
        {
            return Err(anyhow!(
                "world bootstrap funding is pending or missing; retry shortly or clean regtest"
            ));
        }
        let funding_total = funding.iter().try_fold(0_u64, |total, record| {
            total
                .checked_add(record.amount_sats)
                .ok_or_else(|| anyhow!("world bootstrap funding amount overflow"))
        })?;
        if funding_total != world_funding_sats(&services.params)? {
            return Err(anyhow!("world bootstrap funding amount is invalid"));
        }
        submit_direct(
            deployer_keys,
            &services.rest,
            decode_psbt(&plan.issuance_ark)?,
            decode_psbts(&plan.issuance_checkpoints)?,
        )
        .await?;
        issuance = Some(wait_for_exact_vtxo(&services.rest, issuance_outpoint).await?);
    }
    let issuance =
        issuance.ok_or_else(|| anyhow!("world issuance is missing after bootstrap submission"))?;
    require_asset_amount(
        &issuance,
        world.tree_asset,
        world.trees.len() as u64,
        "world TREE",
    )?;

    let shard_outpoints = plan
        .shards
        .iter()
        .map(|shard| OutPoint::from_str(&shard.source_outpoint))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse deployment shard outpoint")?;
    let mut distributed = services
        .rest
        .get_vtxos_by_outpoints(&shard_outpoints)
        .await?;
    if distributed.len() != plan.shards.len() {
        if issuance.is_spent {
            return Err(anyhow!("world distribution outputs are incomplete"));
        }
        submit_direct(
            deployer_keys,
            &services.rest,
            distribution,
            distribution_checkpoints,
        )
        .await?;
        distributed.clear();
        for outpoint in &shard_outpoints {
            distributed.push(wait_for_exact_vtxo(&services.rest, *outpoint).await?);
        }
    }
    for (record, shard) in distributed.iter().zip(&plan.shards) {
        require_asset_amount(record, world.tree_asset, shard.tree_count, "shard TREE")?;
        require_asset_amount(
            record,
            world.log_asset,
            LOG_RESERVE_PER_TREE * shard.tree_count,
            "shard LOG",
        )?;
        require_asset_amount(
            record,
            world.xp_asset,
            XP_PER_TREE * shard.tree_count,
            "shard XP",
        )?;
    }

    let results = stream::iter(plan.shards.iter().map(|shard| {
        execute_deployment_shard(
            deployer_keys,
            services,
            &world,
            &plan.deployer_script,
            shard,
            &deployed,
        )
    }))
    .buffer_unordered(RENEWAL_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    let mut submitted = 0;
    for result in results {
        submitted += result?;
    }
    world.verify_indexed_assets(&services.rest).await?;

    write_json_atomic(manifest_path, &plan.manifest.to_json()?)?;
    if pending_path.is_file() {
        std::fs::remove_file(pending_path).context("remove completed bootstrap plan")?;
    }
    println!(
        "woodland.sh world deployed: {} trees from {} ({submitted} submitted)",
        plan.manifest.trees.len(),
        plan.manifest.genesis_txid
    );
    Ok(())
}

async fn execute_deployment_shard(
    deployer_keys: &Keys,
    services: &Services,
    world: &crate::world::ValidatedWorld,
    deployer_script: &str,
    shard: &PlannedShard,
    deployed: &std::collections::HashSet<Txid>,
) -> Result<usize> {
    let mut submitted = 0;
    for (index, planned) in shard.deployments.iter().enumerate() {
        let deployment = decode_psbt(&planned.ark)?;
        let txid = deployment.unsigned_tx.compute_txid();
        if deployed.contains(&txid) {
            continue;
        }
        let source = OutPoint::from_str(&planned.source_outpoint)
            .context("parse deployment treasury source")?;
        let asset_record = wait_for_exact_vtxo(&services.rest, source).await?;
        if asset_record.is_spent || asset_record.script.to_hex_string() != deployer_script {
            return Err(anyhow!("deployment shard treasury is not spendable"));
        }
        let remaining = (shard.deployments.len() - index) as u64;
        let expected_value = services
            .params
            .dust_sats
            .checked_mul(remaining)
            .ok_or_else(|| anyhow!("deployment shard value overflow"))?;
        if asset_record.amount_sats != expected_value {
            return Err(anyhow!("deployment shard BTC value is invalid"));
        }
        require_asset_amount(&asset_record, world.tree_asset, remaining, "shard TREE")?;
        require_asset_amount(
            &asset_record,
            world.log_asset,
            LOG_RESERVE_PER_TREE * remaining,
            "shard LOG",
        )?;
        require_asset_amount(
            &asset_record,
            world.xp_asset,
            XP_PER_TREE * remaining,
            "shard XP",
        )?;
        submit_direct(
            deployer_keys,
            &services.rest,
            deployment,
            decode_psbts(&planned.checkpoints)?,
        )
        .await?;
        let tree_record = wait_for_exact_vtxo(&services.rest, OutPoint { txid, vout: 0 }).await?;
        require_asset_amount(&tree_record, world.tree_asset, 1, "deployed TREE")?;
        require_asset_amount(
            &tree_record,
            world.log_asset,
            LOG_RESERVE_PER_TREE,
            "deployed LOG reserve",
        )?;
        require_asset_amount(&tree_record, world.xp_asset, XP_PER_TREE, "deployed XP")?;
        submitted += 1;
    }
    Ok(submitted)
}

async fn require_current_trees(
    rest: &ArkadeRest,
    manifest: &WorldManifest,
    world: &crate::world::ValidatedWorld,
) -> Result<()> {
    load_current_trees(rest, manifest, world).await?;
    Ok(())
}
async fn wait_for_current_trees(
    rest: &ArkadeRest,
    manifest: &WorldManifest,
    world: &crate::world::ValidatedWorld,
) -> Result<Vec<CurrentTree>> {
    let mut last_error = None;
    for attempt in 0..INDEX_ATTEMPTS {
        match load_current_trees(rest, manifest, world).await {
            Ok(trees) => return Ok(trees),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < INDEX_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(INDEX_POLL_MS)).await;
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("tree index reconciliation failed")))
}

async fn load_exact_tree_records(
    rest: &ArkadeRest,
    world: &crate::world::ValidatedWorld,
) -> Result<Vec<VtxoRecord>> {
    world.load_tree_lineage_records(rest, &world.trees).await
}

async fn load_exact_tree_records_for(
    rest: &ArkadeRest,
    world: &crate::world::ValidatedWorld,
    declared: &[crate::world::ValidatedTree],
) -> Result<Vec<VtxoRecord>> {
    world.load_tree_lineage_records(rest, declared).await
}
async fn load_current_trees(
    rest: &ArkadeRest,
    manifest: &WorldManifest,
    world: &crate::world::ValidatedWorld,
) -> Result<Vec<CurrentTree>> {
    let contract = &world.contract;
    let tree_asset = world.tree_asset;
    let log_asset = world.log_asset;
    let xp_asset = world.xp_asset;
    let declared_trees = &world.trees;
    let trees = load_exact_tree_records(rest, world).await?;
    let txids: Vec<_> = trees.iter().map(|record| record.outpoint.txid).collect();
    let transactions = rest.get_virtual_txs(&txids).await?;
    let mut seen_states = std::collections::HashMap::new();
    let mut current = Vec::with_capacity(trees.len());
    for record in trees {
        let logs = record.asset_amount(log_asset).unwrap_or(0);
        let xp_balance = record.asset_amount(xp_asset).unwrap_or(0);
        if record.amount_sats != manifest.dust_sats
            || record.script != contract.vtxo.script_pubkey()
            || record.asset_amount(tree_asset) != Some(1)
            || logs > LOG_RESERVE_PER_TREE
            || xp_balance > XP_PER_TREE
            || logs != xp_balance
            || !record.assets.iter().all(|asset| {
                asset.asset_id == tree_asset
                    || asset.asset_id == log_asset
                    || asset.asset_id == xp_asset
            })
        {
            return Err(anyhow!("shared tree record is invalid"));
        }
        let transaction = transactions
            .get(&record.outpoint.txid)
            .ok_or_else(|| anyhow!("indexer omitted a current tree transaction"))?;
        record.validate_creating_transaction(transaction)?;
        let state = tree::tree_state_from_tx(transaction)?
            .ok_or_else(|| anyhow!("current tree transaction has no state packet"))?;
        let health = tree::tree_health_from_tx(transaction)?
            .ok_or_else(|| anyhow!("current tree transaction has no health packet"))?;
        if health.value() > logs || health.value() > xp_balance {
            return Err(anyhow!("tree health is invalid"));
        }
        let declared = declared_trees
            .iter()
            .find(|tree| tree.state == state)
            .ok_or_else(|| anyhow!("current tree has an unknown identity"))?;
        let is_deployment = record.outpoint.txid == declared.deployment_txid;
        let transition = tree::classify_transition(transaction, is_deployment)?;
        let expected_vout = transition.output_index();
        if record.outpoint.vout != expected_vout
            || (is_deployment && health.value() != ACTIVE_LOGS_PER_TREE)
        {
            return Err(anyhow!("current tree lineage is invalid"));
        }
        if let Some(competing) = seen_states.insert(state, record.outpoint) {
            return Err(anyhow!(
                "tree {} exposes competing spendable lineages {competing} and {}",
                state.tree_id,
                record.outpoint
            ));
        }
        current.push(CurrentTree {
            state,
            health,
            record,
            previous_tx: transaction.clone(),
        });
    }
    if seen_states.len() != declared_trees.len() {
        return Err(anyhow!("not all declared trees are discoverable"));
    }
    current.sort_by_key(|tree| tree.state.tree_id);
    Ok(current)
}

fn tree_rollover_due(
    expires_at: Option<i64>,
    health: u64,
    logs: u64,
    now: i64,
    margin: i64,
) -> bool {
    let Some(expires_at) = expires_at else {
        return false;
    };
    if health == 0 && logs > 0 {
        return true;
    }
    expires_at - now < margin
}

/// Split the live tree lineages into renewal candidates and the records the
/// indexer reports without an expiry. `tree_rollover_due` can never select
/// the latter, so the watcher counts them loudly instead of silently never
/// renewing them.
fn partition_rollover_candidates(
    trees: Vec<CurrentTree>,
    log_asset: AssetId,
    now: i64,
) -> (Vec<(i64, CurrentTree)>, Vec<OutPoint>) {
    let mut candidates = Vec::new();
    let mut missing_expiry = Vec::new();
    for current in trees {
        let Some(expires_at) = current.record.expires_at else {
            missing_expiry.push(current.record.outpoint);
            continue;
        };
        let logs = current.record.asset_amount(log_asset).unwrap_or(0);
        if !tree_rollover_due(
            Some(expires_at),
            current.health.value(),
            logs,
            now,
            current.record.rollover_margin_seconds(),
        ) {
            continue;
        }
        candidates.push((expires_at, current));
    }
    candidates.sort_by_key(|(expires_at, current)| (*expires_at, current.state.tree_id));
    (candidates, missing_expiry)
}

async fn renew_expiring_trees(
    path: &Path,
    participant_keys: &Keys,
    services: &Services,
) -> Result<(usize, usize)> {
    let manifest = read_manifest(path)?;
    let world = manifest.validate(&participant_keys.secp, &services.params, &services.emulator)?;
    let trees = wait_for_current_trees(&services.rest, &manifest, &world).await?;
    let now = crate::arkade::now_unix();
    let (candidates, missing_expiry) = partition_rollover_candidates(trees, world.log_asset, now);
    if !missing_expiry.is_empty() {
        eprintln!(
            "woodland.sh rollover: {} live tree(s) have no indexed expiry and cannot be renewed",
            missing_expiry.len()
        );
    }
    let world_ref = &world;
    let candidate_count = candidates.len();
    let mut attempted = 0;
    let mut renewed = 0;
    let mut failures = Vec::new();
    let mut paused_for_arkd_ban = false;
    // A fee-funded renewal spends and recreates one ordinary wallet VTXO.
    // Serialize those renewals so concurrent intents cannot select the same
    // funding outpoint. Zero-fee renewals retain bounded batch concurrency.
    let renewal_concurrency = if services.params.zero_offchain_fees {
        RENEWAL_CONCURRENCY
    } else {
        1
    };
    // Keep each correlated expiry wave inside one bounded Ark round. Launching
    // another round after Arkd bans the shared covenant script only amplifies
    // the outage and cannot renew any tree until that temporary ban expires.
    for chunk in candidates.chunks(renewal_concurrency) {
        attempted += chunk.len();
        let results = stream::iter(chunk.iter().map(|(_, current)| {
            let world = world_ref;
            async move {
                (
                    current.state.tree_id,
                    renew_current_tree(
                        participant_keys,
                        services,
                        world,
                        &current.record,
                        &current.previous_tx,
                    )
                    .await,
                )
            }
        }))
        .buffer_unordered(renewal_concurrency)
        .collect::<Vec<_>>()
        .await;
        for (tree_id, result) in results {
            match result {
                Ok(_) => renewed += 1,
                Err(error) => {
                    let arkd_banned = error
                        .chain()
                        .any(|cause| cause.to_string().contains("VTXO_BANNED"));
                    let message = if arkd_banned {
                        format!(
                            "tree {tree_id}: arkd temporarily rejected the shared covenant script \
                             (VTXO_BANNED); see arkd logs for the signing failure"
                        )
                    } else {
                        format!("tree {tree_id}: {error:#}")
                    };
                    eprintln!("woodland.sh rollover failed: {message}");
                    failures.push(message);
                    paused_for_arkd_ban |= arkd_banned;
                }
            }
        }
        if paused_for_arkd_ban {
            break;
        }
    }
    if paused_for_arkd_ban {
        eprintln!(
            "woodland.sh rollover paused after arkd ban: {renewed} renewed, {} failed, {} deferred",
            failures.len(),
            candidate_count - attempted
        );
    }
    if !failures.is_empty() {
        return Err(anyhow!(
            "tree rollover did not finish cleanly: {}",
            failures.join("; ")
        ));
    }
    wait_for_current_trees(&services.rest, &manifest, &world)
        .await
        .context("verify the sole live lineage for every tree after rollover")?;
    Ok((renewed, missing_expiry.len()))
}

async fn renew_world(
    path: &Path,
    participant_keys: &Keys,
    services: &Services,
) -> Result<(usize, usize)> {
    renew_expiring_trees(path, participant_keys, services).await
}

fn clean_funding_balance(records: &[VtxoRecord]) -> Result<u64> {
    records
        .iter()
        .filter(|record| record.assets.is_empty())
        .try_fold(0_u64, |total, record| {
            total
                .checked_add(record.amount_sats)
                .ok_or_else(|| anyhow!("deployer funding balance overflow"))
        })
}

fn select_funding(records: &[VtxoRecord], amount: u64) -> Option<Vec<VtxoRecord>> {
    if let Some(record) = records
        .iter()
        .find(|record| record.amount_sats == amount && record.assets.is_empty())
    {
        return Some(vec![record.clone()]);
    }
    let mut funding = records
        .iter()
        .filter(|record| record.assets.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    funding.sort_by_key(|record| record.outpoint);
    clean_funding_balance(&funding)
        .ok()
        .filter(|total| *total == amount)
        .map(|_| funding)
}

async fn wait_for_funding(rest: &ArkadeRest, script: &str, amount: u64) -> Result<Vec<VtxoRecord>> {
    for _ in 0..INDEX_ATTEMPTS {
        let records = rest.get_vtxos(script, "spendableOnly").await?;
        if let Some(funding) = select_funding(&records, amount) {
            return Ok(funding);
        }
        tokio::time::sleep(std::time::Duration::from_millis(INDEX_POLL_MS)).await;
    }
    Err(anyhow!("world deployer has not received {amount} sats"))
}

async fn wait_for_exact_vtxo(rest: &ArkadeRest, outpoint: OutPoint) -> Result<VtxoRecord> {
    for _ in 0..INDEX_ATTEMPTS {
        if let Some(record) = rest
            .get_vtxos_by_outpoints(&[outpoint])
            .await?
            .into_iter()
            .next()
        {
            return Ok(record);
        }
        tokio::time::sleep(std::time::Duration::from_millis(INDEX_POLL_MS)).await;
    }
    Err(anyhow!("indexer did not expose exact VTXO {outpoint}"))
}

async fn submit_direct(
    keys: &Keys,
    rest: &ArkadeRest,
    ark_tx: Psbt,
    checkpoints: Vec<Psbt>,
) -> Result<Txid> {
    let expected_txid = ark_tx.unsigned_tx.compute_txid();
    let txid = match txbuild::run_tx(keys, rest, ark_tx, checkpoints).await? {
        txbuild::RunTxStatus::Finalized(txid) => txid,
        txbuild::RunTxStatus::Pending(pending) => {
            txbuild::finalize_pending(keys, rest, &pending).await?;
            pending.txid
        }
        txbuild::RunTxStatus::SubmissionUnknown(unknown) => {
            return Err(anyhow!(
                "submission outcome is unknown for {}: {}",
                unknown.txid,
                unknown.last_error
            ));
        }
    };
    if txid != expected_txid {
        return Err(anyhow!("finalized transaction ID changed"));
    }
    Ok(txid)
}

fn transfer_group(
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

fn require_asset_amount(
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

fn encode_psbt(psbt: &Psbt) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(psbt.serialize())
}

fn decode_psbt(encoded: &str) -> Result<Psbt> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("decode bootstrap PSBT base64")?;
    Psbt::deserialize(&bytes).context("decode bootstrap PSBT")
}

fn decode_psbts(encoded: &[String]) -> Result<Vec<Psbt>> {
    encoded.iter().map(|value| decode_psbt(value)).collect()
}

fn read_manifest(path: &Path) -> Result<WorldManifest> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("read world manifest {}", path.display()))?;
    WorldManifest::from_json(&json)
}

/// Emulator-approve and batch-settle one exact tree state transition. Funded
/// stump regrowth is permissionless; maintenance is authorized by rollover.
async fn run_one_renewal(
    participant_keys: &Keys,
    services: &Services,
    world: &crate::world::ValidatedWorld,
    prepared: crate::renewal::RenewalIntent,
    previous_tx: &bitcoin::Transaction,
) -> Result<crate::batch::RenewalOutcome> {
    let batch_services = crate::batch::BatchServices::connect(
        &services.arkade_url,
        services.emulator_rest.clone(),
        services.params.clone(),
        world.pins.clone(),
    )
    .await?;
    let fee_funding = if batch_services.renewal_requires_fee(&prepared)? {
        crate::batch::find_renewal_fee_funding(
            &services.rest,
            participant_keys,
            &services.params,
            prepared.state_outpoint(),
        )
        .await?
    } else {
        None
    };
    batch_services
        .settle_renewal(
            participant_keys,
            services.emulator.signer_pk,
            prepared,
            previous_tx,
            fee_funding.as_ref().map(|funding| funding.source()),
        )
        .await
}

/// Wait for the indexer to expose the renewed VTXO, then return its record.
async fn wait_for_record(
    rest: &ArkadeRest,
    script_hex: &str,
    outpoint: OutPoint,
) -> Result<VtxoRecord> {
    for _ in 0..INDEX_ATTEMPTS {
        let records = rest.get_vtxos(script_hex, "spendableOnly").await?;
        if let Some(record) = records.iter().find(|record| record.outpoint == outpoint) {
            return Ok(record.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(INDEX_POLL_MS)).await;
    }
    Err(anyhow!("renewed VTXO {outpoint} was not indexed"))
}

fn force_rollover_enabled() -> bool {
    std::env::var(FORCE_ROLLOVER_ENV).as_deref() == Ok("1")
}

/// Forced renewals are an operator rescue path: they may run while the input
/// is still live even inside the usual safety margin.
fn renewal_expiry_margin_secs() -> i64 {
    if force_rollover_enabled() {
        0
    } else {
        crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
    }
}

fn require_rollover_due(record: &VtxoRecord, health: tree::TreeHealth, logs: u64) -> Result<()> {
    if force_rollover_enabled()
        || tree_rollover_due(
            record.expires_at,
            health.value(),
            logs,
            crate::arkade::now_unix(),
            record.rollover_margin_seconds(),
        )
    {
        return Ok(());
    }
    Err(anyhow!("tree renewal or regrowth is not due"))
}

/// Result of one settled tree renewal, for CLI reporting.
struct RenewedTree {
    tree_id: u32,
    old_outpoint: OutPoint,
    old_expires_at: Option<i64>,
    new_outpoint: OutPoint,
    new_expires_at: Option<i64>,
    commitment_txid: Txid,
}

/// Prepare, settle, and verify one exact-self-send tree renewal from an
/// already-validated live tree record and its creating transaction.
async fn renew_current_tree(
    participant_keys: &Keys,
    services: &Services,
    world: &crate::world::ValidatedWorld,
    record: &VtxoRecord,
    previous_tx: &bitcoin::Transaction,
) -> Result<RenewedTree> {
    let tree_id = crate::renewal::tree_state_from_tx(previous_tx)?
        .map(|state| state.tree_id)
        .ok_or_else(|| anyhow!("tree transaction has no state packet"))?;
    let tree_script = world.contract.vtxo.script_pubkey().to_hex_string();
    let old_expires_at = record.expires_at;
    let prepared = crate::renewal::prepare_tree(
        record,
        previous_tx,
        &world.contract,
        world.tree_asset,
        world.log_asset,
        world.xp_asset,
        services.params.dust_sats,
        renewal_expiry_margin_secs(),
    )?;
    let outcome = run_one_renewal(participant_keys, services, world, prepared, previous_tx).await?;
    let renewed = wait_for_record(&services.rest, &tree_script, outcome.outpoint).await?;
    require_new_expiry(old_expires_at, renewed.expires_at, renewed.outpoint)?;
    Ok(RenewedTree {
        tree_id,
        old_outpoint: record.outpoint,
        old_expires_at,
        new_outpoint: renewed.outpoint,
        new_expires_at: renewed.expires_at,
        commitment_txid: outcome.commitment_txid,
    })
}

async fn renew_tree(
    path: &Path,
    participant_keys: &Keys,
    services: &Services,
    tree_id: u32,
) -> Result<()> {
    let manifest = read_manifest(path)?;
    let world = manifest.validate(&participant_keys.secp, &services.params, &services.emulator)?;
    let declared = world
        .trees
        .iter()
        .find(|tree| tree.state.tree_id == tree_id)
        .ok_or_else(|| anyhow!("no declared tree {tree_id} in this world"))?;
    let mut records =
        load_exact_tree_records_for(&services.rest, &world, std::slice::from_ref(declared)).await?;
    let record = records
        .pop()
        .ok_or_else(|| anyhow!("no live tree {tree_id} in this world"))?;
    let previous_tx = services
        .rest
        .get_virtual_txs(&[record.outpoint.txid])
        .await?
        .remove(&record.outpoint.txid)
        .ok_or_else(|| anyhow!("indexer omitted the tree's creating transaction"))?;
    if !crate::renewal::tree_state_from_tx(&previous_tx)?
        .is_some_and(|state| state == declared.state)
    {
        return Err(anyhow!("live tree {tree_id} has the wrong identity"));
    }
    let health = tree::tree_health_from_tx(&previous_tx)?
        .ok_or_else(|| anyhow!("tree transaction has no health packet"))?;
    require_rollover_due(
        &record,
        health,
        record.asset_amount(world.log_asset).unwrap_or(0),
    )?;
    let renewed =
        renew_current_tree(participant_keys, services, &world, &record, &previous_tx).await?;
    println!(
        "{}",
        serde_json::json!({
            "kind": "tree",
            "treeId": renewed.tree_id,
            "oldOutpoint": renewed.old_outpoint.to_string(),
            "oldExpiresAt": renewed.old_expires_at,
            "newOutpoint": renewed.new_outpoint.to_string(),
            "newExpiresAt": renewed.new_expires_at,
            "commitmentTxid": renewed.commitment_txid.to_string(),
        })
    );
    Ok(())
}

async fn renew_player(
    path: &Path,
    rollover_keys: &Keys,
    services: &Services,
    owner: XOnlyPublicKey,
    player_asset: AssetId,
) -> Result<()> {
    let manifest = read_manifest(path)?;
    let world = manifest.validate(&rollover_keys.secp, &services.params, &services.emulator)?;
    let renewed = crate::watchtower::renew_player(
        rollover_keys,
        crate::watchtower::WatchtowerServices {
            arkade_url: &services.arkade_url,
            rest: &services.rest,
            emulator_rest: &services.emulator_rest,
            params: &services.params,
            emulator: &services.emulator,
        },
        &world,
        owner,
        player_asset,
        std::env::var(FORCE_ROLLOVER_ENV).as_deref() == Ok("1"),
    )
    .await?;
    println!(
        "{}",
        serde_json::json!({
            "kind": "player",
            "owner": owner.to_string(),
            "xp": renewed.xp,
            "playerAsset": player_asset.to_string(),
            "oldOutpoint": renewed.old_outpoint.to_string(),
            "oldExpiresAt": renewed.old_expires_at,
            "newOutpoint": renewed.new_outpoint.to_string(),
            "newExpiresAt": renewed.new_expires_at,
            "commitmentTxid": renewed.commitment_txid.to_string(),
        })
    );
    Ok(())
}

fn require_new_expiry(old: Option<i64>, new: Option<i64>, outpoint: OutPoint) -> Result<()> {
    let old = old.ok_or_else(|| anyhow!("renewal input has no indexed expiry"))?;
    let new = new.ok_or_else(|| anyhow!("renewed VTXO {outpoint} has no indexed expiry"))?;
    if new <= old {
        return Err(anyhow!(
            "renewed VTXO {outpoint} did not extend expiry ({old} -> {new})"
        ));
    }
    Ok(())
}

fn read_plan(path: &Path) -> Result<BootstrapPlan> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("read bootstrap plan {}", path.display()))?;
    serde_json::from_str(&json).context("parse woodland.sh bootstrap plan")
}

fn write_json_atomic(path: &Path, json: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("world manifest path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, json).with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))
}

fn plan_path(manifest_path: &Path) -> PathBuf {
    let stem = manifest_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("woodland-world");
    manifest_path.with_file_name(format!("{stem}-plan.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{Keypair, SecretKey};

    fn signer(byte: u8) -> (XOnlyPublicKey, bitcoin::PublicKey) {
        let keypair = Keypair::from_secret_key(
            &bitcoin::secp256k1::Secp256k1::new(),
            &SecretKey::from_slice(&[byte; 32]).unwrap(),
        );
        (
            keypair.x_only_public_key().0,
            bitcoin::PublicKey::new(keypair.public_key()),
        )
    }

    fn pin<'a>(name: &'a str, value: Option<&'a str>) -> PinSetting<'a> {
        PinSetting { name, value }
    }

    fn funding_record(byte: u8, amount_sats: u64) -> VtxoRecord {
        VtxoRecord {
            outpoint: OutPoint {
                txid: Txid::from_str(&format!("{byte:02x}").repeat(32)).unwrap(),
                vout: 0,
            },
            script: bitcoin::ScriptBuf::new(),
            amount_sats,
            assets: Vec::new(),
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

    #[test]
    fn deployment_funding_accepts_one_or_an_exact_aggregate() {
        let exact = funding_record(1, 158_000);
        let split = [funding_record(2, 100_000), funding_record(3, 58_000)];
        assert_eq!(
            select_funding(std::slice::from_ref(&exact), 158_000)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(select_funding(&split, 158_000).unwrap().len(), 2);
        assert!(select_funding(&split, 158_001).is_none());

        let mut asset_bearing = funding_record(4, 1);
        asset_bearing.assets.push(Asset {
            asset_id: AssetId {
                txid: asset_bearing.outpoint.txid,
                group_index: 0,
            },
            amount: 1,
        });
        let mut records = split.to_vec();
        records.push(asset_bearing);
        assert_eq!(clean_funding_balance(&records).unwrap(), 158_000);
        assert_eq!(select_funding(&records, 158_000).unwrap().len(), 2);
    }

    #[test]
    fn mainnet_service_urls_require_canonical_https_origins() {
        assert!(validate_service_urls(
            bitcoin::Network::Bitcoin,
            "https://arkade.example",
            "https://emulator.example/",
        )
        .is_ok());
        assert!(validate_service_urls(
            bitcoin::Network::Signet,
            "http://127.0.0.1:7070",
            "http://127.0.0.1:7073",
        )
        .is_ok());
        for (arkade, emulator) in [
            ("http://arkade.example", "https://emulator.example"),
            ("https://arkade.example", "http://emulator.example"),
            ("ftp://arkade.example", "https://emulator.example"),
            ("https://user@arkade.example", "https://emulator.example"),
            ("https://arkade.example/path", "https://emulator.example"),
            ("https://arkade.example?query=1", "https://emulator.example"),
            (
                "https://arkade.example#fragment",
                "https://emulator.example",
            ),
        ] {
            assert!(
                validate_service_urls(bitcoin::Network::Bitcoin, arkade, emulator).is_err(),
                "{arkade} {emulator}"
            );
        }
    }

    #[test]
    fn mainnet_service_pins_are_mandatory_and_exact() {
        let (actual, compressed) = signer(3);
        assert!(validate_service_pin(
            false,
            "service",
            actual,
            "v1",
            pin("SIGNER", None),
            pin("VERSION", None),
        )
        .is_ok());
        assert!(validate_service_pin(
            true,
            "service",
            actual,
            "v1",
            pin("SIGNER", None),
            pin("VERSION", None),
        )
        .unwrap_err()
        .to_string()
        .contains("requires explicit SIGNER and VERSION"));
        assert!(validate_service_pin(
            true,
            "service",
            actual,
            "v1",
            pin("SIGNER", Some(&actual.to_string())),
            pin("VERSION", None),
        )
        .is_err());
        assert!(validate_service_pin(
            true,
            "service",
            actual,
            "v1",
            pin("SIGNER", Some(&compressed.to_string())),
            pin("VERSION", Some("v1")),
        )
        .is_ok());
    }

    #[test]
    fn service_pin_mismatch_or_malformed_key_fails_closed() {
        let (actual, _) = signer(3);
        let (other, _) = signer(4);
        assert!(validate_service_pin(
            true,
            "service",
            actual,
            "v1",
            pin("SIGNER", Some(&other.to_string())),
            pin("VERSION", Some("v1")),
        )
        .is_err());
        assert!(validate_service_pin(
            true,
            "service",
            actual,
            "v1",
            pin("SIGNER", Some(&actual.to_string())),
            pin("VERSION", Some("v2")),
        )
        .is_err());
        assert!(validate_service_pin(
            true,
            "service",
            actual,
            "v1",
            pin("SIGNER", Some("not-a-key")),
            pin("VERSION", Some("v1")),
        )
        .is_err());
    }

    fn current_tree(tree_id: u32, expires_at: Option<i64>) -> CurrentTree {
        CurrentTree {
            state: tree::TreeState {
                tree_id,
                x: 0,
                y: 0,
            },
            health: tree::TreeHealth::new(5).unwrap(),
            record: VtxoRecord {
                outpoint: OutPoint {
                    txid: Txid::from_str(&format!("{:064x}", tree_id)).unwrap(),
                    vout: 0,
                },
                script: bitcoin::ScriptBuf::new(),
                amount_sats: 330,
                assets: Vec::new(),
                created_at: Some(1),
                expires_at,
                is_preconfirmed: false,
                is_swept: false,
                spent_by: None,
                settled_by: None,
                is_unrolled: false,
                is_spent: false,
            },
            previous_tx: bitcoin::Transaction {
                version: bitcoin::transaction::Version::TWO,
                lock_time: bitcoin::absolute::LockTime::ZERO,
                input: Vec::new(),
                output: Vec::new(),
            },
        }
    }

    #[test]
    fn rollover_selection_renews_funded_stumps_in_one_batch() {
        let now = 10_000;
        let due = Some(now + 99);
        let far = Some(now + 100);
        assert!(tree_rollover_due(due, 5, 10, now, 100));
        assert!(tree_rollover_due(far, 0, 5, now, 100));
        assert!(tree_rollover_due(due, 0, 0, now, 100));
        assert!(!tree_rollover_due(far, 0, 0, now, 100));
        assert!(!tree_rollover_due(far, 5, 10, now, 100));
        assert!(!tree_rollover_due(None, 0, 5, now, 100));
        assert_eq!(
            plan_path(Path::new("/tmp/mutinynet-season-1.json")),
            PathBuf::from("/tmp/mutinynet-season-1-plan.json")
        );
    }

    #[test]
    fn rollover_selection_counts_live_trees_missing_expiry() {
        let now = 10_000;
        let asset = |byte: u8| AssetId {
            txid: Txid::from_str(&format!("{:064x}", byte)).unwrap(),
            group_index: 0,
        };
        let due = current_tree(1, Some(now + 99));
        let far = current_tree(2, Some(now + 100_000));
        let missing = current_tree(3, None);
        let missing_outpoint = missing.record.outpoint;
        let (candidates, missing_expiry) =
            partition_rollover_candidates(vec![far, due, missing], asset(4), now);
        let candidate_ids = candidates
            .iter()
            .map(|(_, current)| current.state.tree_id)
            .collect::<Vec<_>>();
        assert_eq!(candidate_ids, [1]);
        assert_eq!(missing_expiry, [missing_outpoint]);
    }
}
