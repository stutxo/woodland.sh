//! Export real transaction-builder cases for the pinned stock Arkade VM.

use crate::arkade::{ServerParams, VtxoRecord};
use crate::chop::{
    prepare_chop, prepare_craft, prepare_withdraw, ChopMutation, ChopWorld, PlayerChopState,
    TreeChopState,
};
use crate::player::{self, AxeTier, PlayerContract, PlayerLuck, PlayerState};
use crate::tree::{self, TreeContract, TreeHealth, TreeState};
use crate::Keys;
use ark_core::asset::packet::{AssetGroup, AssetInput, AssetOutput, Packet};
use ark_core::asset::AssetId;
use bitcoin::hashes::Hash;
use bitcoin::hex::{DisplayHex, FromHex};
use bitcoin::{Amount, Network, OutPoint, Psbt, ScriptBuf, Sequence, Transaction, TxOut, Txid};
use serde_json::{json, Value};

fn asset(group_index: u16) -> AssetId {
    AssetId {
        txid: Txid::from_byte_array([9; 32]),
        group_index,
    }
}

fn previous(script: ScriptBuf, assets: &[(AssetId, u64)]) -> Psbt {
    let mut psbt = Psbt::from_unsigned_tx(Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn::default()],
        output: vec![
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: script,
            },
            ark_core::anchor_output(),
        ],
    })
    .unwrap();
    let groups = assets
        .iter()
        .filter(|(_, amount)| *amount > 0)
        .map(|(asset_id, amount)| AssetGroup {
            asset_id: Some(*asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: *amount,
            }],
            outputs: vec![AssetOutput {
                output_index: 0,
                amount: *amount,
            }],
        })
        .collect();
    if !assets.is_empty() {
        ark_core::asset::packet::add_asset_packet_to_psbt(&mut psbt, &Packet { groups }).unwrap();
    }
    psbt
}

fn record(tx: &Transaction) -> VtxoRecord {
    VtxoRecord {
        outpoint: OutPoint {
            txid: tx.compute_txid(),
            vout: 0,
        },
        script: tx.output[0].script_pubkey.clone(),
        amount_sats: 330,
        assets: crate::asset_packet::output_assets(tx, 0).unwrap(),
        created_at: Some(1),
        expires_at: Some(i64::MAX),
        is_preconfirmed: false,
        is_spent: false,
        is_swept: false,
        is_unrolled: false,
        spent_by: None,
        settled_by: None,
    }
}

struct Fixture {
    keys: Keys,
    params: ServerParams,
    tree: TreeContract,
    player: PlayerContract,
    marker: AssetId,
}

impl Fixture {
    fn new() -> Self {
        let keys = Keys::from_hex(&"03".repeat(32)).unwrap();
        let operator = Keys::from_hex(&"04".repeat(32)).unwrap();
        let emulator = Keys::from_hex(&"05".repeat(32)).unwrap();
        let rollover = Keys::from_hex(&"06".repeat(32)).unwrap();
        let params = ServerParams {
            version: "fixture".into(),
            signer_pk: operator.owner_pk(),
            forfeit_pk: bitcoin::PublicKey::new(operator.keypair.public_key()),
            network: Network::Regtest,
            dust_sats: 330,
            vtxo_min_sats: 330,
            unilateral_exit_delay: Sequence::from_height(144),
            max_tx_weight: 40_000,
            max_op_return_outputs: 2,
            zero_offchain_fees: true,
            checkpoint_tapscript: ark_core::script::csv_sig_script(
                Sequence::from_height(144),
                operator.owner_pk(),
            ),
            forfeit_address: bitcoin::Address::p2tr(
                &keys.secp,
                operator.owner_pk(),
                None,
                Network::Regtest,
            ),
        };
        let tree = tree::build_tree_contract(
            &keys.secp,
            operator.owner_pk(),
            emulator.owner_pk(),
            rollover.owner_pk(),
            params.unilateral_exit_delay,
            params.network,
            asset(0),
            asset(1),
            asset(2),
            asset(3),
            asset(4),
            330,
        )
        .unwrap();
        let player = player::build_player_contract(
            &keys.secp,
            keys.owner_pk(),
            operator.owner_pk(),
            emulator.owner_pk(),
            rollover.owner_pk(),
            params.unilateral_exit_delay,
            params.network,
            asset(0),
            asset(1),
            asset(2),
            asset(3),
            asset(4),
            330,
            &tree.vtxo.script_pubkey(),
        )
        .unwrap();
        Self {
            keys,
            params,
            tree,
            player,
            marker: AssetId {
                txid: Txid::from_byte_array([10; 32]),
                group_index: 0,
            },
        }
    }

    fn chop_vectors(
        &self,
        name: &str,
        xp: u64,
        axe: AxeTier,
        success: bool,
        valid: bool,
    ) -> Vec<Value> {
        self.chop_mutation_vectors(name, xp, axe, success, ChopMutation::None, [valid; 2])
    }

    fn chop_mutation_vectors(
        &self,
        name: &str,
        xp: u64,
        axe: AxeTier,
        success: bool,
        mutation: ChopMutation,
        valid: [bool; 2],
    ) -> Vec<Value> {
        let tree_state = TreeState {
            tree_id: 417,
            x: 7,
            y: 13,
        };
        let mut tree_previous = previous(
            self.tree.vtxo.script_pubkey(),
            &[
                (asset(0), 1),
                (asset(1), 50_000),
                (asset(2), 50_000),
                (asset(3), 50_000),
                (asset(4), 50_000),
            ],
        );
        tree::attach_tree_state_packet(&mut tree_previous, tree_state).unwrap();
        tree::attach_tree_health_packet(&mut tree_previous, TreeHealth::new(10).unwrap()).unwrap();
        let mut luck = PlayerLuck::initial(&self.player.vtxo.script_pubkey()).unwrap();
        while luck.advance(xp, axe).1 != success {
            luck.roll = luck.roll.next();
        }
        let state = PlayerState { luck, axe };
        let mut player_previous = previous(
            self.player.vtxo.script_pubkey(),
            &[
                (self.marker, 1),
                (asset(1), xp),
                (asset(2), xp),
                (asset(3), if xp > 0 { 2 } else { 0 }),
                (asset(4), if xp >= 97 { 2 } else { 0 }),
            ],
        );
        player::attach_player_state_packets(&mut player_previous, state).unwrap();
        let player_previous = player_previous.unsigned_tx;
        let tree_previous = tree_previous.unsigned_tx;
        let player_record = record(&player_previous);
        let tree_record = record(&tree_previous);
        let prepared = prepare_chop(
            &self.keys,
            &crate::txbuild::server_info(&self.params),
            &ChopWorld {
                contract: &self.tree,
                tree_asset: asset(0),
                log_asset: asset(1),
                xp_asset: asset(2),
                stone_asset: asset(3),
                iron_ore_asset: asset(4),
                dust_sats: 330,
            },
            &PlayerChopState {
                contract: &self.player,
                player_asset: self.marker,
                record: &player_record,
                previous_tx: &player_previous,
                state,
            },
            &TreeChopState {
                record: &tree_record,
                previous_tx: &tree_previous,
                health: TreeHealth::new(10).unwrap(),
            },
            mutation,
        )
        .unwrap();
        // Production's advertised 40,000-weight limit must still accommodate
        // both programs and the full-template witness after this change.
        // Reserve more than twice the current owner/operator/emulator
        // signatures, spend scripts, and control blocks' serialized weight.
        assert!(prepared.ark_tx.unsigned_tx.weight().to_wu() + 1_500 < 40_000);
        vectors_for(
            name,
            &prepared.ark_tx.unsigned_tx,
            &direct_prevouts(
                &prepared.ark_tx,
                &prepared.checkpoint_txs,
                &[&player_previous, &tree_previous],
            ),
            &valid,
        )
    }

    fn player_previous(&self, xp: u64, axe: AxeTier) -> (Transaction, PlayerState) {
        let state = PlayerState {
            luck: PlayerLuck::initial(&self.player.vtxo.script_pubkey()).unwrap(),
            axe,
        };
        let mut transaction = previous(
            self.player.vtxo.script_pubkey(),
            &[
                (self.marker, 1),
                (asset(1), 10),
                (asset(2), xp),
                (asset(3), 4),
                (asset(4), 4),
            ],
        );
        player::attach_player_state_packets(&mut transaction, state).unwrap();
        (transaction.unsigned_tx, state)
    }

    fn renewal_vectors(&self, name: &str, xp: u64, axe: AxeTier, valid: bool) -> Vec<Value> {
        let (previous, _) = self.player_previous(xp, axe);
        let prepared = crate::renewal::prepare_player(
            &record(&previous),
            &previous,
            &self.player,
            self.marker,
            0,
        )
        .unwrap();
        let prepared = crate::renewal::bind(
            &self.keys,
            prepared,
            &previous,
            self.keys.keypair.public_key(),
        )
        .unwrap();
        let proof = &prepared.intent.proof;
        let prevouts = proof
            .unsigned_tx
            .input
            .iter()
            .zip(&proof.inputs)
            .map(|(input, metadata)| {
                let txout = metadata.witness_utxo.as_ref().unwrap();
                // The fake message input has no gameplay packets; only input one is
                // introspected by this renewal program.
                prevout(
                    input.previous_output,
                    txout,
                    &previous,
                    &txout.script_pubkey,
                )
            })
            .collect::<Vec<_>>();
        vectors_for(name, &proof.unsigned_tx, &prevouts, &[true, valid])
    }

    fn withdraw_vectors(&self, name: &str, xp: u64, axe: AxeTier, valid: bool) -> Vec<Value> {
        let (previous, state) = self.player_previous(xp, axe);
        let wallet = crate::txbuild::player_vtxo(&self.keys, &self.params).unwrap();
        let funding = previous_wallet(wallet.script_pubkey());
        let prepared = prepare_withdraw(
            &self.keys,
            &crate::txbuild::server_info(&self.params),
            &self.player,
            self.marker,
            &record(&previous),
            &previous,
            state,
            &record(&funding),
            &funding,
            &wallet,
            3,
            bitcoin::Address::p2tr(
                &self.keys.secp,
                self.keys.owner_pk(),
                None,
                Network::Regtest,
            ),
        )
        .unwrap();
        vectors_for(
            name,
            &prepared.ark_tx.unsigned_tx,
            &direct_prevouts(
                &prepared.ark_tx,
                &prepared.checkpoint_txs,
                &[&previous, &funding],
            ),
            &[valid, true],
        )
    }

    fn craft_vectors(&self, name: &str, xp: u64, axe: AxeTier) -> Vec<Value> {
        let (previous, state) = self.player_previous(xp, axe);
        let prepared = prepare_craft(
            &self.keys,
            &crate::txbuild::server_info(&self.params),
            &self.player,
            self.marker,
            &record(&previous),
            &previous,
            state,
        )
        .unwrap();
        let prevouts = direct_prevouts(&prepared.ark_tx, &prepared.checkpoint_txs, &[&previous]);
        let mut vectors = vectors_for(name, &prepared.ark_tx.unsigned_tx, &prevouts, &[true]);
        let mut changed = prepared.ark_tx.unsigned_tx.clone();
        replace_packet(
            &mut changed,
            crate::protocol::PLAYER_AXE_PACKET_TYPE,
            &axe.encode(),
        );
        vectors.extend(vectors_for(
            &format!("{name}/reject-unchanged-tier"),
            &changed,
            &prevouts,
            &[false],
        ));
        let mut changed = prepared.ark_tx.unsigned_tx.clone();
        let groups = [
            (self.marker, 1, 1),
            (asset(1), 10, 10),
            (asset(2), xp, xp),
            (asset(3), 4, 4 - prepared.recipe.stone_cost),
            (asset(4), 4, 4 - prepared.recipe.iron_ore_cost),
        ]
        .into_iter()
        .map(|(asset, before, after)| {
            crate::chop::transfer_group(asset, vec![(0, before)], vec![(0, after)])
        })
        .collect();
        replace_packet(&mut changed, 0, &Packet { groups }.encode());
        vectors.extend(vectors_for(
            &format!("{name}/reject-unburned-log"),
            &changed,
            &prevouts,
            &[false],
        ));
        vectors
    }
}

fn previous_wallet(script: ScriptBuf) -> Transaction {
    previous(script, &[]).unsigned_tx
}

fn prevout(outpoint: OutPoint, txout: &TxOut, previous: &Transaction, script: &ScriptBuf) -> Value {
    json!({
        "outpoint": {"txid": outpoint.txid.to_string(), "vout": outpoint.vout},
        "txout": {"value": txout.value.to_sat(), "script": txout.script_pubkey.to_hex_string()},
        "ark_tx": bitcoin::consensus::serialize(previous).to_lower_hex_string(),
        "vtxo_script": script.to_hex_string(),
    })
}

fn direct_prevouts(psbt: &Psbt, checkpoints: &[Psbt], previous: &[&Transaction]) -> Vec<Value> {
    previous
        .iter()
        .enumerate()
        .map(|(index, previous)| {
            prevout(
                psbt.unsigned_tx.input[index].previous_output,
                &checkpoints[index].unsigned_tx.output[0],
                previous,
                &previous.output[0].script_pubkey,
            )
        })
        .collect()
}

fn vectors_for(
    name: &str,
    transaction: &Transaction,
    prevouts: &[Value],
    valid: &[bool],
) -> Vec<Value> {
    let packet = ark_core::introspector::packet::find_packet(transaction)
        .unwrap()
        .unwrap();
    packet.entries.iter().map(|entry| json!({
        "name": format!("{name}/input{}", entry.vin),
        "script": entry.script.to_hex_string(),
        "witness": entry.witness.iter().map(|bytes| bytes.to_lower_hex_string()).collect::<Vec<_>>(),
        "transaction": bitcoin::consensus::serialize(transaction).to_lower_hex_string(),
        "input_index": entry.vin, "prevouts": prevouts, "valid": valid[usize::from(entry.vin)],
    })).collect()
}

fn replace_packet(transaction: &mut Transaction, packet_type: u8, replacement: &[u8]) {
    let output = transaction
        .output
        .iter_mut()
        .find(|output| ark_core::extension::is_extension(&output.script_pubkey))
        .unwrap();
    let payload = ark_core::extension::extension_payload(&output.script_pubkey).unwrap();
    let packets = ark_core::extension::iter_packets(payload).unwrap();
    let mut encoded = ark_core::extension::MAGIC_BYTES.to_vec();
    for (kind, payload) in packets {
        let payload = if kind == packet_type {
            replacement
        } else {
            payload
        };
        encoded.push(kind);
        ark_core::extension::encode_uvarint(&mut encoded, payload.len() as u64);
        encoded.extend_from_slice(payload);
    }
    output.script_pubkey = bitcoin::script::Builder::new()
        .push_opcode(bitcoin::opcodes::all::OP_RETURN)
        .push_slice(bitcoin::script::PushBytesBuf::try_from(encoded).unwrap())
        .into_script();
}

fn malformed_chop_shapes(baseline: &[Value]) -> Vec<Value> {
    let original: Transaction = bitcoin::consensus::deserialize(
        &Vec::<u8>::from_hex(baseline[0]["transaction"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let prevouts = baseline[0]["prevouts"].as_array().unwrap();
    let mut vectors = Vec::new();
    for variation in 0..4 {
        let mut transaction = original.clone();
        let (name, expected) = match variation {
            0 => {
                assert_eq!(transaction.output.pop(), Some(ark_core::anchor_output()));
                ("reject-missing-anchor-output", [false, false])
            }
            1 => {
                let player_script = transaction.output[0].script_pubkey.clone();
                transaction.output[0].script_pubkey = transaction.output[1].script_pubkey.clone();
                transaction.output[1].script_pubkey = player_script;
                ("reject-swapped-output-contracts", [false, false])
            }
            2 => {
                transaction.input.swap(0, 1);
                ("reject-swapped-inputs", [false, false])
            }
            _ => {
                let mut packet = ark_core::introspector::packet::find_packet(&transaction)
                    .unwrap()
                    .unwrap();
                let tree_entry = packet
                    .entries
                    .iter_mut()
                    .find(|entry| entry.vin == 1)
                    .unwrap();
                let mut witness = tree_entry
                    .witness
                    .iter()
                    .map(<[u8]>::to_vec)
                    .collect::<Vec<_>>();
                witness[0] = Keys::from_hex(&"07".repeat(32))
                    .unwrap()
                    .owner_pk()
                    .serialize()
                    .to_vec();
                tree_entry.witness = bitcoin::Witness::from_slice(&witness);
                replace_packet(&mut transaction, 1, &packet.encode().unwrap());
                ("reject-incorrect-template-owner", [true, false])
            }
        };
        vectors.extend(vectors_for(name, &transaction, prevouts, &expected));
    }
    vectors
}

#[test]
fn chop_covenant_vm_vectors() {
    let fixture = Fixture::new();
    let mut vectors = Vec::new();
    for (name, xp, axe) in [
        ("new-player", 0, AxeTier::None),
        ("wooden", 1, AxeTier::Wooden),
        ("stone", 16, AxeTier::Stone),
        ("iron", 97, AxeTier::Iron),
        ("level-50-iron", 4054, AxeTier::Iron),
    ] {
        for success in [false, true] {
            vectors.extend(fixture.chop_vectors(
                &format!("{name}/{success}"),
                xp,
                axe,
                success,
                true,
            ));
        }
    }
    for axe in [AxeTier::Wooden, AxeTier::Stone, AxeTier::Iron] {
        vectors.extend(fixture.chop_vectors(
            &format!("reject-unearned-{axe:?}"),
            0,
            axe,
            true,
            false,
        ));
    }
    vectors.extend(fixture.chop_vectors("reject-early-stone", 15, AxeTier::Stone, true, false));
    vectors.extend(fixture.chop_vectors("reject-early-iron", 96, AxeTier::Iron, true, false));
    vectors.extend(malformed_chop_shapes(&fixture.chop_vectors(
        "shape-baseline",
        97,
        AxeTier::Iron,
        true,
        true,
    )));
    // Several conservation checks intentionally live only in the tree half.
    // Assert each half's real result instead of accepting any unrelated error.
    for (mutation, expected) in [
        (ChopMutation::WrongRoll, [false, false]),
        (ChopMutation::WrongLuckCredit, [false, false]),
        (ChopMutation::WrongLogDelta, [true, false]),
        (ChopMutation::WrongXpDelta, [true, false]),
        (ChopMutation::WrongMaterialDelta, [true, false]),
        (ChopMutation::NonCanonicalHealth, [true, false]),
        (ChopMutation::DoubleTreeMarker, [false, false]),
        (ChopMutation::SwapWorldGroups, [true, false]),
        (ChopMutation::SwapLogXpGroups, [true, false]),
        (ChopMutation::ExtraOutput, [false, false]),
        (ChopMutation::WrongAnchor, [false, false]),
        (ChopMutation::AssetMetadata, [false, false]),
        (ChopMutation::PlayerMarkerMetadata, [false, false]),
        (ChopMutation::AssetControl, [false, false]),
        (ChopMutation::FundExtension, [false, false]),
    ] {
        vectors.extend(fixture.chop_mutation_vectors(
            &format!("mutation-{mutation:?}"),
            97,
            AxeTier::Iron,
            true,
            mutation,
            expected,
        ));
    }
    for (name, xp, axe, valid) in [
        ("new", 0, AxeTier::None, true),
        ("wooden", 1, AxeTier::Wooden, true),
        ("stone", 16, AxeTier::Stone, true),
        ("iron", 97, AxeTier::Iron, true),
        ("unearned-wooden", 0, AxeTier::Wooden, false),
        ("unearned-stone", 0, AxeTier::Stone, false),
        ("unearned-iron", 0, AxeTier::Iron, false),
        ("early-stone", 15, AxeTier::Stone, false),
        ("early-iron", 96, AxeTier::Iron, false),
    ] {
        vectors.extend(fixture.renewal_vectors(&format!("renewal-{name}"), xp, axe, valid));
        vectors.extend(fixture.withdraw_vectors(&format!("withdraw-{name}"), xp, axe, valid));
    }
    for (name, xp, axe) in [
        ("craft-wooden", 1, AxeTier::None),
        ("craft-stone", 16, AxeTier::Wooden),
        ("craft-iron", 97, AxeTier::Stone),
    ] {
        vectors.extend(fixture.craft_vectors(name, xp, axe));
    }
    let (unearned, state) = fixture.player_previous(0, AxeTier::None);
    assert!(prepare_craft(
        &fixture.keys,
        &crate::txbuild::server_info(&fixture.params),
        &fixture.player,
        fixture.marker,
        &record(&unearned),
        &unearned,
        state
    )
    .is_err());
    if let Some(directory) = std::env::var_os("WOODLAND_COVENANT_VECTORS") {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            std::path::Path::new(&directory).join("chop.json"),
            serde_json::to_vec_pretty(&vectors).unwrap(),
        )
        .unwrap();
    }
}
