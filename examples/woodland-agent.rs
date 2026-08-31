//! Minimal headless woodland.sh agent.
//!
//! Connects with a player key, activates if needed, then chops stocked trees
//! and renews the player state before expiry. It is deliberately small: every
//! protocol decision lives in `woodland::client::WoodlandClient` and the
//! modules behind it, so this file reads as the game loop only.
//!
//! Usage:
//!   cargo run --example woodland-agent --features woodland-app -- \
//!     <world-manifest.json> [player-key-hex] [player-asset-id]
//!
//! With no key argument a fresh key is generated and printed; persist it.
//! Fund the printed wallet address with exactly one dust-sized VTXO (on
//! regtest: `./scripts/regtest.sh fund <address> 330`).

use anyhow::{anyhow, Context, Result};
use woodland::arkade::now_unix;
use woodland::client::WoodlandClient;
use woodland::world::WorldManifest;
use woodland::Keys;

/// Swing cadence; the reference browser swings once per second.
const SWING_DELAY_MS: u64 = 1_000;
/// Stop after this many successful drops so the example terminates.
const SEASON_BUDGET_LOGS: u64 = 10;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let manifest_path = args
        .next()
        .context("usage: woodland-agent <manifest> [key-hex] [player-asset]")?;
    let keys = match args.next() {
        Some(hex) => Keys::from_hex(&hex)?,
        None => {
            let keys = Keys::generate()?;
            println!("generated player key (persist this): {}", keys.secret_hex());
            keys
        }
    };
    let player_asset = args
        .next()
        .map(|value| value.parse())
        .transpose()
        .context("parse player asset id")?;

    let manifest = WorldManifest::from_json(&std::fs::read_to_string(&manifest_path)?)?;
    let mut client = WoodlandClient::connect(&manifest, keys, player_asset).await?;
    println!("owner:   {}", client.owner());
    println!("wallet:  {}", client.wallet_address()?);

    if client.sync_player().await?.is_none() {
        if player_asset.is_none() {
            println!("no player yet; fund the wallet with exactly one 330-sat VTXO, waiting...");
        }
        let dust_sats = client.manifest().dust_sats;
        loop {
            let funding = client
                .wallet_records()
                .await?
                .into_iter()
                .filter(|record| record.assets.is_empty() && record.amount_sats == dust_sats)
                .count();
            if funding == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let asset = client.activate().await?;
        println!("activated PLAYER_ID {asset} (persist this with the key)");
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
                trees.sort_by_key(|tree| {
                    let position = player.state.position;
                    u32::from(tree.state.x.abs_diff(position.x))
                        + u32::from(tree.state.y.abs_diff(position.y))
                });
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
                    println!("LOG {logs}/{SEASON_BUDGET_LOGS} from tree #{tree_id}");
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
    println!("season budget reached: {logs} LOG == {logs} XP");
    Ok(())
}
