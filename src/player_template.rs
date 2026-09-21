//! Authenticate every spending path of a personalized player VTXO.
//!
//! Authenticating only its active Arkade script would also accept a VTXO with
//! an additional unrestricted leaf. This template reconstructs all six leaves
//! and the NUMS internal key before comparing the resulting Taproot key.

use anyhow::{Context, Result};
use ark_script::{op, ArkadeTapscript};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::opcodes::all::{
    OP_2DUP, OP_BOOLAND, OP_CAT, OP_DROP, OP_DUP, OP_ENDIF, OP_EQUAL, OP_EQUALVERIFY,
    OP_FROMALTSTACK, OP_GREATERTHANOREQUAL, OP_IF, OP_LESSTHAN, OP_LESSTHANOREQUAL, OP_NOT,
    OP_ROLL, OP_ROT, OP_SHA256, OP_SIZE, OP_SWAP, OP_TOALTSTACK, OP_VERIFY,
};
use bitcoin::script::{Builder, PushBytesBuf};
use bitcoin::{ScriptBuf, Sequence, TapLeafHash, XOnlyPublicKey};

// Present in the pinned emulator v0.0.7-rc.1; the pinned Rust SDK omits its alias.
const REVERSE_BYTES: bitcoin::Opcode = bitcoin::opcodes::all::OP_RETURN_217;

/// Consume `[owner_xonly_32, compressed_output_prefix_1]` from the Arkade witness
/// and authenticate input zero against the complete canonical player template.
/// The prefix is 0x02 or 0x03, as required by OP_TWEAKVERIFY's compressed point.
/// Emulator keys are already tweaked, in chop/renewal/withdraw/craft order.
pub(crate) fn push_player_template(
    builder: Builder,
    operator: XOnlyPublicKey,
    rollover: XOnlyPublicKey,
    exit_delay: Sequence,
    tweaked_emulators: [XOnlyPublicKey; 4],
) -> Result<Builder> {
    let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY
        .parse()
        .context("parse player template NUMS key")?;
    let internal_key = nums.inner.x_only_public_key().0;
    let [chop, renewal, withdraw, craft] = tweaked_emulators;

    let mut builder = builder
        .push_opcode(OP_SIZE)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_opcode(op::BIN2NUM)
        .push_opcode(OP_DUP)
        .push_int(2)
        .push_opcode(OP_GREATERTHANOREQUAL)
        .push_opcode(OP_SWAP)
        .push_int(3)
        .push_opcode(OP_LESSTHANOREQUAL)
        .push_opcode(OP_BOOLAND)
        .push_opcode(OP_VERIFY)
        .push_opcode(OP_SWAP)
        .push_opcode(OP_SIZE)
        .push_int(32)
        .push_opcode(OP_EQUALVERIFY);
    // A signer must not also occupy another signature position in a leaf.
    for signer in [operator, rollover, chop, renewal, withdraw, craft] {
        builder = builder
            .push_opcode(OP_DUP)
            .push_x_only_key(&signer)
            .push_opcode(OP_EQUAL)
            .push_opcode(OP_NOT)
            .push_opcode(OP_VERIFY);
    }
    builder = builder.push_opcode(OP_TOALTSTACK);

    // Match ark-core's btcd-compatible FIFO tree construction:
    // A=(chop,renewal), B=(watchtower,withdraw), C=(craft,exit), root=(C,(A,B)).
    builder = push_owner_leaf(builder, operator, chop, internal_key)?;
    builder = push_owner_leaf(builder, operator, renewal, internal_key)?;
    builder = push_sorted_branch(builder);

    let watchtower = ArkadeTapscript::Multisig {
        pubkeys: vec![operator, rollover, renewal],
    }
    .encode()
    .context("encode player watchtower template leaf")?;
    builder = push_fixed_leaf(builder, &watchtower);
    builder = push_owner_leaf(builder, operator, withdraw, internal_key)?;
    builder = push_sorted_branch(builder);

    builder = push_owner_leaf(builder, operator, craft, internal_key)?;
    let exit = ark_core::script::csv_sig_script(exit_delay, internal_key);
    builder = push_fixed_leaf(builder, &exit);
    builder = push_sorted_branch(builder)
        .push_opcode(OP_ROT)
        .push_opcode(OP_ROT);
    builder = push_sorted_branch(builder);
    builder = push_sorted_branch(builder);

    // Stack: prefix, root. Compute taggedHash("TapTweak", NUMS || root).
    let mut tweak_prefix = tagged_prefix("TapTweak");
    tweak_prefix.extend_from_slice(&internal_key.serialize());
    Ok(builder
        .push_slice(push(tweak_prefix))
        .push_opcode(OP_SWAP)
        .push_opcode(OP_CAT)
        .push_opcode(OP_SHA256)
        .push_x_only_key(&internal_key)
        .push_opcode(OP_SWAP)
        // prefix, P, k -> P, k, prefix
        .push_int(2)
        .push_opcode(OP_ROLL)
        .push_int(crate::protocol::PLAYER_STATE_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_CAT)
        .push_opcode(op::TWEAKVERIFY)
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DROP))
}

/// Hash an owner-dependent leaf, retaining the owner on the altstack.
fn push_owner_leaf(
    builder: Builder,
    operator: XOnlyPublicKey,
    tweaked_emulator: XOnlyPublicKey,
    placeholder_owner: XOnlyPublicKey,
) -> Result<Builder> {
    let leaf = ArkadeTapscript::Multisig {
        pubkeys: vec![placeholder_owner, operator, tweaked_emulator],
    }
    .encode()
    .context("encode player owner template leaf")?;
    let mut prefix = tagged_prefix("TapLeaf");
    prefix.push(bitcoin::taproot::LeafVersion::TapScript.to_consensus());
    prefix.extend(bitcoin::consensus::serialize(&bitcoin::VarInt(
        leaf.len() as u64
    )));
    // The canonical first instruction is a 32-byte pubkey push.
    prefix.push(32);
    Ok(builder
        .push_slice(push(prefix))
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_CAT)
        .push_slice(push(leaf.as_bytes()[33..].to_vec()))
        .push_opcode(OP_CAT)
        .push_opcode(OP_SHA256))
}

fn push_fixed_leaf(builder: Builder, leaf: &ScriptBuf) -> Builder {
    builder.push_slice(
        TapLeafHash::from_script(leaf, bitcoin::taproot::LeafVersion::TapScript).to_byte_array(),
    )
}

/// Hash the top two nodes in BIP341 byte-lexicographic order. Reverse before
/// BIN2NUM because Script integers are little endian, and append a positive sign.
fn push_sorted_branch(builder: Builder) -> Builder {
    let builder = builder.push_opcode(OP_2DUP);
    let builder = push_hash_number(builder).push_opcode(OP_SWAP);
    let builder = push_hash_number(builder)
        .push_opcode(OP_LESSTHAN)
        .push_opcode(OP_IF)
        .push_opcode(OP_SWAP)
        .push_opcode(OP_ENDIF)
        .push_opcode(OP_CAT);
    builder
        .push_slice(push(tagged_prefix("TapBranch")))
        .push_opcode(OP_SWAP)
        .push_opcode(OP_CAT)
        .push_opcode(OP_SHA256)
}

fn push_hash_number(builder: Builder) -> Builder {
    builder
        .push_opcode(REVERSE_BYTES)
        .push_slice([0])
        .push_opcode(OP_CAT)
        .push_opcode(op::BIN2NUM)
}

fn tagged_prefix(tag: &str) -> Vec<u8> {
    let hash = sha256::Hash::hash(tag.as_bytes()).to_byte_array();
    [hash, hash].concat()
}

fn push(bytes: Vec<u8>) -> PushBytesBuf {
    PushBytesBuf::try_from(bytes).expect("template constants are bounded pushes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{Keypair, Parity, Secp256k1, SecretKey};
    use bitcoin::taproot::{LeafVersion, TaprootBuilder};
    use bitcoin::{Amount, Network, OutPoint, Transaction, TxIn, TxOut};
    use serde_json::{json, Value};

    fn key(byte: u8) -> XOnlyPublicKey {
        Keypair::from_secret_key(
            &Secp256k1::new(),
            &SecretKey::from_slice(&[byte; 32]).unwrap(),
        )
        .x_only_public_key()
        .0
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn multisig(keys: Vec<XOnlyPublicKey>) -> ScriptBuf {
        ArkadeTapscript::Multisig { pubkeys: keys }
            .encode()
            .unwrap()
    }

    fn canonical_leaves(
        owner: XOnlyPublicKey,
        operator: XOnlyPublicKey,
        rollover: XOnlyPublicKey,
        emulators: [XOnlyPublicKey; 4],
        exit: Sequence,
        nums: XOnlyPublicKey,
    ) -> Vec<ScriptBuf> {
        vec![
            multisig(vec![owner, operator, emulators[0]]),
            multisig(vec![owner, operator, emulators[1]]),
            multisig(vec![operator, rollover, emulators[1]]),
            multisig(vec![owner, operator, emulators[2]]),
            multisig(vec![owner, operator, emulators[3]]),
            ark_core::script::csv_sig_script(exit, nums),
        ]
    }

    fn output_for(
        leaves: Vec<ScriptBuf>,
        owner: XOnlyPublicKey,
        operator: XOnlyPublicKey,
        exit: Sequence,
    ) -> (ScriptBuf, u8) {
        let first = leaves[0].clone();
        let vtxo = ark_core::Vtxo::new_with_custom_scripts(
            &Secp256k1::new(),
            operator,
            owner,
            leaves,
            exit,
            Network::Regtest,
        )
        .unwrap();
        let parity = vtxo.get_spend_info(first).unwrap().output_key_parity;
        (
            vtxo.script_pubkey(),
            if parity == Parity::Odd { 3 } else { 2 },
        )
    }

    fn vector(
        name: String,
        script: &ScriptBuf,
        witness: Vec<Vec<u8>>,
        input_script: &ScriptBuf,
        valid: bool,
    ) -> Value {
        let previous = Transaction {
            version: bitcoin::transaction::Version::non_standard(3),
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![TxOut {
                value: Amount::from_sat(330),
                script_pubkey: input_script.clone(),
            }],
        };
        let outpoint = OutPoint {
            txid: previous.compute_txid(),
            vout: 0,
        };
        let transaction = Transaction {
            version: bitcoin::transaction::Version::non_standard(3),
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: outpoint,
                ..TxIn::default()
            }],
            output: previous.output.clone(),
        };
        json!({
            "name": name,
            "script": hex(script.as_bytes()),
            "witness": witness.iter().map(|item| hex(item)).collect::<Vec<_>>(),
            "transaction": hex(&bitcoin::consensus::serialize(&transaction)),
            "input_index": 0,
            "prevouts": [{
                "outpoint": {"txid": outpoint.txid.to_string(), "vout": 0},
                "txout": {"value": 330, "script": hex(input_script.as_bytes())},
                "ark_tx": hex(&bitcoin::consensus::serialize(&previous)),
                "vtxo_script": hex(input_script.as_bytes())
            }],
            "valid": valid
        })
    }

    /// Native construction checks plus defensive vectors for the separately
    /// pinned stock-emulator runner. Setting the variable exports the vectors;
    /// the native test itself never claims to execute Arkade opcodes.
    #[test]
    fn covenant_vm_vectors_complete_player_template() {
        let operator = key(1);
        let rollover = key(2);
        let emulators = [key(3), key(4), key(5), key(6)];
        let exit = Sequence::from_512_second_intervals(144);
        let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY.parse().unwrap();
        let nums = nums.inner.x_only_public_key().0;
        let script = push_player_template(Builder::new(), operator, rollover, exit, emulators)
            .unwrap()
            .push_int(1)
            .into_script();
        let mut vectors = Vec::new();
        let mut observed_parities = std::collections::HashSet::new();
        for owner_byte in 20..40 {
            let owner = key(owner_byte);
            let leaves = canonical_leaves(owner, operator, rollover, emulators, exit, nums);
            let (canonical_script, prefix) = output_for(leaves.clone(), owner, operator, exit);
            observed_parities.insert(prefix);
            // Independently express the six-leaf SDK tree's exact depths.
            let reference =
                leaves
                    .iter()
                    .enumerate()
                    .fold(TaprootBuilder::new(), |builder, (index, leaf)| {
                        builder
                            .add_leaf_with_ver(
                                if index < 4 { 3 } else { 2 },
                                leaf.clone(),
                                LeafVersion::TapScript,
                            )
                            .unwrap()
                    });
            let reference = reference.finalize(&Secp256k1::new(), nums).unwrap();
            assert_eq!(
                canonical_script,
                ScriptBuf::new_p2tr_tweaked(reference.output_key())
            );
            let witness = vec![owner.serialize().to_vec(), vec![prefix]];
            vectors.push(vector(
                format!("template-owner-{owner_byte}-canonical"),
                &script,
                witness.clone(),
                &canonical_script,
                true,
            ));
            for variation in 0..7 {
                let mut altered = leaves.clone();
                let label = match variation {
                    0 => {
                        altered.push(multisig(vec![owner, operator]));
                        "extra-leaf"
                    }
                    1 => {
                        altered[5] = ark_core::script::csv_sig_script(exit, owner);
                        "spendable-exit"
                    }
                    2 => {
                        altered[2] = multisig(vec![operator, owner, emulators[1]]);
                        "changed-watchtower"
                    }
                    3 => {
                        altered[1] = multisig(vec![owner, operator, emulators[0]]);
                        "changed-renewal"
                    }
                    4 => {
                        altered.remove(2);
                        "omitted-leaf"
                    }
                    5 => {
                        altered.swap(1, 3);
                        "different-tree-shape"
                    }
                    _ => {
                        altered[5] = ark_core::script::csv_sig_script(Sequence::MAX, nums);
                        "changed-exit-delay"
                    }
                };
                let (altered_script, altered_prefix) = output_for(altered, owner, operator, exit);
                assert_ne!(altered_script, canonical_script);
                vectors.push(vector(
                    format!("template-owner-{owner_byte}-{label}"),
                    &script,
                    vec![owner.serialize().to_vec(), vec![altered_prefix]],
                    &altered_script,
                    false,
                ));
            }
            // An attacker-known internal key is also an unrestricted spend path.
            let alternate =
                leaves
                    .iter()
                    .enumerate()
                    .fold(TaprootBuilder::new(), |builder, (index, leaf)| {
                        builder
                            .add_leaf(if index < 4 { 3 } else { 2 }, leaf.clone())
                            .unwrap()
                    });
            let alternate = alternate.finalize(&Secp256k1::new(), owner).unwrap();
            let alternate_script = ScriptBuf::new_p2tr_tweaked(alternate.output_key());
            assert_ne!(alternate_script, canonical_script);
            vectors.push(vector(
                format!("template-owner-{owner_byte}-different-internal-key"),
                &script,
                vec![
                    owner.serialize().to_vec(),
                    vec![if alternate.output_key_parity() == Parity::Odd {
                        3
                    } else {
                        2
                    }],
                ],
                &alternate_script,
                false,
            ));
            for (label, bad_witness) in [
                (
                    "wrong-owner",
                    vec![key(owner_byte + 1).serialize().to_vec(), vec![prefix]],
                ),
                (
                    "wrong-parity",
                    vec![owner.serialize().to_vec(), vec![prefix ^ 1]],
                ),
                ("invalid-prefix", vec![owner.serialize().to_vec(), vec![0]]),
                (
                    "wide-prefix",
                    vec![owner.serialize().to_vec(), vec![prefix, 0]],
                ),
                ("short-owner", vec![vec![owner_byte; 31], vec![prefix]]),
            ] {
                vectors.push(vector(
                    format!("template-owner-{owner_byte}-{label}"),
                    &script,
                    bad_witness,
                    &canonical_script,
                    false,
                ));
            }
        }
        assert_eq!(observed_parities.len(), 2);
        assert_eq!(vectors.len(), 280);
        if let Some(directory) = std::env::var_os("WOODLAND_COVENANT_VECTORS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("template.json"),
                serde_json::to_vec_pretty(&vectors).unwrap(),
            )
            .unwrap();
        }
    }
}
