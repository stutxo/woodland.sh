//! Canonical transaction indexes and extension packet types for protocol v3.
//!
//! Both covenant halves, transaction builders, state discovery, and tests use
//! these values. They are consensus-like protocol shape, not client defaults.
//!
//! Packet types 3, 4, and 6 belonged to redundant protocol-v2 player state.
//! They stay retired rather than gaining incompatible meanings.

pub const TREE_STATE_PACKET_TYPE: u8 = 2;
pub const PLAYER_ROLL_PACKET_TYPE: u8 = 5;
pub const TREE_HEALTH_PACKET_TYPE: u8 = 7;
pub const PLAYER_LUCK_CREDIT_PACKET_TYPE: u8 = 8;

pub const PLAYER_STATE_INPUT_INDEX: usize = 0;
pub const TREE_INPUT_INDEX: usize = 1;
pub const PLAYER_STATE_OUTPUT_INDEX: u16 = 0;
pub const TREE_OUTPUT_INDEX: u16 = 1;
pub const CHOP_EXTENSION_OUTPUT_INDEX: u16 = 2;
pub const CHOP_ANCHOR_OUTPUT_INDEX: u16 = 3;
pub const CHOP_INPUT_COUNT: usize = 2;
pub const CHOP_OUTPUT_COUNT: usize = 4;
pub const CHOP_OUTPUT_COUNT_BEFORE_EXTENSION: usize = CHOP_OUTPUT_COUNT - 1;
pub const CHOP_FEE_INPUT_INDEX: usize = 2;
pub const CHOP_FEE_CHANGE_OUTPUT_INDEX: u16 = 2;
pub const CHOP_FEE_EXTENSION_OUTPUT_INDEX: u16 = 3;
pub const CHOP_FEE_ANCHOR_OUTPUT_INDEX: u16 = 4;
pub const CHOP_FEE_INPUT_COUNT: usize = 3;
pub const CHOP_FEE_OUTPUT_COUNT: usize = 5;
pub const CHOP_FEE_OUTPUT_COUNT_BEFORE_EXTENSION: usize = CHOP_FEE_OUTPUT_COUNT - 1;

pub const PLAYER_ID_ASSET_GROUP_INDEX: usize = 0;
pub const TREE_ASSET_GROUP_INDEX: usize = 1;
pub const LOG_ASSET_GROUP_INDEX: usize = 2;
pub const XP_ASSET_GROUP_INDEX: usize = 3;
pub const CHOP_ASSET_GROUP_COUNT: usize = 4;

pub const ACTIVATION_STATE_OUTPUT_INDEX: u16 = 0;
pub const ACTIVATION_OUTPUT_COUNT: usize = 3;

/// Batch-renewal intents carry the fake message input at index zero, so the
/// renewed state VTXO is always physical input one. The proof has no anchor.
pub const RENEWAL_STATE_INPUT_INDEX: usize = 1;
pub const RENEWAL_STATE_OUTPUT_INDEX: u16 = 0;
pub const RENEWAL_EXTENSION_OUTPUT_INDEX: u16 = 1;
pub const RENEWAL_INPUT_COUNT: usize = 2;
pub const RENEWAL_OUTPUT_COUNT: usize = 2;
pub const RENEWAL_FEE_INPUT_INDEX: usize = 2;
pub const RENEWAL_FEE_CHANGE_OUTPUT_INDEX: u16 = 1;
pub const RENEWAL_FEE_EXTENSION_OUTPUT_INDEX: u16 = 2;
pub const RENEWAL_FEE_INPUT_COUNT: usize = 3;
pub const RENEWAL_FEE_OUTPUT_COUNT: usize = 3;

/// Owner-authorized LOG withdrawal: player state plus a wallet dust input in,
/// the LOG-depleted player state, the withdrawn LOG destination, extension,
/// and anchor out. XP, sats, PLAYER_ID, and all packets stay in the state, so
/// XP can never move.
pub const WITHDRAW_STATE_INPUT_INDEX: usize = 0;
pub const WITHDRAW_FUNDING_INPUT_INDEX: usize = 1;
pub const WITHDRAW_STATE_OUTPUT_INDEX: u16 = 0;
pub const WITHDRAW_DESTINATION_OUTPUT_INDEX: u16 = 1;
pub const WITHDRAW_EXTENSION_OUTPUT_INDEX: u16 = 2;
pub const WITHDRAW_ANCHOR_OUTPUT_INDEX: u16 = 3;
pub const WITHDRAW_INPUT_COUNT: usize = 2;
pub const WITHDRAW_OUTPUT_COUNT: usize = 4;
pub const WITHDRAW_ASSET_GROUP_COUNT: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_protocol_indexes_are_contiguous() {
        assert_eq!([PLAYER_STATE_INPUT_INDEX, TREE_INPUT_INDEX], [0, 1]);
        assert_eq!(
            [
                usize::from(PLAYER_STATE_OUTPUT_INDEX),
                usize::from(TREE_OUTPUT_INDEX),
                usize::from(CHOP_EXTENSION_OUTPUT_INDEX),
                usize::from(CHOP_ANCHOR_OUTPUT_INDEX),
            ],
            [0, 1, 2, 3]
        );
        assert_eq!(
            [
                PLAYER_ID_ASSET_GROUP_INDEX,
                TREE_ASSET_GROUP_INDEX,
                LOG_ASSET_GROUP_INDEX,
                XP_ASSET_GROUP_INDEX,
            ],
            [0, 1, 2, 3]
        );
        assert_eq!(CHOP_INPUT_COUNT, 2);
        assert_eq!(CHOP_OUTPUT_COUNT, 4);
        assert_eq!(CHOP_ASSET_GROUP_COUNT, 4);
        assert_eq!(ACTIVATION_STATE_OUTPUT_INDEX, 0);
        assert_eq!(ACTIVATION_OUTPUT_COUNT, 3);
    }
}
