//! Supply vault covenant.
//!
//! The vault holds the world's undistributed LOG and XP inside a covenant, not
//! a wallet. It has exactly two moves: the atomic retire-and-restock that
//! replaces one depleted tree at the same coordinate, and an exact-self-send
//! batch renewal. Nobody can redirect the supply anywhere else.

use crate::protocol::{
    RENEWAL_STATE_INPUT_INDEX, RENEWAL_STATE_OUTPUT_INDEX, RESTOCK_ANCHOR_OUTPUT_INDEX,
    RESTOCK_ASSET_GROUP_COUNT, RESTOCK_EXTENSION_OUTPUT_INDEX, RESTOCK_INPUT_COUNT,
    RESTOCK_OUTPUT_COUNT, RESTOCK_TREE_OUTPUT_INDEX, RESTOCK_VAULT_INPUT_INDEX,
    RESTOCK_VAULT_OUTPUT_INDEX,
};
use crate::tree::{
    push_extension_and_anchor_shape, push_input_asset_lookup, push_output_asset_lookup,
    push_renewal_asset_shell, push_renewal_shape,
};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use ark_script::{op, ArkadeLeaf, ArkadeTapscript, ArkadeVtxoInput, ArkadeVtxoScript};
use bitcoin::opcodes::all::{OP_EQUAL, OP_EQUALVERIFY, OP_SUB};
use bitcoin::script::{Builder, PushBytesBuf};
use bitcoin::secp256k1::{Secp256k1, Verification};
use bitcoin::{Network, Script, ScriptBuf, Sequence, XOnlyPublicKey};

/// The complete contract material needed to fund and later spend the vault.
#[derive(Clone, Debug)]
pub struct VaultContract {
    pub vtxo: ark_core::Vtxo,
    pub restock_spend_script: ScriptBuf,
    pub restock_arkade_script: ScriptBuf,
    pub renewal_spend_script: ScriptBuf,
    pub renewal_arkade_script: ScriptBuf,
}

/// Build the two-leaf vault contract: permissionless restock and
/// permissionless exact-self-send renewal, each operator + covenant-tweaked
/// emulator. `tree_script` pins the covenant P2TR the restocked tree must
/// land on, and the reserve constants pin the exact per-tree restock amounts.
#[allow(clippy::too_many_arguments)]
pub fn build_vault_contract<C: Verification>(
    secp: &Secp256k1<C>,
    operator_pk: XOnlyPublicKey,
    emulator_pk: XOnlyPublicKey,
    exit_delay: Sequence,
    network: Network,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    tree_script: &Script,
    log_reserve_per_tree: u64,
    xp_per_tree: u64,
    dust_sats: u64,
) -> Result<VaultContract> {
    if [tree_asset, log_asset, xp_asset]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("TREE, LOG, and XP asset IDs must differ"));
    }
    if operator_pk == emulator_pk {
        return Err(anyhow!("vault contract signers must be distinct"));
    }
    let restock_arkade_script = vault_restock_covenant_script(
        tree_asset,
        log_asset,
        xp_asset,
        tree_script,
        log_reserve_per_tree,
        xp_per_tree,
        dust_sats,
    )?;
    let renewal_arkade_script = vault_renewal_covenant_script(log_asset, xp_asset)?;
    // arkd requires a timelocked exit leaf on every batch VTXO; key it to the
    // NUMS owner like the tree covenant so no one can exit around the vault.
    let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY
        .parse()
        .context("parse Arkade NUMS key")?;
    let owner = nums.inner.x_only_public_key().0;
    for (arkade_script, signers) in [
        (&restock_arkade_script, [operator_pk].as_slice()),
        (&renewal_arkade_script, [operator_pk].as_slice()),
    ] {
        let tweaked_emulator =
            ark_script::compute_arkade_script_public_key(&emulator_pk, arkade_script)
                .context("derive vault emulator signer")?;
        if signers.contains(&tweaked_emulator) {
            return Err(anyhow!("tweaked emulator collides with a vault signer"));
        }
        if tweaked_emulator == owner {
            return Err(anyhow!("tweaked emulator collides with the exit key"));
        }
    }
    let leaf = |arkade_script: ScriptBuf, pubkeys: Vec<XOnlyPublicKey>| {
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script,
            tapscript: ArkadeTapscript::Multisig { pubkeys },
            introspectors: vec![emulator_pk],
        })
    };
    let processed = ArkadeVtxoScript::new(vec![
        leaf(restock_arkade_script.clone(), vec![operator_pk]),
        leaf(renewal_arkade_script.clone(), vec![operator_pk]),
    ])
    .context("build vault Arkade tapleaves")?;
    let [restock_spend_script, renewal_spend_script] = processed.scripts.as_slice() else {
        return Err(anyhow!(
            "vault contract must have restock and renewal leaves"
        ));
    };
    let restock_spend_script = restock_spend_script.clone();
    let renewal_spend_script = renewal_spend_script.clone();
    let scripts = processed
        .scripts
        .into_iter()
        .chain([ark_core::script::csv_sig_script(exit_delay, owner)])
        .collect();
    let vtxo = ark_core::Vtxo::new_with_custom_scripts(
        secp,
        operator_pk,
        owner,
        scripts,
        exit_delay,
        network,
    )
    .map_err(|error| anyhow!("build vault VTXO: {error}"))?;

    Ok(VaultContract {
        vtxo,
        restock_spend_script,
        restock_arkade_script,
        renewal_spend_script,
        renewal_arkade_script,
    })
}

/// Vault half of the atomic retire-and-restock: the vault gives exactly one
/// tree's reserve to the new tree output and keeps the change. The reciprocal
/// tree half proves identity, health, and marker continuity.
///
/// Canonical shape:
///
/// ```text
/// vin 0 dead tree | vin 1 vault
/// vout 0 new tree | vout 1 vault | vout 2 extension | vout 3 anchor
/// groups 0..2 TREE | LOG | XP
/// ```
pub fn vault_restock_covenant_script(
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    tree_script: &Script,
    log_reserve_per_tree: u64,
    xp_per_tree: u64,
    dust_sats: u64,
) -> Result<ScriptBuf> {
    if [tree_asset, log_asset, xp_asset]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("TREE, LOG, and XP asset IDs must differ"));
    }
    let log_reserve = crate::tree::script_int(log_reserve_per_tree, "tree LOG reserve")?;
    let xp_reserve = crate::tree::script_int(xp_per_tree, "tree XP reserve")?;
    let dust = crate::tree::script_int(dust_sats, "tree dust")?;
    if !tree_script.is_p2tr() {
        return Err(anyhow!("tree script must be a P2TR covenant"));
    }
    let tree_program = PushBytesBuf::try_from(tree_script.as_bytes()[2..].to_vec())
        .map_err(|error| anyhow!("invalid tree script witness program: {error}"))?;
    let anchor_program =
        crate::tree::witness_v1_program(&ark_core::anchor_output().script_pubkey, "Arkade anchor")?;
    let builder = Builder::new()
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(RESTOCK_VAULT_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(RESTOCK_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(RESTOCK_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(RESTOCK_ASSET_GROUP_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        // The vault holds exactly LOG and XP, never a TREE marker.
        .push_int(RESTOCK_VAULT_INPUT_INDEX as i64)
        .push_opcode(op::INSPECTINASSETCOUNT)
        .push_int(2)
        .push_opcode(OP_EQUALVERIFY)
        // The new tree lands on the pinned covenant P2TR with one dust.
        .push_int(i64::from(RESTOCK_TREE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_slice(tree_program)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(RESTOCK_TREE_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(dust)
        .push_opcode(OP_EQUALVERIFY);
    // The vault change keeps the exact vault P2TR and dust.
    let builder = crate::tree::push_equal_input_output_scripts(
        builder,
        RESTOCK_VAULT_INPUT_INDEX,
        RESTOCK_VAULT_OUTPUT_INDEX,
    )
    .push_int(i64::from(RESTOCK_VAULT_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(RESTOCK_VAULT_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_opcode(OP_EQUALVERIFY);
    let builder = push_extension_and_anchor_shape(
        builder,
        RESTOCK_EXTENSION_OUTPUT_INDEX,
        RESTOCK_ANCHOR_OUTPUT_INDEX,
        &anchor_program,
    )
    .push_int(i64::from(RESTOCK_EXTENSION_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(RESTOCK_ANCHOR_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY);

    let builder = crate::tree::push_canonical_asset_group(builder, tree_asset, 1, 1);
    let builder = crate::tree::push_canonical_asset_group(builder, log_asset, 1, 2);
    let builder = crate::tree::push_canonical_asset_group(builder, xp_asset, 1, 2);
    // The marker moves from the dead tree input to the new tree output.
    let builder = crate::tree::push_restock_asset_input(builder, tree_asset, 0);
    // LOG and XP leave the vault at exactly the per-tree reserve delta.
    let builder =
        crate::tree::push_restock_asset_input(builder, log_asset, RESTOCK_VAULT_INPUT_INDEX);
    let builder =
        crate::tree::push_restock_asset_input(builder, xp_asset, RESTOCK_VAULT_INPUT_INDEX);

    let builder = push_output_asset_lookup(builder, RESTOCK_TREE_OUTPUT_INDEX, tree_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_input_asset_lookup(builder, RESTOCK_VAULT_INPUT_INDEX, log_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_output_asset_lookup(builder, RESTOCK_VAULT_OUTPUT_INDEX, log_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SUB)
        .push_int(log_reserve)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_input_asset_lookup(builder, RESTOCK_VAULT_INPUT_INDEX, xp_asset)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY);
    Ok(
        push_output_asset_lookup(builder, RESTOCK_VAULT_OUTPUT_INDEX, xp_asset)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_SUB)
            .push_int(xp_reserve)
            .push_opcode(OP_EQUAL)
            .into_script(),
    )
}

/// Covenant for the vault's batch-renewal leaf: a version-2 intent proof that
/// only permits an exact self-send — identical P2TR, value, and LOG/XP
/// balances. Renewal changes nothing; it only re-enters the vault into a
/// fresh batch for a new expiry.
pub fn vault_renewal_covenant_script(log_asset: AssetId, xp_asset: AssetId) -> Result<ScriptBuf> {
    if log_asset == xp_asset {
        return Err(anyhow!("LOG and XP asset IDs must differ"));
    }
    let builder = push_renewal_shape(Builder::new())?;
    let builder = crate::tree::push_equal_input_output_scripts(
        builder,
        RENEWAL_STATE_INPUT_INDEX,
        RENEWAL_STATE_OUTPUT_INDEX,
    )
    .push_int(i64::from(RENEWAL_STATE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(RENEWAL_STATE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_opcode(OP_EQUALVERIFY);
    let builder = push_renewal_asset_shell(builder)?;
    let builder = crate::tree::push_optional_transfer_group_shell(builder, log_asset);
    let builder = crate::tree::push_optional_transfer_group_shell(builder, xp_asset);

    let builder = crate::tree::push_optional_input_asset_lookup(
        builder,
        RENEWAL_STATE_INPUT_INDEX,
        log_asset,
    );
    let builder = crate::tree::push_optional_output_asset_lookup(
        builder,
        RENEWAL_STATE_OUTPUT_INDEX,
        log_asset,
    );
    let builder = builder
        .push_opcode(bitcoin::opcodes::all::OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY);

    let builder =
        crate::tree::push_optional_input_asset_lookup(builder, RENEWAL_STATE_INPUT_INDEX, xp_asset);
    let builder = crate::tree::push_optional_output_asset_lookup(
        builder,
        RENEWAL_STATE_OUTPUT_INDEX,
        xp_asset,
    );
    Ok(builder
        .push_opcode(bitcoin::opcodes::all::OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUAL)
        .into_script())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};

    fn asset(byte: u8, group_index: u16) -> AssetId {
        AssetId {
            txid: bitcoin::Txid::from_byte_array([byte; 32]),
            group_index,
        }
    }

    fn xonly(secp: &Secp256k1<bitcoin::secp256k1::All>, byte: u8) -> XOnlyPublicKey {
        Keypair::from_secret_key(secp, &SecretKey::from_slice(&[byte; 32]).unwrap())
            .x_only_public_key()
            .0
    }

    #[test]
    fn vault_contract_builds_restock_and_renewal_leaves() {
        let secp = Secp256k1::new();
        let operator = xonly(&secp, 3);
        let emulator = xonly(&secp, 4);
        let tree_contract = crate::tree::build_tree_contract(
            &secp,
            operator,
            emulator,
            Sequence::from_height(144),
            Network::Regtest,
            asset(1, 0),
            asset(1, 1),
            asset(1, 2),
            1_000,
            1_000,
            330,
        )
        .unwrap();
        let contract = build_vault_contract(
            &secp,
            operator,
            emulator,
            Sequence::from_height(144),
            Network::Regtest,
            asset(1, 0),
            asset(1, 1),
            asset(1, 2),
            &tree_contract.vtxo.script_pubkey(),
            1_000,
            1_000,
            330,
        )
        .unwrap();
        assert_eq!(contract.vtxo.tapscripts().len(), 3);
        let restock_asm = ark_script::to_asm(&contract.restock_arkade_script).unwrap();
        assert!(restock_asm.contains("OP_INSPECTNUMINPUTS OP_PUSHNUM_2 OP_EQUALVERIFY"));
        let renewal_asm = ark_script::to_asm(&contract.renewal_arkade_script).unwrap();
        assert!(renewal_asm.contains("OP_INSPECTNUMINPUTS OP_PUSHNUM_2 OP_EQUALVERIFY"));
        for (spend, arkade) in [
            (
                &contract.restock_spend_script,
                &contract.restock_arkade_script,
            ),
            (
                &contract.renewal_spend_script,
                &contract.renewal_arkade_script,
            ),
        ] {
            let tweaked = ark_script::compute_arkade_script_public_key(&emulator, arkade).unwrap();
            assert_ne!(tweaked, operator);
            let signers = ark_core::script::extract_checksig_pubkeys(spend);
            assert_eq!(signers.len(), 2);
            assert!(signers.contains(&operator));
            assert!(signers.contains(&tweaked));
        }
    }
}
