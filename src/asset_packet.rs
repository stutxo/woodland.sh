//! Strict decoding of Asset V1 output assignments from creating transactions.

use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use ark_core::Asset;
use bitcoin::hashes::Hash;
use bitcoin::{Transaction, Txid};
use std::collections::HashSet;

const ASSET_PACKET_TYPE: u8 = 0;
const PRESENCE_ASSET_ID: u8 = 0x01;
const PRESENCE_CONTROL_ASSET: u8 = 0x02;
const PRESENCE_METADATA: u8 = 0x04;
const KNOWN_PRESENCE_BITS: u8 = PRESENCE_ASSET_ID | PRESENCE_CONTROL_ASSET | PRESENCE_METADATA;
const LOCAL_REFERENCE: u8 = 0x01;
const INTENT_REFERENCE: u8 = 0x02;
const CONTROL_BY_ID: u8 = 0x01;
const CONTROL_BY_GROUP: u8 = 0x02;

pub(crate) fn asset_group_count(transaction: &Transaction) -> Result<usize> {
    let payload = ark_core::extension::find_packet_payload(transaction, ASSET_PACKET_TYPE)
        .context("parse transaction extension packets")?
        .ok_or_else(|| anyhow!("transaction has no asset packet"))?;
    let mut cursor = Cursor::new(payload);
    let group_count = cursor.count("asset group count")?;
    if group_count > usize::from(u16::MAX) + 1 {
        return Err(anyhow!("asset packet has too many groups"));
    }
    Ok(group_count)
}

pub(crate) fn output_assets(transaction: &Transaction, output_index: u32) -> Result<Vec<Asset>> {
    if transaction.output.get(output_index as usize).is_none() {
        return Err(anyhow!(
            "asset reconciliation output {output_index} does not exist"
        ));
    }
    let payload = ark_core::extension::find_packet_payload(transaction, ASSET_PACKET_TYPE)
        .context("parse transaction extension packets")?;
    let Some(payload) = payload else {
        return Ok(Vec::new());
    };

    let mut cursor = Cursor::new(payload);
    let group_count = cursor.count("asset group count")?;
    if group_count > usize::from(u16::MAX) + 1 {
        return Err(anyhow!("asset packet has too many groups"));
    }
    let transaction_id = transaction.compute_txid();
    let mut seen_assets = HashSet::with_capacity(group_count);
    let mut assigned = Vec::new();

    for group_index in 0..group_count {
        let presence = cursor.byte("asset group presence")?;
        if presence & !KNOWN_PRESENCE_BITS != 0 {
            return Err(anyhow!(
                "asset group {group_index} has unknown presence bits {presence:#x}"
            ));
        }
        let asset_id = if presence & PRESENCE_ASSET_ID != 0 {
            cursor.asset_id("asset ID")?
        } else {
            AssetId {
                txid: transaction_id,
                group_index: u16::try_from(group_index)
                    .map_err(|_| anyhow!("asset group index overflow"))?,
            }
        };
        if !seen_assets.insert(asset_id) {
            return Err(anyhow!("asset packet repeats asset {asset_id}"));
        }

        if presence & PRESENCE_CONTROL_ASSET != 0 {
            match cursor.byte("control asset reference type")? {
                CONTROL_BY_ID => {
                    cursor.asset_id("control asset ID")?;
                }
                CONTROL_BY_GROUP => {
                    cursor.u16("control asset group index")?;
                }
                kind => return Err(anyhow!("unsupported control asset reference type {kind}")),
            }
        }
        if presence & PRESENCE_METADATA != 0 {
            cursor.metadata()?;
        }

        let input_count = cursor.count("asset input count")?;
        let mut seen_inputs = HashSet::with_capacity(input_count);
        for _ in 0..input_count {
            let reference_type = cursor.byte("asset input reference type")?;
            let (source_txid, input_index) = match reference_type {
                LOCAL_REFERENCE => {
                    let input_index = cursor.u16("asset input index")?;
                    if usize::from(input_index) >= transaction.input.len() {
                        return Err(anyhow!(
                            "asset group {group_index} has an invalid local input index"
                        ));
                    }
                    ([0; 32], input_index)
                }
                INTENT_REFERENCE => (
                    cursor.bytes::<32>("intent input transaction ID")?,
                    cursor.u16("intent input index")?,
                ),
                kind => return Err(anyhow!("unsupported asset input reference type {kind}")),
            };
            if !seen_inputs.insert((reference_type, source_txid, input_index)) {
                return Err(anyhow!(
                    "asset group {group_index} repeats an input reference"
                ));
            }
            cursor.varint("asset input amount")?;
        }

        let output_count = cursor.count("asset output count")?;
        let mut seen_outputs = HashSet::with_capacity(output_count);
        for _ in 0..output_count {
            if cursor.byte("asset output reference type")? != LOCAL_REFERENCE {
                return Err(anyhow!("unsupported non-local asset output"));
            }
            let assigned_index = cursor.u16("asset output index")?;
            if usize::from(assigned_index) >= transaction.output.len()
                || !seen_outputs.insert(assigned_index)
            {
                return Err(anyhow!(
                    "asset group {group_index} has an invalid output index"
                ));
            }
            let amount = cursor.varint("asset output amount")?;
            if amount == 0 {
                return Err(anyhow!(
                    "asset group {group_index} has a zero output amount"
                ));
            }
            if u32::from(assigned_index) == output_index {
                assigned.push(Asset { asset_id, amount });
            }
        }
    }
    cursor.finish()?;
    assigned.sort_by_key(|asset| (asset.asset_id.txid, asset.asset_id.group_index));
    Ok(assigned)
}

pub(crate) fn equal_asset_sets(left: &[Asset], right: &[Asset]) -> bool {
    left.len() == right.len()
        && left.iter().all(|expected| {
            right.iter().any(|actual| {
                actual.asset_id == expected.asset_id && actual.amount == expected.amount
            })
        })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn byte(&mut self, name: &str) -> Result<u8> {
        let byte = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or_else(|| anyhow!("truncated {name}"))?;
        self.offset += 1;
        Ok(byte)
    }

    fn bytes<const N: usize>(&mut self, name: &str) -> Result<[u8; N]> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| anyhow!("{name} offset overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| anyhow!("truncated {name}"))?
            .try_into()
            .expect("fixed-size slice");
        self.offset = end;
        Ok(value)
    }

    fn u16(&mut self, name: &str) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(name)?))
    }

    fn asset_id(&mut self, name: &str) -> Result<AssetId> {
        let mut txid = self.bytes::<32>(name)?;
        txid.reverse();
        Ok(AssetId {
            txid: Txid::from_byte_array(txid),
            group_index: self.u16(name)?,
        })
    }

    fn varint(&mut self, name: &str) -> Result<u64> {
        let start = self.offset;
        let mut value = 0_u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte(name)?;
            if shift == 63 && byte > 1 {
                return Err(anyhow!("{name} overflows u64"));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                if self.offset - start > 1 && byte == 0 {
                    return Err(anyhow!("{name} is not minimally encoded"));
                }
                return Ok(value);
            }
        }
        Err(anyhow!("{name} overflows u64"))
    }

    fn count(&mut self, name: &str) -> Result<usize> {
        let count = usize::try_from(self.varint(name)?)
            .map_err(|_| anyhow!("{name} does not fit in memory"))?;
        if count > self.bytes.len().saturating_sub(self.offset) {
            return Err(anyhow!("{name} exceeds the remaining packet"));
        }
        Ok(count)
    }

    fn text(&mut self, name: &str) -> Result<()> {
        let length = self.count(name)?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| anyhow!("{name} offset overflow"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| anyhow!("truncated {name}"))?;
        std::str::from_utf8(bytes).with_context(|| format!("{name} is not UTF-8"))?;
        self.offset = end;
        Ok(())
    }

    fn metadata(&mut self) -> Result<()> {
        let count = self.count("metadata entry count")?;
        for _ in 0..count {
            self.text("metadata key")?;
            self.text("metadata value")?;
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.offset != self.bytes.len() {
            return Err(anyhow!("asset packet has trailing bytes"));
        }
        Ok(())
    }
}
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_payload(payload: &[u8]) {
    let transaction = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn::default()],
        output: vec![
            bitcoin::TxOut {
                value: bitcoin::Amount::ZERO,
                script_pubkey: bitcoin::ScriptBuf::new(),
            },
            ark_core::extension::packet_txout(ASSET_PACKET_TYPE, payload),
        ],
    };
    let _ = output_assets(&transaction, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_core::asset::packet::{AssetGroup, AssetOutput, Packet};
    use bitcoin::{absolute, transaction, Amount, ScriptBuf, TxIn, TxOut};

    fn transaction(packet: Packet) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: ScriptBuf::new(),
                },
                packet.to_txout(),
            ],
        }
    }

    #[test]
    fn derives_issuance_id_and_output_assignment() {
        let tx = transaction(Packet {
            groups: vec![AssetGroup {
                asset_id: None,
                control_asset: None,
                metadata: Some(vec![("game".to_owned(), "woodland.sh".to_owned())]),
                inputs: Vec::new(),
                outputs: vec![AssetOutput {
                    output_index: 0,
                    amount: 5,
                }],
            }],
        });
        assert_eq!(
            output_assets(&tx, 0).unwrap()[0].asset_id,
            AssetId {
                txid: tx.compute_txid(),
                group_index: 0,
            }
        );
        assert_eq!(output_assets(&tx, 0).unwrap()[0].amount, 5);
        assert!(output_assets(&tx, 1).unwrap().is_empty());
    }

    #[test]
    fn rejects_truncated_or_trailing_asset_payloads() {
        let mut tx = transaction(Packet {
            groups: vec![AssetGroup {
                asset_id: None,
                control_asset: None,
                metadata: None,
                inputs: Vec::new(),
                outputs: vec![AssetOutput {
                    output_index: 0,
                    amount: 1,
                }],
            }],
        });
        let extension = tx.output.pop().unwrap();
        let payload = ark_core::extension::find_packet_payload(
            &Transaction {
                output: vec![extension],
                ..tx.clone()
            },
            ASSET_PACKET_TYPE,
        )
        .unwrap()
        .unwrap()
        .to_vec();
        assert!(Cursor::new(&payload[..payload.len() - 1])
            .count("groups")
            .is_ok());
        let mut trailing = payload;
        trailing.push(0);
        let mut cursor = Cursor::new(&trailing);
        let _ = cursor.count("groups").unwrap();
        assert!(cursor.finish().is_err());
    }
}
