//! Canonical transaction indexes and extension packet types for protocol v3.
//!
//! Both covenant halves, transaction builders, state discovery, and tests use
//! these values. They are consensus-like protocol shape, not client defaults.
//!
//! Packet types 3, 4, and 6 belonged to redundant protocol-v2 player state.
//! They stay retired rather than gaining incompatible meanings. Protocol-v3
//! axe state uses the next unallocated packet type.

pub const TREE_STATE_PACKET_TYPE: u8 = 2;
pub const PLAYER_ROLL_PACKET_TYPE: u8 = 5;
pub const TREE_HEALTH_PACKET_TYPE: u8 = 7;
pub const PLAYER_LUCK_CREDIT_PACKET_TYPE: u8 = 8;
pub const PLAYER_AXE_PACKET_TYPE: u8 = 9;

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
pub const STONE_ASSET_GROUP_INDEX: usize = 4;
pub const IRON_ORE_ASSET_GROUP_INDEX: usize = 5;
pub const CHOP_ASSET_GROUP_COUNT: usize = 6;

pub const ACTIVATION_STATE_OUTPUT_INDEX: u16 = 0;
pub const ACTIVATION_OUTPUT_COUNT: usize = 3;
/// Direct axe crafting spends one player state VTXO and recreates it beside
/// the extension and anchor outputs while burning the exact recipe inputs.
pub const CRAFT_TRANSACTION_VERSION: i64 = 3;
pub const CRAFT_STATE_INPUT_INDEX: usize = 0;
pub const CRAFT_STATE_OUTPUT_INDEX: u16 = 0;
pub const CRAFT_EXTENSION_OUTPUT_INDEX: u16 = 1;
pub const CRAFT_ANCHOR_OUTPUT_INDEX: u16 = 2;
pub const CRAFT_INPUT_COUNT: usize = 1;
pub const CRAFT_OUTPUT_COUNT: usize = 3;

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
/// and anchor out. XP, crafting materials, sats, PLAYER_ID, and all packets
/// stay in the state, so only LOG can move.
pub const WITHDRAW_STATE_INPUT_INDEX: usize = 0;
pub const WITHDRAW_FUNDING_INPUT_INDEX: usize = 1;
pub const WITHDRAW_STATE_OUTPUT_INDEX: u16 = 0;
pub const WITHDRAW_DESTINATION_OUTPUT_INDEX: u16 = 1;
pub const WITHDRAW_EXTENSION_OUTPUT_INDEX: u16 = 2;
pub const WITHDRAW_ANCHOR_OUTPUT_INDEX: u16 = 3;
pub const WITHDRAW_INPUT_COUNT: usize = 2;
pub const WITHDRAW_OUTPUT_COUNT: usize = 4;
pub const WITHDRAW_ASSET_GROUP_COUNT: usize = 5;

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
                STONE_ASSET_GROUP_INDEX,
                IRON_ORE_ASSET_GROUP_INDEX,
            ],
            [0, 1, 2, 3, 4, 5]
        );
        assert_eq!(CHOP_INPUT_COUNT, 2);
        assert_eq!(CHOP_OUTPUT_COUNT, 4);
        assert_eq!(CHOP_ASSET_GROUP_COUNT, 6);
        assert_eq!(ACTIVATION_STATE_OUTPUT_INDEX, 0);
        assert_eq!(ACTIVATION_OUTPUT_COUNT, 3);
        assert_eq!(CRAFT_STATE_INPUT_INDEX, 0);
        assert_eq!(
            [
                usize::from(CRAFT_STATE_OUTPUT_INDEX),
                usize::from(CRAFT_EXTENSION_OUTPUT_INDEX),
                usize::from(CRAFT_ANCHOR_OUTPUT_INDEX),
            ],
            [0, 1, 2]
        );
        assert_eq!(CRAFT_INPUT_COUNT, 1);
        assert_eq!(CRAFT_TRANSACTION_VERSION, 3);
        assert_eq!(CRAFT_OUTPUT_COUNT, 3);
        assert_eq!(PLAYER_AXE_PACKET_TYPE, 9);
    }
}
