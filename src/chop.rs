//! Durable journal for a prepared browser chop.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use bitcoin::{OutPoint, Psbt, Txid};
use serde::{Deserialize, Serialize};

const PENDING_CHOP_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingChop {
    schema_version: u32,
    pub(crate) tree_id: u32,
    pub(crate) success: bool,
    pub(crate) expected_txid: String,
    player_state_input: String,
    tree_input: String,
    ark_psbt: String,
    checkpoint_psbts: Vec<String>,
}

impl PendingChop {
    pub(crate) fn new(
        tree_id: u32,
        success: bool,
        player_state_input: OutPoint,
        tree_input: OutPoint,
        ark_psbt: &Psbt,
        checkpoint_psbts: &[Psbt],
    ) -> Self {
        Self {
            schema_version: PENDING_CHOP_SCHEMA_VERSION,
            tree_id,
            success,
            expected_txid: ark_psbt.unsigned_tx.compute_txid().to_string(),
            player_state_input: player_state_input.to_string(),
            tree_input: tree_input.to_string(),
            ark_psbt: encode_psbt(ark_psbt),
            checkpoint_psbts: checkpoint_psbts.iter().map(encode_psbt).collect(),
        }
    }

    pub(crate) fn from_json(json: &str) -> Result<Self> {
        let pending: Self = serde_json::from_str(json).context("parse pending chop journal")?;
        if pending.schema_version != PENDING_CHOP_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported pending chop journal schema {}",
                pending.schema_version
            ));
        }
        pending.player_state_input.parse::<OutPoint>()?;
        pending.tree_input.parse::<OutPoint>()?;
        let expected_txid = pending.expected_txid.parse::<Txid>()?;
        let (ark_psbt, checkpoints) = pending.decode_psbts()?;
        if ark_psbt.unsigned_tx.compute_txid() != expected_txid
            || ark_psbt.unsigned_tx.input.len() != crate::protocol::CHOP_INPUT_COUNT
            || ark_psbt.unsigned_tx.output.len() != crate::protocol::CHOP_OUTPUT_COUNT
            || checkpoints.len() != crate::protocol::CHOP_INPUT_COUNT
        {
            return Err(anyhow!("pending chop journal transaction shape is invalid"));
        }
        Ok(pending)
    }

    pub(crate) fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialize pending chop journal")
    }

    pub(crate) fn txid(&self) -> Result<Txid> {
        self.expected_txid
            .parse()
            .context("parse pending chop transaction ID")
    }

    pub(crate) fn player_state_input(&self) -> Result<OutPoint> {
        self.player_state_input
            .parse()
            .context("parse pending player-state input")
    }

    pub(crate) fn tree_input(&self) -> Result<OutPoint> {
        self.tree_input.parse().context("parse pending tree input")
    }

    pub(crate) fn decode_psbts(&self) -> Result<(Psbt, Vec<Psbt>)> {
        let ark_psbt = decode_psbt(&self.ark_psbt).context("decode pending chop Ark PSBT")?;
        let checkpoints = self
            .checkpoint_psbts
            .iter()
            .map(|encoded| decode_psbt(encoded).context("decode pending chop checkpoint"))
            .collect::<Result<Vec<_>>>()?;
        Ok((ark_psbt, checkpoints))
    }
}

fn encode_psbt(psbt: &Psbt) -> String {
    base64::engine::general_purpose::STANDARD.encode(psbt.serialize())
}

fn decode_psbt(encoded: &str) -> Result<Psbt> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("decode PSBT base64")?;
    Psbt::deserialize(&bytes).context("decode PSBT bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::{absolute, transaction, Amount, ScriptBuf, Transaction, TxIn, TxOut};

    fn psbt(inputs: usize, outputs: usize) -> Psbt {
        Psbt::from_unsigned_tx(Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default(); inputs],
            output: vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new(),
                };
                outputs
            ],
        })
        .unwrap()
    }

    #[test]
    fn pending_chop_round_trips_exact_prepared_transaction() {
        let ark = psbt(
            crate::protocol::CHOP_INPUT_COUNT,
            crate::protocol::CHOP_OUTPUT_COUNT,
        );
        let checkpoints = (0..crate::protocol::CHOP_INPUT_COUNT)
            .map(|_| psbt(1, 1))
            .collect::<Vec<_>>();
        let pending = PendingChop::new(
            417,
            true,
            OutPoint::null(),
            OutPoint::null(),
            &ark,
            &checkpoints,
        );
        let decoded = PendingChop::from_json(&pending.to_json().unwrap()).unwrap();
        let (decoded_ark, decoded_checkpoints) = decoded.decode_psbts().unwrap();
        assert_eq!(decoded.txid().unwrap(), ark.unsigned_tx.compute_txid());
        assert_eq!(decoded.player_state_input().unwrap(), OutPoint::null());
        assert_eq!(decoded.tree_input().unwrap(), OutPoint::null());
        assert_eq!(decoded_ark, ark);
        assert_eq!(decoded_checkpoints, checkpoints);
    }

    #[test]
    fn pending_chop_rejects_tampered_identity_or_shape() {
        let ark = psbt(
            crate::protocol::CHOP_INPUT_COUNT,
            crate::protocol::CHOP_OUTPUT_COUNT,
        );
        let checkpoints = (0..crate::protocol::CHOP_INPUT_COUNT)
            .map(|_| psbt(1, 1))
            .collect::<Vec<_>>();
        let pending = PendingChop::new(
            417,
            false,
            OutPoint::null(),
            OutPoint::null(),
            &ark,
            &checkpoints,
        );
        let mut json = serde_json::to_value(pending).unwrap();
        json["expectedTxid"] = serde_json::Value::String(Txid::all_zeros().to_string());
        assert!(PendingChop::from_json(&json.to_string()).is_err());
        json["expectedTxid"] =
            serde_json::Value::String(ark.unsigned_tx.compute_txid().to_string());
        json["checkpointPsbts"].as_array_mut().unwrap().pop();
        assert!(PendingChop::from_json(&json.to_string()).is_err());
    }
}
