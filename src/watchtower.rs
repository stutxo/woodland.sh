//! Unattended player-state renewal through the covenant's exact-self-send leaf.

use crate::arkade::{ArkadeRest, EmulatorParams, EmulatorRest, ServerParams, VtxoRecord};
use crate::batch::BatchServices;
use crate::keys::Keys;
use crate::player;
use crate::renewal;
use crate::world::ValidatedWorld;
use anyhow::{anyhow, Result};
use ark_core::asset::AssetId;
use bitcoin::{OutPoint, XOnlyPublicKey};

const INDEX_ATTEMPTS: usize = 80;
const INDEX_POLL_MS: u64 = 250;

pub struct WatchtowerServices<'a> {
    pub arkade_url: &'a str,
    pub rest: &'a ArkadeRest,
    pub emulator_rest: &'a EmulatorRest,
    pub params: &'a ServerParams,
    pub emulator: &'a EmulatorParams,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerRenewalOutcome {
    pub xp: u64,
    pub old_outpoint: OutPoint,
    pub old_expires_at: Option<i64>,
    pub new_outpoint: OutPoint,
    pub new_expires_at: Option<i64>,
    pub commitment_txid: bitcoin::Txid,
}

fn require_rollover_due(record: &VtxoRecord, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let remaining = record
        .expires_in(crate::arkade::now_unix())
        .ok_or_else(|| anyhow!("rollover input has no indexed expiry"))?;
    let margin = record.rollover_margin_seconds();
    if remaining >= margin {
        return Err(anyhow!(
            "rollover is not due: {remaining}s remain, margin is {margin}s"
        ));
    }
    Ok(())
}

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

pub async fn renew_player(
    rollover_keys: &Keys,
    services: WatchtowerServices<'_>,
    world: &ValidatedWorld,
    owner: XOnlyPublicKey,
    player_asset: AssetId,
    force: bool,
) -> Result<PlayerRenewalOutcome> {
    if rollover_keys.owner_pk() != world.rollover_signer {
        return Err(anyhow!(
            "the configured key is not this world's rollover signer"
        ));
    }
    let contract = player::build_player_contract(
        &rollover_keys.secp,
        owner,
        services.params.signer_pk,
        services.emulator.signer_pk,
        world.rollover_signer,
        services.params.unilateral_exit_delay,
        services.params.network,
        world.tree_asset,
        world.log_asset,
        world.xp_asset,
        services.params.dust_sats,
        &world.contract.vtxo.script_pubkey(),
    )?;
    let script = contract.vtxo.script_pubkey().to_hex_string();
    let records = services.rest.get_vtxos(&script, "spendableOnly").await?;
    let candidates = records
        .into_iter()
        .filter(|record| {
            player::validate_player_state_record(record, &contract, player_asset).is_ok()
        })
        .collect::<Vec<_>>();
    let record = match candidates.as_slice() {
        [record] => record.clone(),
        [] => return Err(anyhow!("no live player state for this owner and PLAYER_ID")),
        _ => return Err(anyhow!("PLAYER_ID has multiple live player states")),
    };
    require_rollover_due(&record, force)?;
    let previous_tx = services
        .rest
        .get_virtual_txs(&[record.outpoint.txid])
        .await?
        .remove(&record.outpoint.txid)
        .ok_or_else(|| anyhow!("indexer omitted the player's creating transaction"))?;
    let previous_state = player::player_state_from_tx(&previous_tx)?
        .ok_or_else(|| anyhow!("player creating transaction has no state packets"))?;
    let old_outpoint = record.outpoint;
    let old_expires_at = record.expires_at;
    let expiry_margin_secs = if force {
        0
    } else {
        crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS
    };
    let prepared = renewal::prepare_player_watchtower(
        &record,
        &previous_tx,
        &contract,
        player_asset,
        expiry_margin_secs,
    )?;
    let batch_services = BatchServices::connect(
        services.arkade_url,
        services.emulator_rest.clone(),
        services.params.clone(),
        world.pins.clone(),
    )
    .await?;
    let outcome = batch_services
        .settle_renewal(
            rollover_keys,
            services.emulator.signer_pk,
            prepared,
            &previous_tx,
        )
        .await?;
    let renewed = wait_for_record(services.rest, &script, outcome.outpoint).await?;
    if renewed.expires_at <= old_expires_at {
        return Err(anyhow!("player renewal did not extend the indexed expiry"));
    }
    let renewed_tx = services
        .rest
        .get_virtual_txs(&[renewed.outpoint.txid])
        .await?
        .remove(&renewed.outpoint.txid)
        .ok_or_else(|| anyhow!("indexer omitted the renewed player transaction"))?;
    renewed.validate_creating_transaction(&renewed_tx)?;
    player::validate_player_state_record(&renewed, &contract, player_asset)?;
    let renewed_state = player::player_state_from_tx(&renewed_tx)?
        .ok_or_else(|| anyhow!("renewed player transaction has no state packets"))?;
    if renewed_state != previous_state {
        return Err(anyhow!("player renewal changed recursive state or XP"));
    }

    Ok(PlayerRenewalOutcome {
        old_outpoint,
        xp: previous_state.xp.value(),
        old_expires_at,
        new_outpoint: renewed.outpoint,
        new_expires_at: renewed.expires_at,
        commitment_txid: outcome.commitment_txid,
    })
}
