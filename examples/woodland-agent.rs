//! Minimal headless woodland.sh agent.
//!
//! Connects with a player key, activates if needed, then chops stocked trees
//! and renews the player state before expiry. It is deliberately small: every
//! protocol decision lives in `woodland::client::WoodlandClient` and the
//! modules behind it, so this file reads as the game loop only.
//!
//! Usage:
//!   cargo run --example woodland-agent --features woodland-app -- \
//!     <world-manifest.json> <player-profile.json> [player-key-hex] [player-asset-id]
//!
//! The profile stores the key, selected PLAYER_ID, and unfinished activation.
//! It is saved durably before any submission; restart with the same file to
//! recover. Optional key/asset arguments import an identity into a new file.
//! Run only one agent per profile. Fund the wallet with one exact dust VTXO
//! (regtest: `./scripts/regtest.sh fund <address> 330`).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use woodland::arkade::now_unix;
use woodland::chop::PreparedActivation;
use woodland::client::WoodlandClient;
use woodland::txbuild::RunTxStatus;
use woodland::world::WorldManifest;
use woodland::Keys;

/// Swing cadence; the reference browser swings once per second.
const SWING_DELAY_MS: u64 = 1_000;
/// Stop after this many successful drops so the example terminates.
const SEASON_BUDGET_LOGS: u64 = 10;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentProfile {
    genesis_txid: String,
    secret_key: String,
    #[serde(default, deserialize_with = "deserialize_player_asset")]
    player_asset: Option<ark_core::asset::AssetId>,
    pending_activation: Option<PreparedActivation>,
}

fn deserialize_player_asset<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<ark_core::asset::AssetId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|encoded| {
            woodland::txbuild::parse_asset_id_pub(&encoded)
                .ok_or_else(|| serde::de::Error::custom("invalid player asset ID"))
        })
        .transpose()
}

fn save_profile(path: &Path, profile: &AgentProfile) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).context("create profile journal")?;
    file.write_all(&serde_json::to_vec_pretty(profile)?)?;
    file.sync_all().context("flush profile journal")?;
    std::fs::rename(&temporary, path).context("replace profile journal")?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?
        .sync_all()
        .context("flush profile directory")?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let manifest_path = args
        .next()
        .context("usage: woodland-agent <manifest> <profile.json> [key-hex] [player-asset]")?;
    let profile_path = args
        .next()
        .context("provide a persistent player profile path")?;
    let profile_path = Path::new(&profile_path);
    let manifest = WorldManifest::from_json(&std::fs::read_to_string(&manifest_path)?)?;
    let mut profile: AgentProfile = match std::fs::read(profile_path) {
        Ok(bytes) => {
            if args.next().is_some() {
                return Err(anyhow!(
                    "identity import arguments require a new profile file"
                ));
            }
            serde_json::from_slice(&bytes).context("decode player profile")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let keys = match args.next() {
                Some(hex) => Keys::from_hex(&hex)?,
                None => Keys::generate()?,
            };
            let profile = AgentProfile {
                genesis_txid: manifest.genesis_txid.clone(),
                secret_key: keys.secret_hex(),
                player_asset: args
                    .next()
                    .map(|value| value.parse())
                    .transpose()
                    .context("parse player asset id")?,
                pending_activation: None,
            };
            save_profile(profile_path, &profile)?;
            profile
        }
        Err(error) => return Err(error).context("read player profile"),
    };
    if profile.genesis_txid != manifest.genesis_txid {
        return Err(anyhow!("player profile belongs to a different world"));
    }
    if profile
        .pending_activation
        .as_ref()
        .is_some_and(|prepared| profile.player_asset != Some(prepared.player_asset))
    {
        return Err(anyhow!(
            "activation journal does not match the selected PLAYER_ID"
        ));
    }
    let keys = Keys::from_hex(&profile.secret_key)?;
    let mut client = WoodlandClient::connect(&manifest, keys, profile.player_asset).await?;
    println!("owner:   {}", client.owner());
    println!("wallet:  {}", client.wallet_address()?);

    if profile.pending_activation.is_none() && client.sync_player().await?.is_none() {
        if profile.player_asset.is_some() {
            return Err(anyhow!("selected player is not indexed; restore its activation journal rather than depositing again"));
        }
        let dust_sats = client.manifest().dust_sats;
        println!("fund the wallet with exactly one clean, live {dust_sats}-sat VTXO, waiting...");
        loop {
            let funding = client
                .wallet_records()
                .await?
                .into_iter()
                .filter(|record| {
                    record.assets.is_empty()
                        && record.amount_sats == dust_sats
                        && record
                            .ensure_live(now_unix(), woodland::arkade::DEFAULT_EXPIRY_MARGIN_SECS)
                            .is_ok()
                })
                .count();
            if funding == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let prepared = client.prepare_activation().await?;
        profile.player_asset = Some(prepared.player_asset);
        profile.pending_activation = Some(prepared);
        // This completes before resume_activation can perform its first submission.
        save_profile(profile_path, &profile)?;
    }
    while let Some(prepared) = profile.pending_activation.as_ref() {
        match client.resume_activation(prepared).await {
            Ok(RunTxStatus::Finalized(txid)) => {
                println!("activated PLAYER_ID {} at {txid}", prepared.player_asset);
                profile.pending_activation = None;
                save_profile(profile_path, &profile)?;
            }
            Ok(RunTxStatus::Pending(_)) => {
                println!("activation accepted, finalization pending; retaining journal");
            }
            Ok(RunTxStatus::SubmissionUnknown(_)) => {
                println!("activation outcome unknown; retaining journal, do not deposit again");
            }
            Err(error) => println!("activation recovery failed; retaining journal: {error:#}"),
        }
        if profile.pending_activation.is_some() {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    let mut logs = 0_u64;
    // Stay on one tree until it is depleted, like the browser's auto-swing;
    // re-scan the world only when a target is needed.
    let mut target: Option<u32> = None;
    'season: while logs < SEASON_BUDGET_LOGS {
        let player = client
            .sync_player()
            .await?
            .ok_or_else(|| anyhow!("player lineage vanished"))?;
        let remaining = player
            .expires_at()
            .map(|expiry| expiry - now_unix())
            .unwrap_or_default();
        if remaining < player.record.rollover_margin_seconds() {
            let outpoint = client.renew_player().await?;
            println!("renewed player state at {outpoint} (was {remaining}s from expiry)");
            continue;
        }

        let tree_id = match target {
            Some(tree_id) => tree_id,
            None => {
                let mut trees = client.trees(&[]).await?;
                trees.retain(|tree| tree.health.value() > 0 && tree.logs > 0);
                trees.sort_by_key(|tree| tree.state.tree_id);
                let Some(nearest) = trees.first() else {
                    println!("no active trees; waiting for renewal or regrowth");
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                };
                let tree_id = nearest.state.tree_id;
                println!("locked on tree #{tree_id} ({} LOG stocked)", nearest.logs);
                target = Some(tree_id);
                tree_id
            }
        };
        match client.chop(tree_id).await {
            Ok(outcome) => {
                if outcome.success {
                    logs += 1;
                    let xp = logs.saturating_mul(client.manifest().woodcutting_xp_per_log);
                    println!(
                        "LOG {logs}/{SEASON_BUDGET_LOGS} from tree #{tree_id} ({xp} Woodcutting XP)"
                    );
                } else {
                    println!("miss on tree #{tree_id}");
                }
                let refreshed = client.tree(tree_id).await?;
                if refreshed.health.value() == 0 || refreshed.logs == 0 {
                    println!("tree #{tree_id} depleted, moving on");
                    target = None;
                }
            }
            Err(error) => {
                println!("swing failed, resyncing: {error:#}");
                target = None;
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue 'season;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(SWING_DELAY_MS)).await;
    }
    let xp = logs.saturating_mul(client.manifest().woodcutting_xp_per_log);
    println!("season budget reached: {logs} LOG and {xp} Woodcutting XP");
    Ok(())
}
