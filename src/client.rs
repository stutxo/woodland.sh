//! Headless woodland.sh client for agents and tooling.
//!
//! The reference browser is one consumer of the protocol modules; this client
//! is the headless equivalent for bots and alternative frontends. It holds no
//! framework and no hidden state: connect with a world manifest and player
//! keys, then drive activation, chopping, axe crafting, regrowth, and renewal
//! through [`WoodlandClient`].
//! Persistence of the player key, PLAYER_ID, and any submission journal is the caller's choice.

use crate::arkade::{ArkadeRest, EmulatorParams, EmulatorRest, ServerParams, VtxoRecord};

use crate::batch::BatchServices;
use crate::chop::{ChopMutation, ChopWorld, PlayerChopState, PreparedActivation, TreeChopState};
use crate::keys::Keys;
use crate::player::{self, PlayerContract, PlayerState};
use crate::tree::{self, TreeHealth, TreeState};
use crate::world::{ValidatedTree, ValidatedWorld, WorldManifest};
use crate::{protocol, renewal, txbuild};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use bitcoin::{OutPoint, Transaction, Txid};

const INDEX_ATTEMPTS: usize = 80;
const INDEX_POLL_MS: u64 = 250;

/// Live player lineage with the balances an agent reasons about.
#[derive(Clone)]
pub struct PlayerSnapshot {
    pub state: PlayerState,
    pub record: VtxoRecord,
    previous_tx: Transaction,
    pub logs: u64,
    pub xp_balance: u64,
    pub stone: u64,
    pub iron_ore: u64,
}

impl PlayerSnapshot {
    pub fn outpoint(&self) -> OutPoint {
        self.record.outpoint
    }

    pub fn expires_at(&self) -> Option<i64> {
        self.record.expires_at
    }
}

/// Live tree lineage used for target selection.
#[derive(Clone)]
pub struct TreeSnapshot {
    pub state: TreeState,
    pub health: TreeHealth,
    pub record: VtxoRecord,
    /// Current LOG reserve; one successful swing moves one unit to the player.
    pub logs: u64,
    pub stone_reserve: u64,
    pub iron_ore_reserve: u64,
    /// Current XP reserve; tracks LOG one-for-one.
    pub xp_reserve: u64,
    previous_tx: Transaction,
}

/// What a settled swing produced.
#[derive(Clone, Copy, Debug)]
pub struct ChopOutcome {
    pub tree_id: u32,
    pub success: bool,
    pub material: player::MaterialDrop,
    pub txid: Txid,
    pub player_outpoint: OutPoint,
    pub tree_outpoint: OutPoint,
}

/// What a settled covenant-enforced axe upgrade produced.
#[derive(Clone, Copy, Debug)]
pub struct CraftOutcome {
    pub axe: player::AxeTier,
    pub txid: Txid,
    pub player_outpoint: OutPoint,
}

pub struct WoodlandClient {
    keys: Keys,
    rest: ArkadeRest,
    emulator: EmulatorRest,
    emulator_params: EmulatorParams,
    params: ServerParams,
    arkade_url: String,
    world: ValidatedWorld,
    manifest: WorldManifest,
    contract: PlayerContract,
    player_asset: Option<AssetId>,
}

impl WoodlandClient {
    /// Connect to the manifest-pinned services, validate the manifest against
    /// them, and rebuild the caller's player contract. `player_asset` is the
    /// caller-persisted PLAYER_ID from a previous activation, if any.
    pub async fn connect(
        manifest: &WorldManifest,
        keys: Keys,
        player_asset: Option<AssetId>,
    ) -> Result<Self> {
        let rest = ArkadeRest::new(&manifest.arkade_service_url);
        let emulator = EmulatorRest::new(&manifest.emulator_url);
        let params = rest.get_info().await.context("read Arkade service info")?;
        let emulator_params = emulator
            .get_info()
            .await
            .context("read emulator service info")?;
        let world = manifest
            .validate(&keys.secp, &params, &emulator_params)
            .context("validate woodland.sh world manifest")?;
        world.verify_indexed_assets(&rest).await?;
        let contract = player::build_player_contract(
            &keys.secp,
            keys.owner_pk(),
            params.signer_pk,
            emulator_params.signer_pk,
            world.rollover_signer,
            params.unilateral_exit_delay,
            params.network,
            world.tree_asset,
            world.log_asset,
            world.xp_asset,
            world.stone_asset,
            world.iron_ore_asset,
            params.dust_sats,
            &world.contract.vtxo.script_pubkey(),
        )?;
        Ok(Self {
            arkade_url: manifest.arkade_service_url.clone(),
            keys,
            rest,
            emulator,
            emulator_params,
            params,
            world,
            manifest: manifest.clone(),
            contract,
            player_asset,
        })
    }

    pub fn owner(&self) -> bitcoin::XOnlyPublicKey {
        self.keys.owner_pk()
    }

    pub fn player_asset(&self) -> Option<AssetId> {
        self.player_asset
    }

    pub fn manifest(&self) -> &WorldManifest {
        &self.manifest
    }
    /// Every declared tree in the world, for [`Self::trees`].
    pub fn declared_trees(&self) -> &[ValidatedTree] {
        &self.world.trees
    }

    /// The ordinary Arkade wallet address to fund for activation.
    pub fn wallet_address(&self) -> Result<String> {
        Ok(txbuild::player_vtxo(&self.keys, &self.params)?
            .to_ark_address()
            .to_string())
    }

    /// Every spendable plain-wallet VTXO (activation funding candidates).
    pub async fn wallet_records(&self) -> Result<Vec<VtxoRecord>> {
        let wallet = txbuild::player_vtxo(&self.keys, &self.params)?;
        self.rest
            .get_vtxos(&wallet.script_pubkey().to_hex_string(), "spendableOnly")
            .await
    }

    /// Discover the live player lineage for the caller's PLAYER_ID.
    pub async fn sync_player(&self) -> Result<Option<PlayerSnapshot>> {
        let Some(player_asset) = self.player_asset else {
            return Ok(None);
        };
        let script = self.contract.vtxo.script_pubkey().to_hex_string();
        let records = self.rest.get_vtxos(&script, "spendableOnly").await?;
        let candidates = records
            .into_iter()
            .filter(|record| {
                player::validate_player_state_record(record, &self.contract, player_asset).is_ok()
            })
            .collect::<Vec<_>>();
        let record = match candidates.as_slice() {
            [record] => record.clone(),
            [] => return Ok(None),
            _ => return Err(anyhow!("PLAYER_ID has multiple live player states")),
        };
        let previous_tx = self
            .rest
            .get_virtual_txs(&[record.outpoint.txid])
            .await?
            .remove(&record.outpoint.txid)
            .ok_or_else(|| anyhow!("indexer omitted the player's creating transaction"))?;
        record.validate_creating_transaction(&previous_tx)?;
        let state = player::player_state_from_tx(&previous_tx)?
            .ok_or_else(|| anyhow!("player creating transaction has no state packets"))?;
        let logs = record.asset_amount(self.world.log_asset).unwrap_or(0);
        let xp_balance = record.asset_amount(self.world.xp_asset).unwrap_or(0);
        let stone = record.asset_amount(self.world.stone_asset).unwrap_or(0);
        let iron_ore = record.asset_amount(self.world.iron_ore_asset).unwrap_or(0);
        Ok(Some(PlayerSnapshot {
            state,
            record,
            previous_tx,
            logs,
            xp_balance,
            stone,
            iron_ore,
        }))
    }

    /// Resolve one declared tree to its current lineage head.
    pub async fn tree(&self, tree_id: u32) -> Result<TreeSnapshot> {
        let declared = self
            .world
            .trees
            .iter()
            .find(|tree| tree.state.tree_id == tree_id)
            .copied()
            .ok_or_else(|| anyhow!("tree {tree_id} is not part of this world"))?;
        let mut records = self
            .world
            .load_tree_lineage_records(&self.rest, std::slice::from_ref(&declared))
            .await?;
        let record = records
            .pop()
            .ok_or_else(|| anyhow!("no live tree {tree_id} in this world"))?;
        self.decode_tree(declared, record).await
    }

    /// Resolve a set of declared trees (or the whole world) to current heads.
    /// Records are paired back to their declared tree by decoded identity, so
    /// indexer response order never matters.
    pub async fn trees(&self, declared: &[ValidatedTree]) -> Result<Vec<TreeSnapshot>> {
        let declared = if declared.is_empty() {
            &self.world.trees
        } else {
            declared
        };
        let records = self
            .world
            .load_tree_lineage_records(&self.rest, declared)
            .await?;
        let txids = records
            .iter()
            .map(|record| record.outpoint.txid)
            .collect::<Vec<_>>();
        let mut transactions = self.rest.get_virtual_txs(&txids).await?;
        let mut snapshots = Vec::with_capacity(records.len());
        for record in records {
            let previous_tx = transactions
                .remove(&record.outpoint.txid)
                .ok_or_else(|| anyhow!("indexer omitted the tree's creating transaction"))?;
            snapshots.push(self.decode_tree_with(record, previous_tx, None)?);
        }
        Ok(snapshots)
    }

    async fn decode_tree(
        &self,
        declared: ValidatedTree,
        record: VtxoRecord,
    ) -> Result<TreeSnapshot> {
        let previous_tx = self
            .rest
            .get_virtual_txs(&[record.outpoint.txid])
            .await?
            .remove(&record.outpoint.txid)
            .ok_or_else(|| anyhow!("indexer omitted the tree's creating transaction"))?;
        self.decode_tree_with(record, previous_tx, Some(declared.state))
    }

    fn decode_tree_with(
        &self,
        record: VtxoRecord,
        previous_tx: Transaction,
        expected_state: Option<TreeState>,
    ) -> Result<TreeSnapshot> {
        record.validate_creating_transaction(&previous_tx)?;
        let state = tree::tree_state_from_tx(&previous_tx)?
            .ok_or_else(|| anyhow!("tree transaction has no state packet"))?;
        let declared = self
            .world
            .trees
            .iter()
            .find(|tree| tree.state == state)
            .ok_or_else(|| anyhow!("tree lineage changed identity"))?;
        if expected_state.is_some_and(|expected| expected != state) {
            return Err(anyhow!("tree lineage changed identity"));
        }
        let health = tree::tree_health_from_tx(&previous_tx)?
            .ok_or_else(|| anyhow!("tree transaction has no health packet"))?;
        let is_deployment = record.outpoint.txid == declared.deployment_txid;
        let transition = tree::classify_transition(&previous_tx, is_deployment)?;
        if record.outpoint.vout != transition.output_index()
            || (is_deployment && health.value() != tree::LOGS_PER_TREE)
        {
            return Err(anyhow!("tree record has an invalid lineage"));
        }
        let logs = record.asset_amount(self.world.log_asset).unwrap_or(0);
        let xp_reserve = record.asset_amount(self.world.xp_asset).unwrap_or(0);
        let stone_reserve = record.asset_amount(self.world.stone_asset).unwrap_or(0);
        let iron_ore_reserve = record.asset_amount(self.world.iron_ore_asset).unwrap_or(0);
        if record.amount_sats != self.manifest.dust_sats
            || record.script != self.world.contract.vtxo.script_pubkey()
            || record.asset_amount(self.world.tree_asset) != Some(1)
            || logs > self.manifest.log_reserve_per_tree
            || xp_reserve > self.manifest.xp_per_tree
            || stone_reserve > self.manifest.stone_reserve_per_tree
            || iron_ore_reserve > self.manifest.iron_ore_reserve_per_tree
            || logs != xp_reserve
            || health.value() > logs
            || !record.assets.iter().all(|asset| {
                asset.asset_id == self.world.tree_asset
                    || asset.asset_id == self.world.log_asset
                    || asset.asset_id == self.world.xp_asset
                    || asset.asset_id == self.world.stone_asset
                    || asset.asset_id == self.world.iron_ore_asset
            })
        {
            return Err(anyhow!("tree record has invalid local reserves"));
        }
        Ok(TreeSnapshot {
            state,
            health,
            record,
            logs,
            xp_reserve,
            stone_reserve,
            iron_ore_reserve,
            previous_tx,
        })
    }

    /// Prepare permissionless activation without submitting it. Persist the
    /// returned journal with the player key and PLAYER_ID before calling
    /// [`Self::resume_activation`]. On restart, resume that same journal.
    pub async fn prepare_activation(&mut self) -> Result<PreparedActivation> {
        if self.sync_player().await?.is_some() {
            return Err(anyhow!("player is already activated"));
        }
        let wallet = txbuild::player_vtxo(&self.keys, &self.params)?;
        let wallet_script = wallet.script_pubkey();
        let records = self.wallet_records().await?;
        let now = crate::arkade::now_unix();
        let mut funding = records.iter().filter(|record| {
            record.assets.is_empty()
                && record.amount_sats == self.params.dust_sats
                && record.script == wallet_script
                && record
                    .ensure_live(now, crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS)
                    .is_ok()
        });
        let record = funding.next().ok_or_else(|| {
            anyhow!(
            "deposit one clean, live, exact {}-sat wallet VTXO first; balances are not combined",
            self.params.dust_sats
        )
        })?;
        if funding.next().is_some() {
            return Err(anyhow!(
                "multiple activation VTXOs are available; keep exactly one"
            ));
        }
        let prepared =
            crate::chop::prepare_activation(&self.keys, &self.params, &self.contract, record)?;
        if self
            .player_asset
            .is_some_and(|asset| asset != prepared.player_asset)
        {
            return Err(anyhow!(
                "resume the saved activation journal for the selected PLAYER_ID"
            ));
        }
        self.player_asset = Some(prepared.player_asset);
        Ok(prepared)
    }

    /// Resume a previously persisted activation. Keep the original journal on
    /// errors, Pending, and SubmissionUnknown; only Finalized confirms the exact
    /// activation output, even if that output has since been spent.
    pub async fn resume_activation(
        &mut self,
        prepared: &PreparedActivation,
    ) -> Result<txbuild::RunTxStatus> {
        let txid = txbuild::validate_activation_journal(
            &self.keys,
            &self.params,
            prepared,
            &self.contract,
        )?;
        if self
            .player_asset
            .is_some_and(|asset| asset != prepared.player_asset)
        {
            return Err(anyhow!(
                "activation journal does not match the selected PLAYER_ID"
            ));
        }
        self.player_asset = Some(prepared.player_asset);
        let script = self.contract.vtxo.script_pubkey();
        let outpoint = OutPoint {
            txid,
            vout: u32::from(protocol::ACTIVATION_STATE_OUTPUT_INDEX),
        };
        if self
            .rest
            .find_settled_vtxo(&script, outpoint)
            .await?
            .is_some()
        {
            return Ok(txbuild::RunTxStatus::Finalized(txid));
        }
        match txbuild::resume_tx(&self.keys, &self.rest, &prepared.transaction).await? {
            txbuild::RunTxStatus::Finalized(finalized) => {
                if finalized != txid {
                    return Err(anyhow!("activation finalized an unexpected transaction"));
                }
                self.rest.wait_for_settled_vtxo(&script, outpoint).await?;
                Ok(txbuild::RunTxStatus::Finalized(txid))
            }
            status @ (txbuild::RunTxStatus::Pending(_)
            | txbuild::RunTxStatus::SubmissionUnknown(_)) => Ok(status),
        }
    }

    /// Swing at one tree: build, sign, emulator-execute, and verify the exact
    /// accepted transition. On a lost submission response the settled lineage
    /// is re-checked before the error is returned, so callers can retry
    /// safely by syncing and inspecting state rather than blind-resubmitting.
    pub async fn chop(&mut self, tree_id: u32) -> Result<ChopOutcome> {
        let player = self
            .sync_player()
            .await?
            .ok_or_else(|| anyhow!("activate the player before chopping"))?;
        let tree = self.tree(tree_id).await?;
        let player_asset = self
            .player_asset
            .ok_or_else(|| anyhow!("activate the player before chopping"))?;
        let prepared = crate::chop::prepare_chop(
            &self.keys,
            &self.info(),
            &ChopWorld {
                contract: &self.world.contract,
                tree_asset: self.world.tree_asset,
                log_asset: self.world.log_asset,
                xp_asset: self.world.xp_asset,
                stone_asset: self.world.stone_asset,
                iron_ore_asset: self.world.iron_ore_asset,
                dust_sats: self.params.dust_sats,
            },
            &PlayerChopState {
                record: &player.record,
                previous_tx: &player.previous_tx,
                contract: &self.contract,
                state: player.state,
                player_asset,
            },
            &TreeChopState {
                record: &tree.record,
                previous_tx: &tree.previous_tx,
                health: tree.health,
            },
            ChopMutation::None,
        )?;
        let txid = prepared.ark_tx.unsigned_tx.compute_txid();
        let expected_player = OutPoint {
            txid,
            vout: u32::from(protocol::PLAYER_STATE_OUTPUT_INDEX),
        };
        let expected_tree = OutPoint {
            txid,
            vout: u32::from(protocol::TREE_OUTPUT_INDEX),
        };
        let submission = self
            .emulator
            .submit_tx(&prepared.ark_tx, &prepared.checkpoint_txs)
            .await
            .context("submit chop to emulator");
        let (returned_ark, returned_checkpoints) = match submission {
            Ok(response) => response,
            Err(error) => {
                // The original may still land; never blind-resubmit. Give the
                // indexer a bounded window to show the journaled outpoints.
                for _ in 0..12 {
                    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
                    let landed = self
                        .rest
                        .find_settled_vtxo(&self.contract.vtxo.script_pubkey(), expected_player)
                        .await?
                        .is_some()
                        && self
                            .rest
                            .find_settled_vtxo(
                                &self.world.contract.vtxo.script_pubkey(),
                                expected_tree,
                            )
                            .await?
                            .is_some();
                    if landed {
                        return Ok(ChopOutcome {
                            tree_id,
                            success: prepared.success,
                            material: prepared.material,
                            txid,
                            player_outpoint: expected_player,
                            tree_outpoint: expected_tree,
                        });
                    }
                }
                return Err(error.context(
                    "chop submission failed and did not reconcile; sync and inspect before retrying",
                ));
            }
        };
        player::verify_player_chop_response(
            &self.keys,
            &self.contract,
            &self.world.contract,
            &prepared.ark_tx,
            &prepared.checkpoint_txs,
            &returned_ark,
            returned_checkpoints,
        )?;
        self.rest
            .wait_for_settled_vtxo(&self.contract.vtxo.script_pubkey(), expected_player)
            .await?;
        self.rest
            .wait_for_settled_vtxo(&self.world.contract.vtxo.script_pubkey(), expected_tree)
            .await?;
        Ok(ChopOutcome {
            tree_id,
            success: prepared.success,
            material: prepared.material,
            txid,
            player_outpoint: expected_player,
            tree_outpoint: expected_tree,
        })
    }

    /// Burn the exact next-tier recipe under the recursive player covenant.
    /// XP, PLAYER_ID, roll, luck, sats, and unspent inventory remain in state.
    /// Bind the displayed recipe to its input so a retry cannot buy another tier.
    pub async fn craft_axe(&mut self, expected_outpoint: OutPoint) -> Result<CraftOutcome> {
        let player = self
            .sync_player()
            .await?
            .ok_or_else(|| anyhow!("activate the player before crafting an axe"))?;
        if player.outpoint() != expected_outpoint {
            return Err(anyhow!("player state changed; refresh before crafting"));
        }
        let player_asset = self
            .player_asset
            .ok_or_else(|| anyhow!("activate the player before crafting an axe"))?;
        let prepared = crate::chop::prepare_craft(
            &self.keys,
            &self.info(),
            &self.contract,
            player_asset,
            &player.record,
            &player.previous_tx,
            player.state,
        )?;
        let txid = prepared.ark_tx.unsigned_tx.compute_txid();
        let player_outpoint = OutPoint {
            txid,
            vout: u32::from(protocol::CRAFT_STATE_OUTPUT_INDEX),
        };
        let (returned_ark, _) = self
            .emulator
            .submit_tx(&prepared.ark_tx, &prepared.checkpoint_txs)
            .await
            .context("submit axe crafting transaction to emulator")?;
        if returned_ark.unsigned_tx != prepared.ark_tx.unsigned_tx {
            return Err(anyhow!(
                "emulator changed the submitted axe crafting transaction"
            ));
        }
        self.wait_for_vtxo(&self.contract.vtxo.script_pubkey(), player_outpoint)
            .await?;
        let settled = self
            .sync_player()
            .await?
            .ok_or_else(|| anyhow!("crafted player state was not discovered"))?;
        let recipe = prepared.recipe;
        if settled.record.outpoint != player_outpoint
            || settled.state.axe != recipe.axe
            || settled.state.luck != player.state.luck
            || settled.logs != player.logs - recipe.log_cost
            || settled.xp_balance != player.xp_balance
            || settled.stone != player.stone - recipe.stone_cost
            || settled.iron_ore != player.iron_ore - recipe.iron_ore_cost
        {
            return Err(anyhow!("settled axe crafting state is invalid"));
        }
        Ok(CraftOutcome {
            axe: recipe.axe,
            txid,
            player_outpoint,
        })
    }

    /// Withdraw LOG to an Arkade address (defaults to the player's own plain
    /// wallet). XP is soulbound and can never move; the covenant rejects any
    /// withdrawal that touches it.
    pub async fn withdraw_log(
        &mut self,
        amount: u64,
        destination: Option<bitcoin::Address>,
    ) -> Result<ChopOutcome> {
        let player = self
            .sync_player()
            .await?
            .ok_or_else(|| anyhow!("activate the player before withdrawing"))?;
        let player_asset = self
            .player_asset
            .ok_or_else(|| anyhow!("activate the player before withdrawing"))?;
        let wallet = txbuild::player_vtxo(&self.keys, &self.params)?;
        let funding = self
            .wallet_records()
            .await?
            .into_iter()
            .filter(|record| {
                record.assets.is_empty() && record.amount_sats == self.params.dust_sats
            })
            .collect::<Vec<_>>();
        let [funding] = funding.as_slice() else {
            return Err(anyhow!(
                "keep exactly one {}-sat wallet VTXO for withdrawal funding",
                self.params.dust_sats
            ));
        };
        let destination = match destination {
            Some(address) => address,
            None => bitcoin::Address::from_script(&wallet.script_pubkey(), self.params.network)
                .map_err(|error| anyhow!("derive wallet address: {error}"))?,
        };
        let funding_previous_tx = self
            .rest
            .get_virtual_txs(&[funding.outpoint.txid])
            .await?
            .remove(&funding.outpoint.txid)
            .ok_or_else(|| anyhow!("indexer omitted the funding transaction"))?;
        let prepared = crate::chop::prepare_withdraw(
            &self.keys,
            &self.info(),
            &self.contract,
            player_asset,
            &player.record,
            &player.previous_tx,
            player.state,
            funding,
            &funding_previous_tx,
            &wallet,
            amount,
            destination,
        )?;
        let txid = prepared.ark_tx.unsigned_tx.compute_txid();
        let state_outpoint = OutPoint {
            txid,
            vout: u32::from(protocol::WITHDRAW_STATE_OUTPUT_INDEX),
        };
        let (returned_ark, _returned_checkpoints) = self
            .emulator
            .submit_tx(&prepared.ark_tx, &prepared.checkpoint_txs)
            .await
            .context("submit withdraw to emulator")?;
        if returned_ark.unsigned_tx != prepared.ark_tx.unsigned_tx {
            return Err(anyhow!("emulator changed the submitted withdraw"));
        }
        self.wait_for_vtxo(&self.contract.vtxo.script_pubkey(), state_outpoint)
            .await?;
        self.wait_for_vtxo(
            &prepared.destination.script_pubkey,
            OutPoint {
                txid,
                vout: u32::from(protocol::WITHDRAW_DESTINATION_OUTPUT_INDEX),
            },
        )
        .await?;
        Ok(ChopOutcome {
            tree_id: 0,
            success: true,
            material: player::MaterialDrop::None,
            txid,
            player_outpoint: state_outpoint,
            tree_outpoint: state_outpoint,
        })
    }

    /// Permissionlessly regrow a funded stump through one exact-self-send
    /// batch. The fresh batch is the complete lifecycle boundary.
    pub async fn regrow(&mut self, tree_id: u32) -> Result<OutPoint> {
        let tree = self.tree(tree_id).await?;
        if tree.health.value() != 0 {
            return Err(anyhow!("tree {tree_id} is not a stump"));
        }
        if tree.record.asset_amount(self.world.log_asset).unwrap_or(0) == 0 {
            return Err(anyhow!("tree {tree_id} has exhausted its local reserve"));
        }
        let prepared = renewal::prepare_tree(
            &tree.record,
            &tree.previous_tx,
            &self.world.contract,
            self.world.tree_asset,
            self.world.log_asset,
            self.world.xp_asset,
            self.world.stone_asset,
            self.world.iron_ore_asset,
            self.params.dust_sats,
            0,
        )?;
        let services = BatchServices::connect(
            &self.arkade_url,
            self.emulator.clone(),
            self.params.clone(),
            self.world.pins.clone(),
        )
        .await?;
        let fee_funding = if services.renewal_requires_fee(&prepared)? {
            crate::batch::find_renewal_fee_funding(
                &self.rest,
                &self.keys,
                &self.params,
                tree.record.outpoint,
            )
            .await?
        } else {
            None
        };
        let outcome = services
            .settle_renewal(
                &self.keys,
                self.emulator_params.signer_pk,
                prepared,
                &tree.previous_tx,
                fee_funding.as_ref().map(|funding| funding.source()),
            )
            .await?;
        let renewed = self
            .wait_for_vtxo(&self.world.contract.vtxo.script_pubkey(), outcome.outpoint)
            .await?;
        let transaction = self
            .rest
            .get_virtual_txs(&[renewed.outpoint.txid])
            .await?
            .remove(&renewed.outpoint.txid)
            .ok_or_else(|| anyhow!("indexer omitted the regrown tree transaction"))?;
        let health = tree::tree_health_from_tx(&transaction)?
            .ok_or_else(|| anyhow!("regrown tree has no health packet"))?;
        if health.value() != tree::LOGS_PER_TREE {
            return Err(anyhow!("regrown tree state is invalid"));
        }
        Ok(renewed.outpoint)
    }

    /// Owner-authorized exact-self-send renewal through the batch flow.
    pub async fn renew_player(&mut self) -> Result<OutPoint> {
        let player = self
            .sync_player()
            .await?
            .ok_or_else(|| anyhow!("activate the player before renewal"))?;
        let player_asset = self
            .player_asset
            .ok_or_else(|| anyhow!("activate the player before renewal"))?;
        let old_expires_at = player.record.expires_at;
        let prepared = renewal::prepare_player(
            &player.record,
            &player.previous_tx,
            &self.contract,
            player_asset,
            crate::arkade::DEFAULT_EXPIRY_MARGIN_SECS,
        )?;
        let services = BatchServices::connect(
            &self.arkade_url,
            self.emulator.clone(),
            self.params.clone(),
            self.world.pins.clone(),
        )
        .await?;
        let fee_funding = if services.renewal_requires_fee(&prepared)? {
            crate::batch::find_renewal_fee_funding(
                &self.rest,
                &self.keys,
                &self.params,
                player.record.outpoint,
            )
            .await?
        } else {
            None
        };
        let outcome = services
            .settle_renewal(
                &self.keys,
                self.emulator_params.signer_pk,
                prepared,
                &player.previous_tx,
                fee_funding.as_ref().map(|funding| funding.source()),
            )
            .await?;
        let renewed = self
            .wait_for_vtxo(&self.contract.vtxo.script_pubkey(), outcome.outpoint)
            .await?;
        if renewed.expires_at <= old_expires_at {
            return Err(anyhow!("player renewal did not extend the indexed expiry"));
        }
        Ok(renewed.outpoint)
    }

    fn info(&self) -> ark_core::server::Info {
        txbuild::server_info(&self.params)
    }

    async fn wait_for_vtxo(
        &self,
        script: &bitcoin::ScriptBuf,
        outpoint: OutPoint,
    ) -> Result<VtxoRecord> {
        for _ in 0..INDEX_ATTEMPTS {
            let records = self.rest.get_vtxos_by_outpoints(&[outpoint]).await?;
            if let Some(record) = records.into_iter().find(|record| {
                record.outpoint == outpoint
                    && record.script == *script
                    && !record.is_spent
                    && !record.is_swept
                    && !record.is_unrolled
            }) {
                return Ok(record);
            }
            tokio::time::sleep(std::time::Duration::from_millis(INDEX_POLL_MS)).await;
        }
        Err(anyhow!("indexer did not expose VTXO {outpoint}"))
    }
}
