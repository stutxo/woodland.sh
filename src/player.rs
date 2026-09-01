//! Recursive player state for atomic tree chopping.
//!
//! A player owns one 330-sat recursive state VTXO. PLAYER_ID identifies its
//! lineage; LOG, XP, STONE, and IRON ORE assets are its inventory and
//! progression; and deterministic luck plus the equipped axe are mutable
//! packets.

use crate::protocol::{
    PLAYER_AXE_PACKET_TYPE, PLAYER_LUCK_CREDIT_PACKET_TYPE, PLAYER_ROLL_PACKET_TYPE,
    PLAYER_STATE_INPUT_INDEX, PLAYER_STATE_OUTPUT_INDEX, RENEWAL_STATE_INPUT_INDEX,
    RENEWAL_STATE_OUTPUT_INDEX, TREE_INPUT_INDEX, TREE_OUTPUT_INDEX,
};
use anyhow::{anyhow, Context, Result};
use ark_core::asset::AssetId;
use ark_core::send::VtxoInput;
use ark_core::Asset;
use ark_script::{op, ArkadeLeaf, ArkadeTapscript, ArkadeVtxoInput, ArkadeVtxoScript};
use bitcoin::hashes::{sha256, Hash, HashEngine};
use bitcoin::opcodes::all::{
    OP_2DROP, OP_ADD, OP_CAT, OP_DROP, OP_DUP, OP_ELSE, OP_ENDIF, OP_EQUAL, OP_EQUALVERIFY,
    OP_FROMALTSTACK, OP_GREATERTHAN, OP_IF, OP_LESSTHAN, OP_MOD, OP_ROT, OP_SHA256, OP_SIZE,
    OP_SUB, OP_TOALTSTACK, OP_VERIFY,
};
use bitcoin::script::{Builder, PushBytesBuf};
use bitcoin::secp256k1::{Message, Secp256k1, Verification};
use bitcoin::{
    Amount, Network, OutPoint, Psbt, ScriptBuf, Sequence, Transaction, Txid, XOnlyPublicKey,
};
use std::collections::{HashMap, HashSet};

const PLAYER_ROLL_LEN: usize = 32;
const PLAYER_LUCK_CREDIT_LEN: usize = 9;
const PLAYER_AXE_LEN: usize = 1;
const P2A_PROGRAM: [u8; 2] = [0x4e, 0x73];

/// BIP340 consent proof for publishing one PLAYER_ID to one game server.
pub fn server_registration_message(
    genesis_txid: Txid,
    owner: XOnlyPublicKey,
    player_asset: AssetId,
    server_url: &str,
) -> Message {
    let preimage = format!(
        "woodland.sh/ServerRegistration/v1\nworld={genesis_txid}\nowner={owner}\nplayerAsset={player_asset}\nserver={server_url}\n"
    );
    Message::from_digest(sha256::Hash::hash(preimage.as_bytes()).to_byte_array())
}

pub const SERVER_ACTION_LOCATION: &str = "location";
pub const SERVER_ACTION_CHAT: &str = "chat";
pub const SERVER_ACTION_DELEGATION: &str = "delegation";

/// BIP340 proof for one time-ordered action accepted by one social server.
pub fn server_action_message(
    genesis_txid: Txid,
    owner: XOnlyPublicKey,
    player_asset: AssetId,
    server_url: &str,
    action: &str,
    timestamp_ms: u64,
    payload: &str,
) -> Message {
    let payload_hash = sha256::Hash::hash(payload.as_bytes());
    let preimage = format!(
        "woodland.sh/ServerAction/v1\nworld={genesis_txid}\nowner={owner}\nplayerAsset={player_asset}\nserver={server_url}\naction={action}\ntimestampMs={timestamp_ms}\npayloadHash={payload_hash}\n"
    );
    Message::from_digest(sha256::Hash::hash(preimage.as_bytes()).to_byte_array())
}

/// XP is the soulbound XP asset balance held by recursive player state. Each
/// earned unit represents one LOG and 25 Woodcutting XP; level and drop chance
/// are derived from that single conserved balance.
pub const PLAYER_LEVEL_CURVE: &str = "woodland-xp-v1";
pub const WOODCUTTING_XP_PER_LOG: u64 = 25;
pub const MAX_PLAYER_LEVEL: u64 = 99;

/// Canonical Woodcutting XP thresholds for levels 1 through 99.
/// Level remains derived state; these values are not stored in the covenant.
pub const XP_FOR_LEVEL: [u64; MAX_PLAYER_LEVEL as usize] = [
    0, 83, 174, 276, 388, 512, 650, 801, 969, 1_154, 1_358, 1_584, 1_833, 2_107, 2_411, 2_746,
    3_115, 3_523, 3_973, 4_470, 5_018, 5_624, 6_291, 7_028, 7_842, 8_740, 9_730, 10_824, 12_031,
    13_363, 14_833, 16_456, 18_247, 20_224, 22_406, 24_815, 27_473, 30_408, 33_648, 37_224, 41_171,
    45_529, 50_339, 55_649, 61_512, 67_983, 75_127, 83_014, 91_721, 101_333, 111_945, 123_660,
    136_594, 150_872, 166_636, 184_040, 203_254, 224_466, 247_886, 273_742, 302_288, 333_804,
    368_599, 407_015, 449_428, 496_254, 547_953, 605_032, 668_051, 737_627, 814_445, 899_257,
    992_895, 1_096_278, 1_210_421, 1_336_443, 1_475_581, 1_629_200, 1_798_808, 1_986_068,
    2_192_818, 2_421_087, 2_673_114, 2_951_373, 3_258_594, 3_597_792, 3_972_294, 4_385_776,
    4_842_295, 5_346_332, 5_902_831, 6_517_253, 7_195_629, 7_944_614, 8_771_558, 9_684_577,
    10_692_629, 11_805_606, 13_034_431,
];

pub const CHOP_ROLL_BASIS_POINTS: u64 = 10_000;
pub const BASE_LOG_DROP_BASIS_POINTS: u64 = 2_000;
pub const LEVEL_LOG_DROP_BONUS_BASIS_POINTS: u64 = 200;
/// User-facing Woodcutting XP at levels 10, 20, 30, 40, and 50.
pub const LEVEL_LOG_DROP_XP_THRESHOLDS: [u64; 5] = [1_154, 4_470, 13_363, 37_224, 101_333];
/// The corresponding soulbound XP asset balances inspected by the covenant.
pub const LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS: [u64; 5] = [
    xp_balance_for_woodcutting_xp(LEVEL_LOG_DROP_XP_THRESHOLDS[0]),
    xp_balance_for_woodcutting_xp(LEVEL_LOG_DROP_XP_THRESHOLDS[1]),
    xp_balance_for_woodcutting_xp(LEVEL_LOG_DROP_XP_THRESHOLDS[2]),
    xp_balance_for_woodcutting_xp(LEVEL_LOG_DROP_XP_THRESHOLDS[3]),
    xp_balance_for_woodcutting_xp(LEVEL_LOG_DROP_XP_THRESHOLDS[4]),
];
pub const MAX_LEVEL_LOG_DROP_BASIS_POINTS: u64 = 3_000;
pub const WOODEN_AXE_BONUS_BASIS_POINTS: u64 = 200;
pub const STONE_AXE_BONUS_BASIS_POINTS: u64 = 500;
pub const IRON_AXE_BONUS_BASIS_POINTS: u64 = 800;
pub const MAX_LOG_DROP_BASIS_POINTS: u64 =
    MAX_LEVEL_LOG_DROP_BASIS_POINTS + IRON_AXE_BONUS_BASIS_POINTS;
pub const STONE_DROP_BASIS_POINTS: u64 = 1_000;
pub const IRON_ORE_DROP_BASIS_POINTS: u64 = 200;
pub const IRON_ORE_UNLOCK_LEVEL: u64 = 10;
pub const IRON_ORE_UNLOCK_XP_BALANCE: u64 =
    xp_balance_for_woodcutting_xp(XP_FOR_LEVEL[(IRON_ORE_UNLOCK_LEVEL - 1) as usize]);
pub const LUCK_WINDOW_BASIS_POINTS: u64 = 10_000;
pub const MAX_LUCK_CREDIT: u64 = LUCK_WINDOW_BASIS_POINTS * 2;
pub const INITIAL_LUCK_CREDIT: u64 = LUCK_WINDOW_BASIS_POINTS - BASE_LOG_DROP_BASIS_POINTS;
const PLAYER_ROLL_DOMAIN: &[u8] = b"woodland.sh/player-roll/v2";
const MATERIAL_ROLL_DOMAIN: &[u8] = b"woodland.sh/material-roll/v1";

/// Convert the canonical soulbound balance into user-facing Woodcutting XP.
/// Saturation is unreachable for the signed fixed-supply world.
pub const fn woodcutting_xp(player_xp_balance: u64) -> u64 {
    player_xp_balance.saturating_mul(WOODCUTTING_XP_PER_LOG)
}

/// Smallest soulbound XP balance that reaches a Woodcutting XP threshold.
pub const fn xp_balance_for_woodcutting_xp(woodcutting_xp: u64) -> u64 {
    woodcutting_xp.div_ceil(WOODCUTTING_XP_PER_LOG)
}

/// Soulbound axe tier. The highest crafted tier is always equipped.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Deserialize,
    serde::Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "camelCase")]
pub enum AxeTier {
    #[default]
    None = 0,
    Wooden = 1,
    Stone = 2,
    Iron = 3,
}

impl AxeTier {
    pub const fn encode(self) -> [u8; PLAYER_AXE_LEN] {
        [self as u8]
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        match encoded {
            [0] => Ok(Self::None),
            [1] => Ok(Self::Wooden),
            [2] => Ok(Self::Stone),
            [3] => Ok(Self::Iron),
            _ => Err(anyhow!("invalid player axe packet")),
        }
    }

    pub const fn bonus_basis_points(self) -> u64 {
        match self {
            Self::None => 0,
            Self::Wooden => WOODEN_AXE_BONUS_BASIS_POINTS,
            Self::Stone => STONE_AXE_BONUS_BASIS_POINTS,
            Self::Iron => IRON_AXE_BONUS_BASIS_POINTS,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::None => "No Axe",
            Self::Wooden => "Wooden Axe",
            Self::Stone => "Stone Axe",
            Self::Iron => "Iron Axe",
        }
    }

    pub const fn next_recipe(self) -> Option<AxeRecipe> {
        match self {
            Self::None => Some(AXE_RECIPES[0]),
            Self::Wooden => Some(AXE_RECIPES[1]),
            Self::Stone => Some(AXE_RECIPES[2]),
            Self::Iron => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AxeRecipe {
    pub axe: AxeTier,
    pub required_level: u64,
    pub log_cost: u64,
    pub stone_cost: u64,
    pub iron_ore_cost: u64,
}

impl AxeRecipe {
    pub const fn required_xp_balance(self) -> u64 {
        xp_balance_for_woodcutting_xp(XP_FOR_LEVEL[(self.required_level - 1) as usize])
    }
}

pub const AXE_RECIPES: [AxeRecipe; 3] = [
    AxeRecipe {
        axe: AxeTier::Wooden,
        required_level: 1,
        log_cost: 1,
        stone_cost: 0,
        iron_ore_cost: 0,
    },
    AxeRecipe {
        axe: AxeTier::Stone,
        required_level: 5,
        log_cost: 2,
        stone_cost: 2,
        iron_ore_cost: 0,
    },
    AxeRecipe {
        axe: AxeTier::Iron,
        required_level: 15,
        log_cost: 5,
        stone_cost: 0,
        iron_ore_cost: 2,
    },
];

/// Drop chance derives from soulbound XP and the permanent axe tier.
pub const fn log_drop_basis_points(player_xp_balance: u64, axe: AxeTier) -> u64 {
    let mut basis_points = BASE_LOG_DROP_BASIS_POINTS;
    let mut index = 0;
    while index < LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS.len() {
        if player_xp_balance >= LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS[index] {
            basis_points += LEVEL_LOG_DROP_BONUS_BASIS_POINTS;
        }
        index += 1;
    }
    if basis_points > MAX_LEVEL_LOG_DROP_BASIS_POINTS {
        basis_points = MAX_LEVEL_LOG_DROP_BASIS_POINTS;
    }
    basis_points + axe.bonus_basis_points()
}

/// Public deterministic entropy bound to one recursive player lineage.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct PlayerRoll([u8; PLAYER_ROLL_LEN]);

impl PlayerRoll {
    pub fn initial(player_script: &ScriptBuf) -> Result<Self> {
        if !player_script.is_p2tr() {
            return Err(anyhow!("player roll seed must be a P2TR script"));
        }
        let mut engine = sha256::Hash::engine();
        engine.input(PLAYER_ROLL_DOMAIN);
        engine.input(&player_script.as_bytes()[2..]);
        Ok(Self(sha256::Hash::from_engine(engine).to_byte_array()))
    }

    pub const fn encode(self) -> [u8; PLAYER_ROLL_LEN] {
        self.0
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        Ok(Self(
            encoded
                .try_into()
                .map_err(|_| anyhow!("invalid player roll packet"))?,
        ))
    }

    pub fn next(self) -> Self {
        Self(sha256::Hash::hash(&self.0).to_byte_array())
    }

    pub fn bucket(self) -> u64 {
        self.0.iter().rev().fold(0_u64, |value, byte| {
            (value * 256 + u64::from(*byte)) % CHOP_ROLL_BASIS_POINTS
        })
    }

    /// Independent crafting-material bucket derived from this next player roll.
    pub fn material_bucket(self) -> u64 {
        let mut engine = sha256::Hash::engine();
        engine.input(MATERIAL_ROLL_DOMAIN);
        engine.input(&self.0);
        sha256::Hash::from_engine(engine)
            .to_byte_array()
            .iter()
            .rev()
            .fold(0_u64, |value, byte| {
                (value * 256 + u64::from(*byte)) % CHOP_ROLL_BASIS_POINTS
            })
    }
}

#[derive(
    Clone, Copy, Debug, Default, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "camelCase")]
pub enum MaterialDrop {
    #[default]
    None,
    Stone,
    IronOre,
}

/// A material can accompany a successful LOG. Buckets are disjoint, so one
/// chop can never find both materials.
pub fn material_drop(next_roll: PlayerRoll, player_xp_balance: u64, success: bool) -> MaterialDrop {
    if !success {
        return MaterialDrop::None;
    }
    let bucket = next_roll.material_bucket();
    if player_xp_balance >= IRON_ORE_UNLOCK_XP_BALANCE {
        if bucket < IRON_ORE_DROP_BASIS_POINTS {
            MaterialDrop::IronOre
        } else if bucket < IRON_ORE_DROP_BASIS_POINTS + STONE_DROP_BASIS_POINTS {
            MaterialDrop::Stone
        } else {
            MaterialDrop::None
        }
    } else if bucket < STONE_DROP_BASIS_POINTS {
        MaterialDrop::Stone
    } else {
        MaterialDrop::None
    }
}

/// Reward credit centered on a one-drop luck corridor.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlayerLuckCredit(u64);

impl PlayerLuckCredit {
    pub fn new(value: u64) -> Result<Self> {
        if value > MAX_LUCK_CREDIT {
            return Err(anyhow!("player luck credit exceeds {MAX_LUCK_CREDIT}"));
        }
        Ok(Self(value))
    }

    pub const fn initial() -> Self {
        Self(INITIAL_LUCK_CREDIT)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub fn encode(self) -> [u8; PLAYER_LUCK_CREDIT_LEN] {
        let mut encoded = [0_u8; PLAYER_LUCK_CREDIT_LEN];
        encoded[..8].copy_from_slice(&self.0.to_le_bytes());
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() != PLAYER_LUCK_CREDIT_LEN || encoded[8] != 0 {
            return Err(anyhow!("invalid player luck credit packet"));
        }
        Self::new(u64::from_le_bytes(
            encoded[..8].try_into().expect("fixed player luck credit"),
        ))
    }
}

impl serde::Serialize for PlayerLuckCredit {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PlayerLuckCredit {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerLuck {
    pub roll: PlayerRoll,
    pub credit: PlayerLuckCredit,
}

impl PlayerLuck {
    pub fn initial(player_script: &ScriptBuf) -> Result<Self> {
        Ok(Self {
            roll: PlayerRoll::initial(player_script)?,
            credit: PlayerLuckCredit::initial(),
        })
    }

    pub fn advance(self, player_xp_balance: u64, axe: AxeTier) -> (Self, bool) {
        let next_roll = self.roll.next();
        let drop_basis_points = log_drop_basis_points(player_xp_balance, axe);
        let raw_credit = self.credit.value() + drop_basis_points;
        let candidate = next_roll.bucket() < drop_basis_points;
        let success = if raw_credit > MAX_LUCK_CREDIT {
            true
        } else if raw_credit < CHOP_ROLL_BASIS_POINTS {
            false
        } else {
            candidate
        };
        let next_credit = raw_credit - u64::from(success) * CHOP_ROLL_BASIS_POINTS;
        debug_assert!(next_credit <= MAX_LUCK_CREDIT);
        (
            Self {
                roll: next_roll,
                credit: PlayerLuckCredit(next_credit),
            },
            success,
        )
    }
}

/// Level is derived from user-facing Woodcutting XP, never committed as a
/// second mutable state value. XP can continue increasing at level 99 without
/// changing the displayed level.
pub fn level_from_xp(woodcutting_xp: u64) -> u64 {
    XP_FOR_LEVEL.partition_point(|threshold| *threshold <= woodcutting_xp) as u64
}

pub fn xp_for_level(level: u64) -> Option<u64> {
    let index = usize::try_from(level.checked_sub(1)?).ok()?;
    XP_FOR_LEVEL.get(index).copied()
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    pub luck: PlayerLuck,
    pub axe: AxeTier,
}

/// Host-side mirror of one canonical atomic chop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerChopTransition {
    pub previous_state: PlayerState,
    pub next_state: PlayerState,
    pub previous_tree_state: crate::tree::TreeState,
    pub next_tree_state: crate::tree::TreeState,
    pub previous_tree_health: crate::tree::TreeHealth,
    pub next_tree_health: crate::tree::TreeHealth,
    pub success: bool,
    pub state_logs_before: u64,
    pub state_logs_after: u64,
    pub state_xp_balance_before: u64,
    pub state_xp_balance_after: u64,
    pub state_stone_before: u64,
    pub state_stone_after: u64,
    pub state_iron_ore_before: u64,
    pub state_iron_ore_after: u64,
    pub tree_markers_before: u64,
    pub tree_markers_after: u64,
    pub tree_logs_before: u64,
    pub tree_logs_after: u64,
    pub tree_xp_balance_before: u64,
    pub tree_xp_balance_after: u64,
    pub tree_stone_before: u64,
    pub tree_stone_after: u64,
    pub tree_iron_ore_before: u64,
    pub tree_iron_ore_after: u64,
    pub state_value_before: u64,
    pub state_value_after: u64,
    pub tree_value_before: u64,
    pub tree_value_after: u64,
    pub dust_sats: u64,
}

impl PlayerChopTransition {
    pub fn validate(self) -> Result<()> {
        if self.next_state.axe != self.previous_state.axe {
            return Err(anyhow!("axe tier changed during chop"));
        }
        let (expected_luck, expected_success) = self
            .previous_state
            .luck
            .advance(self.state_xp_balance_before, self.previous_state.axe);
        if self.next_state.luck != expected_luck || self.success != expected_success {
            return Err(anyhow!("player chop luck transition is invalid"));
        }
        let reward = u64::from(self.success);
        let material = material_drop(
            expected_luck.roll,
            self.state_xp_balance_before,
            self.success,
        );
        let stone_reward = u64::from(material == MaterialDrop::Stone);
        let iron_ore_reward = u64::from(material == MaterialDrop::IronOre);
        if self.state_xp_balance_after
            != self
                .state_xp_balance_before
                .checked_add(reward)
                .ok_or_else(|| anyhow!("player XP asset overflow"))?
        {
            return Err(anyhow!(
                "player XP asset balance must advance by exactly the reward bit"
            ));
        }
        if self.state_logs_after
            != self
                .state_logs_before
                .checked_add(reward)
                .ok_or_else(|| anyhow!("player LOG balance overflow"))?
        {
            return Err(anyhow!("player LOG balance does not match the chop roll"));
        }
        if self.state_stone_after
            != self
                .state_stone_before
                .checked_add(stone_reward)
                .ok_or_else(|| anyhow!("player STONE balance overflow"))?
            || self.state_iron_ore_after
                != self
                    .state_iron_ore_before
                    .checked_add(iron_ore_reward)
                    .ok_or_else(|| anyhow!("player IRON ORE balance overflow"))?
        {
            return Err(anyhow!(
                "player material balances do not match the chop roll"
            ));
        }
        if self.previous_tree_state != self.next_tree_state {
            return Err(anyhow!("tree identity or position changed during chop"));
        }
        if self.tree_markers_before != 1 || self.tree_markers_after != 1 {
            return Err(anyhow!("player chop must preserve one TREE marker"));
        }
        if self.previous_tree_health.value() == 0
            || self.tree_logs_before == 0
            || self.tree_xp_balance_before == 0
        {
            return Err(anyhow!("cannot chop a depleted tree"));
        }
        if self.next_tree_health.value()
            != self
                .previous_tree_health
                .value()
                .checked_sub(reward)
                .ok_or_else(|| anyhow!("tree health underflow"))?
            || self.tree_logs_after
                != self
                    .tree_logs_before
                    .checked_sub(reward)
                    .ok_or_else(|| anyhow!("tree LOG balance underflow"))?
            || self.tree_xp_balance_after
                != self
                    .tree_xp_balance_before
                    .checked_sub(reward)
                    .ok_or_else(|| anyhow!("tree XP balance underflow"))?
            || self.tree_stone_after
                != self
                    .tree_stone_before
                    .checked_sub(stone_reward)
                    .ok_or_else(|| anyhow!("tree STONE balance underflow"))?
            || self.tree_iron_ore_after
                != self
                    .tree_iron_ore_before
                    .checked_sub(iron_ore_reward)
                    .ok_or_else(|| anyhow!("tree IRON ORE balance underflow"))?
        {
            return Err(anyhow!("tree inventory does not match the chop roll"));
        }
        if self.state_value_before != self.dust_sats || self.state_value_after != self.dust_sats {
            return Err(anyhow!("player state must preserve one dust value"));
        }
        let expected_tree_value = crate::tree::tree_value_sats(self.dust_sats);
        if self.tree_value_before != expected_tree_value
            || self.tree_value_after != expected_tree_value
        {
            return Err(anyhow!("tree sats changed during chop"));
        }
        Ok(())
    }
}

/// Complete material needed to fund and spend one personalized player VTXO.
#[derive(Clone, Debug)]
pub struct PlayerContract {
    pub vtxo: ark_core::Vtxo,
    pub chop_spend_script: ScriptBuf,
    pub chop_arkade_script: ScriptBuf,
    /// Owner-authorized exact self-send into a fresh Ark batch.
    pub renewal_spend_script: ScriptBuf,
    /// Optional unattended renewal path for the low-authority watchtower.
    pub watchtower_renewal_spend_script: ScriptBuf,
    pub renewal_arkade_script: ScriptBuf,
    /// Owner-authorized LOG withdrawal; progression and materials stay soulbound.
    pub withdraw_spend_script: ScriptBuf,
    pub withdraw_arkade_script: ScriptBuf,
    /// Owner-authorized exact recipe burn advancing the permanent axe tier.
    pub craft_spend_script: ScriptBuf,
    pub craft_arkade_script: ScriptBuf,
    pub owner: XOnlyPublicKey,
    pub operator: XOnlyPublicKey,
    pub emulator: XOnlyPublicKey,
    pub chop_tweaked_emulator: XOnlyPublicKey,
    pub log_asset: AssetId,
    pub xp_asset: AssetId,
    pub craft_tweaked_emulator: XOnlyPublicKey,
    pub stone_asset: AssetId,
    pub iron_ore_asset: AssetId,
    pub dust_sats: u64,
    pub tree_script_pubkey: ScriptBuf,
}

/// Build a personalized recursive player contract. Activation supplies one
/// unique uncontrolled PLAYER_ID asset, which the covenant preserves
/// dynamically without committing its post-transaction AssetId into P2TR.
///
/// Tapleaf authority is deliberately split:
/// - watchtower renewal: rollover + operator + covenant-tweaked emulator;
/// - LOG withdrawal: owner + operator + covenant-tweaked emulator;
/// - axe crafting: owner + operator + covenant-tweaked emulator.
///
/// The required Arkade CSV exit is keyed to the NUMS point, so it satisfies
/// expiry accounting without creating a unilateral path around recursion.
#[allow(clippy::too_many_arguments)]
pub fn build_player_contract<C: Verification>(
    secp: &Secp256k1<C>,
    owner_pk: XOnlyPublicKey,
    operator_pk: XOnlyPublicKey,
    emulator_pk: XOnlyPublicKey,
    rollover_pk: XOnlyPublicKey,
    exit_delay: Sequence,
    network: Network,
    tree_asset: AssetId,
    log_asset: AssetId,
    xp_asset: AssetId,
    stone_asset: AssetId,
    iron_ore_asset: AssetId,
    dust_sats: u64,
    tree_script: &ScriptBuf,
) -> Result<PlayerContract> {
    if [owner_pk, operator_pk, emulator_pk, rollover_pk]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
        != 4
    {
        return Err(anyhow!("player contract signers must be distinct"));
    }
    if [tree_asset, log_asset, xp_asset, stone_asset, iron_ore_asset]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
        != 5
    {
        return Err(anyhow!(
            "TREE, LOG, XP, STONE, and IRON ORE assets must differ"
        ));
    }
    let chop_arkade_script = player_chop_covenant_script(
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
        dust_sats,
        tree_script,
    )?;
    let renewal_arkade_script = player_renewal_covenant_script(
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
        dust_sats,
    )?;
    let withdraw_arkade_script = player_withdraw_covenant_script(
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
        dust_sats,
    )?;
    let craft_arkade_script =
        player_craft_covenant_script(log_asset, xp_asset, stone_asset, iron_ore_asset, dust_sats)?;
    let chop_tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, &chop_arkade_script)
            .context("derive player chop emulator signer")?;
    let renewal_tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, &renewal_arkade_script)
            .context("derive player renewal emulator signer")?;
    let withdraw_tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, &withdraw_arkade_script)
            .context("derive player withdraw emulator signer")?;
    let craft_tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, &craft_arkade_script)
            .context("derive player craft emulator signer")?;
    let nums: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY
        .parse()
        .context("parse Arkade NUMS key")?;
    let exit_owner = nums.inner.x_only_public_key().0;
    // Each covenant leaf's emulator position is the script-tweaked key
    // emulator + H("ArkScriptHash", script)·G. A tweaked key equal to a plain
    // signer of the same leaf lets that signer take the emulator path without
    // covenant execution; equal to the NUMS exit owner the leaf is dead.
    for tweaked_emulator in [
        chop_tweaked_emulator,
        renewal_tweaked_emulator,
        withdraw_tweaked_emulator,
        craft_tweaked_emulator,
    ] {
        if [owner_pk, operator_pk, rollover_pk].contains(&tweaked_emulator) {
            return Err(anyhow!("tweaked emulator collides with a player signer"));
        }
        if tweaked_emulator == exit_owner {
            return Err(anyhow!("tweaked emulator collides with the exit key"));
        }
    }
    let processed = ArkadeVtxoScript::new(vec![
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script: chop_arkade_script.clone(),
            tapscript: ArkadeTapscript::Multisig {
                pubkeys: vec![owner_pk, operator_pk],
            },
            introspectors: vec![emulator_pk],
        }),
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script: renewal_arkade_script.clone(),
            tapscript: ArkadeTapscript::Multisig {
                pubkeys: vec![owner_pk, operator_pk],
            },
            introspectors: vec![emulator_pk],
        }),
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script: renewal_arkade_script.clone(),
            tapscript: ArkadeTapscript::Multisig {
                pubkeys: vec![operator_pk, rollover_pk],
            },
            introspectors: vec![emulator_pk],
        }),
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script: withdraw_arkade_script.clone(),
            tapscript: ArkadeTapscript::Multisig {
                pubkeys: vec![owner_pk, operator_pk],
            },
            introspectors: vec![emulator_pk],
        }),
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script: craft_arkade_script.clone(),
            tapscript: ArkadeTapscript::Multisig {
                pubkeys: vec![owner_pk, operator_pk],
            },
            introspectors: vec![emulator_pk],
        }),
    ])
    .context("build player Arkade tapleaves")?;
    let [chop_spend_script, renewal_spend_script, watchtower_renewal_spend_script, withdraw_spend_script, craft_spend_script] =
        processed.scripts.as_slice()
    else {
        return Err(anyhow!(
            "player contract must have chop, renewal, withdraw, and craft leaves"
        ));
    };
    let chop_spend_script = chop_spend_script.clone();
    let renewal_spend_script = renewal_spend_script.clone();
    let watchtower_renewal_spend_script = watchtower_renewal_spend_script.clone();
    let withdraw_spend_script = withdraw_spend_script.clone();
    let craft_spend_script = craft_spend_script.clone();
    let scripts = processed
        .scripts
        .into_iter()
        .chain([ark_core::script::csv_sig_script(exit_delay, exit_owner)])
        .collect();
    let vtxo = ark_core::Vtxo::new_with_custom_scripts(
        secp,
        operator_pk,
        owner_pk,
        scripts,
        exit_delay,
        network,
    )
    .map_err(|error| anyhow!("build player VTXO: {error}"))?;

    Ok(PlayerContract {
        vtxo,
        chop_spend_script,
        chop_arkade_script,
        renewal_spend_script,
        watchtower_renewal_spend_script,
        renewal_arkade_script,
        withdraw_spend_script,
        withdraw_arkade_script,
        craft_spend_script,
        craft_arkade_script,
        owner: owner_pk,
        operator: operator_pk,
        emulator: emulator_pk,
        chop_tweaked_emulator,
        log_asset,
        xp_asset,
        stone_asset,
        craft_tweaked_emulator,
        iron_ore_asset,
        dust_sats,
        tree_script_pubkey: tree_script.clone(),
    })
}
/// Covenant-enforced axe crafting. Exact ingredients are burned while every
/// other player resource and mutable packet stays in the recursive state.
pub fn player_craft_covenant_script(
    log_asset: AssetId,
    xp_asset: AssetId,
    stone_asset: AssetId,
    iron_ore_asset: AssetId,
    dust_sats: u64,
) -> Result<ScriptBuf> {
    if [log_asset, xp_asset, stone_asset, iron_ore_asset]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
        != 4
    {
        return Err(anyhow!("player inventory asset IDs must differ"));
    }
    let dust = crate::tree::script_int(dust_sats, "player dust")?;
    let anchor_program =
        crate::tree::witness_v1_program(&ark_core::anchor_output().script_pubkey, "Arkade anchor")?;
    let builder = Builder::new()
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(crate::protocol::CRAFT_STATE_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTVERSION)
        .push_int(crate::protocol::CRAFT_TRANSACTION_VERSION)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(crate::protocol::CRAFT_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(crate::protocol::CRAFT_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY);
    let builder = crate::tree::push_equal_input_output_scripts(
        builder,
        crate::protocol::CRAFT_STATE_INPUT_INDEX,
        crate::protocol::CRAFT_STATE_OUTPUT_INDEX,
    )
    .push_int(i64::from(crate::protocol::CRAFT_STATE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(dust)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(crate::protocol::CRAFT_STATE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_int(dust)
    .push_opcode(OP_EQUALVERIFY);
    let builder = crate::tree::push_extension_and_anchor_shape(
        builder,
        crate::protocol::CRAFT_EXTENSION_OUTPUT_INDEX,
        crate::protocol::CRAFT_ANCHOR_OUTPUT_INDEX,
        &anchor_program,
    )
    .push_int(i64::from(crate::protocol::CRAFT_EXTENSION_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(crate::protocol::CRAFT_ANCHOR_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY);
    let builder =
        push_canonical_initial_player_luck(builder, crate::protocol::CRAFT_STATE_INPUT_INDEX);
    let builder = crate::tree::push_equal_state_packet_at(
        builder,
        PLAYER_ROLL_PACKET_TYPE,
        crate::protocol::CRAFT_STATE_INPUT_INDEX,
    );
    let builder = crate::tree::push_equal_state_packet_at(
        builder,
        PLAYER_LUCK_CREDIT_PACKET_TYPE,
        crate::protocol::CRAFT_STATE_INPUT_INDEX,
    );
    let builder = crate::tree::push_player_marker_group(
        builder,
        crate::protocol::CRAFT_STATE_INPUT_INDEX,
        crate::protocol::CRAFT_STATE_OUTPUT_INDEX,
    );
    let builder = crate::tree::push_player_renewal_group_set(
        builder,
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
    );
    let builder = crate::tree::push_canonical_player_asset_counts(
        builder,
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
    );
    let builder = push_exact_state_asset_burn(builder, xp_asset, 0);

    // Validate the one-tier transition, retaining the input tier for the
    // recipe branch.
    let builder = push_player_axe_value(builder, Some(crate::protocol::CRAFT_STATE_INPUT_INDEX))
        .push_opcode(OP_DUP)
        .push_int(AxeTier::Iron as i64)
        .push_opcode(OP_LESSTHAN)
        .push_opcode(OP_VERIFY)
        .push_opcode(OP_DUP)
        .push_int(1)
        .push_opcode(OP_ADD);
    let builder = push_player_axe_value(builder, None).push_opcode(OP_EQUALVERIFY);

    let builder = builder
        .push_opcode(OP_DUP)
        .push_int(AxeTier::None as i64)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP);
    let builder = push_exact_state_asset_burn(builder, log_asset, AXE_RECIPES[0].log_cost);
    let builder = push_exact_state_asset_burn(builder, stone_asset, 0);
    let builder = push_exact_state_asset_burn(builder, iron_ore_asset, 0)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DUP)
        .push_int(AxeTier::Wooden as i64)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP);
    let builder = push_required_xp_balance(builder, xp_asset, AXE_RECIPES[1].required_xp_balance());
    let builder = push_exact_state_asset_burn(builder, log_asset, AXE_RECIPES[1].log_cost);
    let builder = push_exact_state_asset_burn(builder, stone_asset, AXE_RECIPES[1].stone_cost);
    let builder = push_exact_state_asset_burn(builder, iron_ore_asset, 0)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DROP);
    let builder = push_required_xp_balance(builder, xp_asset, AXE_RECIPES[2].required_xp_balance());
    let builder = push_exact_state_asset_burn(builder, log_asset, AXE_RECIPES[2].log_cost);
    let builder = push_exact_state_asset_burn(builder, stone_asset, 0);
    Ok(
        push_exact_state_asset_burn(builder, iron_ore_asset, AXE_RECIPES[2].iron_ore_cost)
            .push_opcode(OP_ENDIF)
            .push_opcode(OP_ENDIF)
            .push_int(1)
            .into_script(),
    )
}

fn push_exact_state_asset_burn(builder: Builder, asset: AssetId, amount: u64) -> Builder {
    let builder = crate::tree::push_optional_input_asset_lookup(
        builder,
        crate::protocol::CRAFT_STATE_INPUT_INDEX,
        asset,
    )
    .push_opcode(OP_DROP);
    crate::tree::push_optional_output_asset_lookup(
        builder,
        crate::protocol::CRAFT_STATE_OUTPUT_INDEX,
        asset,
    )
    .push_opcode(OP_DROP)
    .push_int(amount as i64)
    .push_opcode(OP_ADD)
    .push_opcode(OP_EQUALVERIFY)
}

fn push_required_xp_balance(builder: Builder, xp_asset: AssetId, required: u64) -> Builder {
    crate::tree::push_optional_input_asset_lookup(
        builder,
        crate::protocol::CRAFT_STATE_INPUT_INDEX,
        xp_asset,
    )
    .push_opcode(OP_DROP)
    // Asset amounts are integers: `balance > required - 1` is the exact
    // inclusive level gate.
    .push_int(required.saturating_sub(1) as i64)
    .push_opcode(OP_GREATERTHAN)
    .push_opcode(OP_VERIFY)
}

/// Covenant for both player batch-renewal paths. It permits only an exact
/// self-send preserving P2TR, value, every player packet, PLAYER_ID, and all
/// inventory assets.
pub fn player_renewal_covenant_script(
    log_asset: AssetId,
    xp_asset: AssetId,
    stone_asset: AssetId,
    iron_ore_asset: AssetId,
    dust_sats: u64,
) -> Result<ScriptBuf> {
    let dust = i64::try_from(dust_sats)
        .map_err(|_| anyhow!("player dust exceeds script integer range"))?;
    let builder = crate::tree::push_renewal_shape(Builder::new())?;
    let builder = crate::tree::push_equal_input_output_scripts(
        builder,
        RENEWAL_STATE_INPUT_INDEX,
        RENEWAL_STATE_OUTPUT_INDEX,
    )
    .push_int(i64::from(RENEWAL_STATE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(dust)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(RENEWAL_STATE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_int(dust)
    .push_opcode(OP_EQUALVERIFY);
    let builder = push_canonical_initial_player_luck(builder, RENEWAL_STATE_INPUT_INDEX);
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_ROLL_PACKET_TYPE);
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_LUCK_CREDIT_PACKET_TYPE);
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_AXE_PACKET_TYPE);
    let builder = crate::tree::push_renewal_asset_shell(builder)?;
    let builder = crate::tree::push_player_marker_group(
        builder,
        RENEWAL_STATE_INPUT_INDEX,
        RENEWAL_STATE_OUTPUT_INDEX,
    );
    let builder = crate::tree::push_player_renewal_group_set(
        builder,
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
    );

    let mut builder = builder;
    for asset in [log_asset, xp_asset, stone_asset, iron_ore_asset] {
        builder = crate::tree::push_optional_input_asset_lookup(
            builder,
            RENEWAL_STATE_INPUT_INDEX,
            asset,
        );
        builder = crate::tree::push_optional_output_asset_lookup(
            builder,
            RENEWAL_STATE_OUTPUT_INDEX,
            asset,
        )
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY);
    }
    Ok(builder.push_int(1).into_script())
}

/// Covenant for owner-authorized LOG withdrawal. Only LOG may leave: player
/// P2TR, dust, PLAYER_ID, all packets, XP, STONE, and IRON ORE stay exact.
///
/// Canonical shape:
///
/// ```text
/// vin 0 player state | vin 1 wallet funding
/// vout 0 player state | vout 1 LOG destination | vout 2 extension | vout 3 anchor
/// groups PLAYER_ID | LOG | XP | STONE | IRON ORE
/// ```
pub fn player_withdraw_covenant_script(
    log_asset: AssetId,
    xp_asset: AssetId,
    stone_asset: AssetId,
    iron_ore_asset: AssetId,
    dust_sats: u64,
) -> Result<ScriptBuf> {
    if [log_asset, xp_asset, stone_asset, iron_ore_asset]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
        != 4
    {
        return Err(anyhow!("player inventory asset IDs must differ"));
    }
    let dust = i64::try_from(dust_sats)
        .map_err(|_| anyhow!("player dust exceeds script integer range"))?;
    let anchor_program =
        crate::tree::witness_v1_program(&ark_core::anchor_output().script_pubkey, "Arkade anchor")?;
    let builder = Builder::new()
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(crate::protocol::WITHDRAW_STATE_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(crate::protocol::WITHDRAW_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(crate::protocol::WITHDRAW_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY);
    // The state keeps its exact P2TR and dust; the destination is funded
    // entirely by the wallet input with zero fee skimmed from state.
    let builder = crate::tree::push_equal_input_output_scripts(
        builder,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
        crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX,
    )
    .push_int(i64::from(crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(dust)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(crate::protocol::WITHDRAW_STATE_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_int(dust)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(
        crate::protocol::WITHDRAW_DESTINATION_OUTPUT_INDEX,
    ))
    .push_opcode(op::INSPECTOUTPUTVALUE)
    .push_int(crate::protocol::WITHDRAW_FUNDING_INPUT_INDEX as i64)
    .push_opcode(op::INSPECTINPUTVALUE)
    .push_opcode(OP_EQUALVERIFY);
    let builder =
        push_canonical_initial_player_luck(builder, crate::protocol::WITHDRAW_STATE_INPUT_INDEX);
    // Roll and luck credit are the complete mutable player packet state.
    let builder = crate::tree::push_equal_state_packet_at(
        builder,
        PLAYER_ROLL_PACKET_TYPE,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
    );
    let builder = crate::tree::push_equal_state_packet_at(
        builder,
        PLAYER_LUCK_CREDIT_PACKET_TYPE,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
    );
    let builder = crate::tree::push_equal_state_packet_at(
        builder,
        PLAYER_AXE_PACKET_TYPE,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
    );
    let builder = crate::tree::push_extension_and_anchor_shape(
        builder,
        crate::protocol::WITHDRAW_EXTENSION_OUTPUT_INDEX,
        crate::protocol::WITHDRAW_ANCHOR_OUTPUT_INDEX,
        &anchor_program,
    )
    .push_int(i64::from(crate::protocol::WITHDRAW_EXTENSION_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY)
    .push_int(i64::from(crate::protocol::WITHDRAW_ANCHOR_OUTPUT_INDEX))
    .push_opcode(op::INSPECTOUTASSETCOUNT)
    .push_int(0)
    .push_opcode(OP_EQUALVERIFY);
    let builder = crate::tree::push_player_marker_group(
        builder,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
        crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX,
    );
    let builder = crate::tree::push_player_renewal_group_set(
        builder,
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
    );

    // LOG conservation: state-in minus state-out is exactly the destination.
    let builder = crate::tree::push_input_asset_lookup(
        builder,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
        log_asset,
    )
    .push_int(1)
    .push_opcode(OP_EQUALVERIFY);
    let builder = crate::tree::push_optional_output_asset_lookup(
        builder,
        crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX,
        log_asset,
    )
    .push_opcode(OP_DROP)
    .push_opcode(OP_SUB);
    let builder = crate::tree::push_optional_output_asset_lookup(
        builder,
        crate::protocol::WITHDRAW_DESTINATION_OUTPUT_INDEX,
        log_asset,
    )
    .push_opcode(OP_DROP)
    .push_opcode(OP_EQUALVERIFY);
    // XP and materials are soulbound: presence and amount remain exact.
    let mut builder = builder;
    for asset in [xp_asset, stone_asset, iron_ore_asset] {
        builder = crate::tree::push_optional_input_asset_lookup(
            builder,
            crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
            asset,
        );
        builder = crate::tree::push_optional_output_asset_lookup(
            builder,
            crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX,
            asset,
        )
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY);
    }
    Ok(builder.push_int(1).into_script())
}

/// Convert a canonical indexed player-state VTXO into its chop spend path.
pub fn player_state_vtxo_input(
    record: &crate::arkade::VtxoRecord,
    contract: &PlayerContract,
    player_asset: AssetId,
) -> Result<VtxoInput> {
    let expected_script = contract.vtxo.script_pubkey();
    let assets = canonical_player_state_assets(record, contract, player_asset)?;
    let control_block = contract
        .vtxo
        .get_spend_info(contract.chop_spend_script.clone())
        .map_err(|error| anyhow!("player covenant spend info: {error}"))?;
    Ok(VtxoInput::new(
        contract.chop_spend_script.clone(),
        None,
        control_block,
        contract.vtxo.tapscripts(),
        expected_script,
        Amount::from_sat(record.amount_sats),
        record.outpoint,
        assets.to_vec(),
    ))
}

pub(crate) fn validate_player_state_record(
    record: &crate::arkade::VtxoRecord,
    contract: &PlayerContract,
    player_asset: AssetId,
) -> Result<()> {
    canonical_player_state_assets(record, contract, player_asset).map(|_| ())
}

fn canonical_player_state_assets<'a>(
    record: &'a crate::arkade::VtxoRecord,
    contract: &PlayerContract,
    player_asset: AssetId,
) -> Result<&'a [Asset]> {
    let marker_count = record.asset_amount(player_asset);
    let marker_entries = record
        .assets
        .iter()
        .filter(|asset| asset.asset_id == player_asset)
        .count();
    let canonical_ids = [
        contract.log_asset,
        contract.xp_asset,
        contract.stone_asset,
        contract.iron_ore_asset,
    ];
    let canonical_assets = (1..=5).contains(&record.assets.len())
        && marker_entries == 1
        && canonical_ids.iter().all(|asset_id| {
            record
                .assets
                .iter()
                .filter(|asset| asset.asset_id == *asset_id)
                .count()
                <= 1
        })
        && record
            .assets
            .iter()
            .all(|asset| asset.asset_id == player_asset || canonical_ids.contains(&asset.asset_id));
    if record.amount_sats != contract.dust_sats
        || record.script != contract.vtxo.script_pubkey()
        || marker_count != Some(1)
        || !canonical_assets
    {
        return Err(anyhow!(
            "indexed player state has invalid value, script, PLAYER_ID, or inventory"
        ));
    }
    Ok(record.assets.as_slice())
}

/// Build the personalized half of the atomic PLAYER/TREE chop. The shared tree
/// covenant owns every reciprocal resource delta and the player-luck
/// transition; this half pins that tree and preserves owner continuity.
pub fn player_chop_covenant_script(
    log_asset: AssetId,
    xp_asset: AssetId,
    stone_asset: AssetId,
    iron_ore_asset: AssetId,
    dust_sats: u64,
    tree_script: &ScriptBuf,
) -> Result<ScriptBuf> {
    if [log_asset, xp_asset, stone_asset, iron_ore_asset]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
        != 4
    {
        return Err(anyhow!("player inventory asset IDs must differ"));
    }
    if dust_sats == 0 {
        return Err(anyhow!("player chop dust must be non-zero"));
    }
    if !tree_script.is_p2tr() {
        return Err(anyhow!("tree script must be P2TR"));
    }
    let dust = i64::try_from(dust_sats)
        .map_err(|_| anyhow!("player dust exceeds script integer range"))?;
    let tree_program = PushBytesBuf::try_from(tree_script.as_bytes()[2..].to_vec())
        .expect("validated witness program is a bounded push");
    let tree_value = i64::try_from(crate::tree::tree_value_sats(dust_sats))
        .map_err(|_| anyhow!("tree value exceeds script integer range"))?;
    let anchor_program =
        PushBytesBuf::try_from(P2A_PROGRAM.to_vec()).expect("P2A is a bounded push");
    let builder =
        crate::tree::push_chop_shape(Builder::new(), PLAYER_STATE_INPUT_INDEX, &anchor_program)?
            // Recursively preserve the owner-specific player P2TR and its 330 sats.
            .push_int(i64::from(PLAYER_STATE_OUTPUT_INDEX))
            .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(PLAYER_STATE_INPUT_INDEX as i64)
            .push_opcode(op::INSPECTINPUTSCRIPTPUBKEY)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(i64::from(PLAYER_STATE_OUTPUT_INDEX))
            .push_opcode(op::INSPECTOUTPUTVALUE)
            .push_int(dust)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(PLAYER_STATE_INPUT_INDEX as i64)
            .push_opcode(op::INSPECTINPUTVALUE)
            .push_int(dust)
            .push_opcode(OP_EQUALVERIFY)
            // Pin the reciprocal shared TREE input and output.
            .push_int(TREE_INPUT_INDEX as i64)
            .push_opcode(op::INSPECTINPUTSCRIPTPUBKEY)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_slice(tree_program.clone())
            .push_opcode(OP_EQUALVERIFY)
            .push_int(i64::from(TREE_OUTPUT_INDEX))
            .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
            .push_int(1)
            .push_opcode(OP_EQUALVERIFY)
            .push_slice(tree_program)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(TREE_INPUT_INDEX as i64)
            .push_opcode(op::INSPECTINPUTVALUE)
            .push_int(tree_value)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(i64::from(TREE_OUTPUT_INDEX))
            .push_opcode(op::INSPECTOUTPUTVALUE)
            .push_int(tree_value)
            .push_opcode(OP_EQUALVERIFY)
            .push_int(TREE_INPUT_INDEX as i64)
            .push_opcode(op::INSPECTINPUTARKADESCRIPTHASH)
            .push_opcode(OP_DROP);
    let builder = push_canonical_initial_player_luck(builder, PLAYER_STATE_INPUT_INDEX);
    let builder =
        push_advanced_player_luck(builder, PLAYER_STATE_INPUT_INDEX, xp_asset).push_opcode(OP_DROP);
    let builder = crate::tree::push_player_marker_group(
        builder,
        PLAYER_STATE_INPUT_INDEX,
        PLAYER_STATE_OUTPUT_INDEX,
    );
    let builder = crate::tree::push_transfer_group_shell(builder, log_asset);
    let builder = crate::tree::push_transfer_group_shell(builder, xp_asset);
    let builder = crate::tree::push_transfer_group_shell(builder, stone_asset);
    let builder = crate::tree::push_transfer_group_shell(builder, iron_ore_asset);
    let builder = crate::tree::push_canonical_player_asset_counts(
        builder,
        log_asset,
        xp_asset,
        stone_asset,
        iron_ore_asset,
    );
    Ok(builder.push_int(1).into_script())
}

/// Attach the complete mutable player packet state.
pub fn attach_player_state_packets(psbt: &mut Psbt, state: PlayerState) -> Result<()> {
    let mut updated = psbt.clone();
    ark_core::extension::add_packet_to_psbt(
        &mut updated,
        PLAYER_ROLL_PACKET_TYPE,
        &state.luck.roll.encode(),
    )
    .context("attach player roll packet")?;
    ark_core::extension::add_packet_to_psbt(
        &mut updated,
        PLAYER_LUCK_CREDIT_PACKET_TYPE,
        &state.luck.credit.encode(),
    )
    .context("attach player luck credit packet")?;
    ark_core::extension::add_packet_to_psbt(
        &mut updated,
        PLAYER_AXE_PACKET_TYPE,
        &state.axe.encode(),
    )
    .context("attach player axe packet")?;
    *psbt = updated;
    Ok(())
}

/// Decode complete player state from a transaction. No player packets means
/// `None`; a partial set is rejected rather than silently defaulted.
pub fn player_state_from_tx(tx: &Transaction) -> Result<Option<PlayerState>> {
    let [roll, credit, axe] = player_packet_payloads(tx)?;
    match (roll, credit, axe) {
        (None, None, None) => Ok(None),
        (Some(roll), Some(credit), Some(axe)) => Ok(Some(PlayerState {
            luck: PlayerLuck {
                roll: PlayerRoll::decode(roll)?,
                credit: PlayerLuckCredit::decode(credit)?,
            },
            axe: AxeTier::decode(axe)?,
        })),
        _ => Err(anyhow!("transaction contains incomplete player state")),
    }
}

/// Attach previous transactions, next state packets, and both reciprocal
/// Arkade Script entries required by an atomic two-input chop.
pub fn attach_player_chop_context(
    psbt: &mut Psbt,
    checkpoints: &[Psbt],
    contract: &PlayerContract,
    tree_contract: &crate::tree::TreeContract,
    previous_input_txs: [&Transaction; 2],
    player_xp_before: u64,
    next_state: PlayerState,
) -> Result<()> {
    if psbt.unsigned_tx.input.len() != 2 {
        return Err(anyhow!("player chop requires exactly two inputs"));
    }
    let mut updated = psbt.clone();
    crate::txbuild::attach_previous_ark_transactions(
        &mut updated,
        checkpoints,
        previous_input_txs.iter().copied(),
    )?;
    let player_source_output = checkpoints[PLAYER_STATE_INPUT_INDEX].inputs[0]
        .witness_utxo
        .as_ref()
        .expect("previous transaction helper validated the player source");
    if player_source_output.value.to_sat() != contract.dust_sats
        || player_source_output.script_pubkey != contract.vtxo.script_pubkey()
    {
        return Err(anyhow!(
            "player source output does not match the personalized contract"
        ));
    }
    let tree_source_output = checkpoints[TREE_INPUT_INDEX].inputs[0]
        .witness_utxo
        .as_ref()
        .expect("previous transaction helper validated the tree source");
    if tree_source_output.value.to_sat() != crate::tree::tree_value_sats(contract.dust_sats)
        || tree_source_output.script_pubkey != tree_contract.vtxo.script_pubkey()
        || contract.tree_script_pubkey != tree_contract.vtxo.script_pubkey()
    {
        return Err(anyhow!(
            "tree source output does not match the world contract"
        ));
    }

    let previous_state = player_state_from_tx(previous_input_txs[PLAYER_STATE_INPUT_INDEX])?
        .ok_or_else(|| anyhow!("previous player transaction has no complete state"))?;
    let tree_state = crate::tree::tree_state_from_tx(previous_input_txs[TREE_INPUT_INDEX])?
        .ok_or_else(|| anyhow!("previous tree transaction has no tree state"))?;
    let previous_tree_health =
        crate::tree::tree_health_from_tx(previous_input_txs[TREE_INPUT_INDEX])?
            .ok_or_else(|| anyhow!("previous tree transaction has no tree health"))?;
    if previous_tree_health.value() == 0 {
        return Err(anyhow!("cannot chop a stump"));
    }
    let (expected_luck, success) = previous_state
        .luck
        .advance(player_xp_before, previous_state.axe);
    if next_state.luck != expected_luck || next_state.axe != previous_state.axe {
        return Err(anyhow!(
            "next player state does not match the canonical chop transition"
        ));
    }
    let next_health = crate::tree::TreeHealth::new(
        previous_tree_health
            .value()
            .checked_sub(u64::from(success))
            .ok_or_else(|| anyhow!("tree health underflow"))?,
    )?;
    attach_player_state_packets(&mut updated, next_state)?;
    crate::tree::attach_tree_state_packet(&mut updated, tree_state)?;
    crate::tree::attach_tree_health_packet(&mut updated, next_health)?;
    let packet = ark_core::introspector::packet::Packet::new(vec![
        ark_core::introspector::packet::IntrospectorEntry {
            vin: PLAYER_STATE_INPUT_INDEX as u16,
            script: contract.chop_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
        ark_core::introspector::packet::IntrospectorEntry {
            vin: TREE_INPUT_INDEX as u16,
            script: tree_contract.chop_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
    ])
    .context("build player chop emulator packet")?;
    ark_core::introspector::packet::add_packet_to_psbt(&mut updated, &packet)
        .context("attach player chop emulator packet")?;
    *psbt = updated;
    Ok(())
}

/// Verify the exact owner/operator/emulator signature matrix returned for a
/// player chop and its two checkpoints.
pub fn verify_player_chop_response(
    keys: &crate::Keys,
    contract: &PlayerContract,
    tree_contract: &crate::tree::TreeContract,
    expected_ark: &Psbt,
    expected_checkpoints: &[Psbt],
    returned_ark: &Psbt,
    returned_checkpoints: Vec<Psbt>,
) -> Result<()> {
    if keys.owner_pk() != contract.owner {
        return Err(anyhow!("player contract does not belong to these keys"));
    }
    let tweaked_emulator = ark_script::compute_arkade_script_public_key(
        &contract.emulator,
        &contract.chop_arkade_script,
    )
    .context("derive player chop emulator signer")?;
    if tweaked_emulator != contract.chop_tweaked_emulator {
        return Err(anyhow!(
            "player contract has an inconsistent emulator signer"
        ));
    }
    if expected_ark.unsigned_tx != returned_ark.unsigned_tx
        || expected_ark.unsigned_tx.input.len() != 2
        || expected_ark.inputs.len() != 2
        || returned_ark.inputs.len() != 2
    {
        return Err(anyhow!("emulator changed the submitted player chop"));
    }
    let player_signers = [
        (contract.owner, "owner"),
        (contract.operator, "operator"),
        (contract.chop_tweaked_emulator, "emulator"),
    ];
    let tree_emulator = ark_script::compute_arkade_script_public_key(
        &contract.emulator,
        &tree_contract.chop_arkade_script,
    )
    .context("derive tree chop emulator signer")?;
    let tree_signers = [(contract.operator, "operator"), (tree_emulator, "emulator")];
    verify_exact_signatures(
        keys,
        expected_ark,
        returned_ark,
        PLAYER_STATE_INPUT_INDEX,
        &player_signers,
        "player state",
    )?;
    verify_exact_signatures(
        keys,
        expected_ark,
        returned_ark,
        TREE_INPUT_INDEX,
        &tree_signers,
        "tree",
    )?;

    if expected_checkpoints.len() != 2 || returned_checkpoints.len() != 2 {
        return Err(anyhow!(
            "emulator returned an invalid player chop checkpoint count"
        ));
    }
    let mut returned_by_txid = HashMap::new();
    for checkpoint in returned_checkpoints {
        let txid = checkpoint.unsigned_tx.compute_txid();
        if returned_by_txid.insert(txid, checkpoint).is_some() {
            return Err(anyhow!("emulator returned duplicate checkpoint {txid}"));
        }
    }
    let mut expected_txids = HashSet::new();
    for (checkpoint_index, expected) in expected_checkpoints.iter().enumerate() {
        let txid = expected.unsigned_tx.compute_txid();
        if !expected_txids.insert(txid) {
            return Err(anyhow!("expected duplicate checkpoint {txid}"));
        }
        if expected_ark.unsigned_tx.input[checkpoint_index].previous_output
            != (OutPoint { txid, vout: 0 })
        {
            return Err(anyhow!(
                "expected checkpoint {txid} does not map to Ark input {checkpoint_index}"
            ));
        }
        let returned = returned_by_txid
            .remove(&txid)
            .ok_or_else(|| anyhow!("emulator omitted checkpoint {txid}"))?;
        if expected.unsigned_tx != returned.unsigned_tx
            || expected.unsigned_tx.input.len() != 1
            || expected.inputs.len() != 1
            || returned.inputs.len() != 1
        {
            return Err(anyhow!("emulator changed checkpoint {txid}"));
        }
        let (signers, label): (&[(XOnlyPublicKey, &str)], &str) = match checkpoint_index {
            PLAYER_STATE_INPUT_INDEX => (&player_signers, "player state checkpoint"),
            TREE_INPUT_INDEX => (&tree_signers, "tree checkpoint"),
            _ => unreachable!("validated checkpoint count"),
        };
        verify_exact_signatures(keys, expected, &returned, 0, signers, label)?;
    }
    if !returned_by_txid.is_empty() {
        return Err(anyhow!(
            "emulator returned unexpected player chop checkpoints"
        ));
    }
    Ok(())
}

fn verify_exact_signatures(
    keys: &crate::Keys,
    expected: &Psbt,
    returned: &Psbt,
    input_index: usize,
    signers: &[(XOnlyPublicKey, &str)],
    label: &str,
) -> Result<()> {
    let expected_input = expected
        .inputs
        .get(input_index)
        .ok_or_else(|| anyhow!("expected PSBT is missing {label} input"))?;
    let (_, (spend_script, _)) = expected_input
        .tap_scripts
        .first_key_value()
        .ok_or_else(|| anyhow!("expected {label} input has no spend script"))?;
    let closure_signers = ark_core::script::extract_checksig_pubkeys(spend_script);
    if closure_signers.len() != signers.len()
        || signers
            .iter()
            .any(|(signer, _)| !closure_signers.contains(signer))
    {
        return Err(anyhow!("expected {label} input has an invalid signer set"));
    }

    let returned_input = returned
        .inputs
        .get(input_index)
        .ok_or_else(|| anyhow!("returned PSBT is missing {label} input"))?;
    if !returned_input.partial_sigs.is_empty()
        || returned_input.tap_key_sig.is_some()
        || returned_input.final_script_sig.is_some()
        || returned_input.final_script_witness.is_some()
    {
        return Err(anyhow!(
            "returned {label} input has signatures outside the required taproot script path"
        ));
    }
    let signatures = returned_input.tap_script_sigs.len();
    if signatures != signers.len() {
        return Err(anyhow!(
            "returned {label} input has {signatures} signatures, expected {}",
            signers.len()
        ));
    }
    for (signer, name) in signers {
        crate::txbuild::verified_signature_for_key(
            keys,
            expected,
            returned,
            input_index,
            *signer,
            name,
        )?;
    }
    Ok(())
}

fn player_packet_payloads(tx: &Transaction) -> Result<[Option<&[u8]>; 3]> {
    let mut packets = [None, None, None];
    for output in &tx.output {
        let Some(extension) = ark_core::extension::extension_payload(&output.script_pubkey) else {
            continue;
        };
        for (packet_type, payload) in ark_core::extension::iter_packets(extension)
            .context("decode ARK extension containing player state")?
        {
            let slot = match packet_type {
                PLAYER_ROLL_PACKET_TYPE => &mut packets[0],
                PLAYER_LUCK_CREDIT_PACKET_TYPE => &mut packets[1],
                PLAYER_AXE_PACKET_TYPE => &mut packets[2],
                _ => continue,
            };
            if slot.replace(payload).is_some() {
                return Err(anyhow!("duplicate player packet type {packet_type}"));
            }
        }
    }
    Ok(packets)
}

fn push_player_roll_bucket(builder: Builder, player_input_index: usize) -> Builder {
    let positive_sign = PushBytesBuf::try_from(vec![0]).expect("one-byte sign extension");
    builder
        .push_int(PLAYER_ROLL_PACKET_TYPE.into())
        .push_int(player_input_index as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SIZE)
        .push_int(PLAYER_ROLL_LEN as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SHA256)
        .push_opcode(OP_DUP)
        .push_int(PLAYER_ROLL_PACKET_TYPE.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
        .push_slice(positive_sign)
        .push_opcode(OP_CAT)
        .push_opcode(op::BIN2NUM)
        .push_int(CHOP_ROLL_BASIS_POINTS as i64)
        .push_opcode(OP_MOD)
}

fn push_player_axe_value(builder: Builder, input_index: Option<usize>) -> Builder {
    let builder = builder.push_int(PLAYER_AXE_PACKET_TYPE.into());
    let builder = match input_index {
        Some(index) => builder
            .push_int(index as i64)
            .push_opcode(op::INSPECTINPUTPACKET),
        None => builder.push_opcode(op::INSPECTPACKET),
    };
    builder
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SIZE)
        .push_int(PLAYER_AXE_LEN as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_opcode(op::BIN2NUM)
        .push_opcode(OP_DUP)
        .push_int(PLAYER_AXE_LEN as i64)
        .push_opcode(op::NUM2BIN)
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int(-1)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY)
        .push_opcode(OP_DUP)
        .push_int(4)
        .push_opcode(OP_LESSTHAN)
        .push_opcode(OP_VERIFY)
}

fn push_axe_bonus_basis_points(builder: Builder, player_input_index: usize) -> Builder {
    push_player_axe_value(builder, Some(player_input_index))
        .push_opcode(OP_DUP)
        .push_int(AxeTier::Wooden as i64)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP)
        .push_int(WOODEN_AXE_BONUS_BASIS_POINTS as i64)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DUP)
        .push_int(AxeTier::Stone as i64)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP)
        .push_int(STONE_AXE_BONUS_BASIS_POINTS as i64)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DUP)
        .push_int(AxeTier::Iron as i64)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_opcode(OP_DROP)
        .push_int(IRON_AXE_BONUS_BASIS_POINTS as i64)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DROP)
        .push_int(0)
        .push_opcode(OP_ENDIF)
        .push_opcode(OP_ENDIF)
        .push_opcode(OP_ENDIF)
}

fn push_log_drop_basis_points(
    builder: Builder,
    player_input_index: usize,
    xp_asset: AssetId,
) -> Builder {
    let builder =
        crate::tree::push_optional_input_asset_lookup(builder, player_input_index, xp_asset)
            .push_opcode(OP_DROP)
            .push_int(1)
            .push_opcode(OP_ADD)
            .push_int(BASE_LOG_DROP_BASIS_POINTS as i64);
    let mut builder = builder;
    for threshold in LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS {
        builder = builder
            .push_opcode(bitcoin::opcodes::all::OP_OVER)
            .push_int(threshold as i64)
            .push_opcode(OP_GREATERTHAN)
            .push_opcode(OP_IF)
            .push_int(LEVEL_LOG_DROP_BONUS_BASIS_POINTS as i64)
            .push_opcode(OP_ADD)
            .push_opcode(OP_ENDIF);
    }
    let builder = builder
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_DROP)
        .push_opcode(OP_FROMALTSTACK);
    push_axe_bonus_basis_points(builder, player_input_index).push_opcode(OP_ADD)
}
/// Derive the independent next-roll material outcome. Leaves 0 for none, 1
/// for STONE, or 2 for IRON ORE on the stack; LOG success is applied by the
/// tree covenant after this helper returns.
pub(crate) fn push_material_drop_code(
    builder: Builder,
    player_input_index: usize,
    xp_asset: AssetId,
) -> Builder {
    let domain = PushBytesBuf::try_from(MATERIAL_ROLL_DOMAIN.to_vec())
        .expect("material-roll domain is a bounded push");
    let positive_sign = PushBytesBuf::try_from(vec![0]).expect("one-byte sign extension");
    let builder = builder
        .push_slice(domain)
        .push_int(PLAYER_ROLL_PACKET_TYPE.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SIZE)
        .push_int(PLAYER_ROLL_LEN as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_CAT)
        .push_opcode(OP_SHA256)
        .push_slice(positive_sign)
        .push_opcode(OP_CAT)
        .push_opcode(op::BIN2NUM)
        .push_int(CHOP_ROLL_BASIS_POINTS as i64)
        .push_opcode(OP_MOD);
    let builder =
        crate::tree::push_optional_input_asset_lookup(builder, player_input_index, xp_asset)
            .push_opcode(OP_DROP)
            .push_int((IRON_ORE_UNLOCK_XP_BALANCE - 1) as i64)
            .push_opcode(OP_GREATERTHAN)
            .push_opcode(OP_IF)
            .push_opcode(OP_DUP)
            .push_int(IRON_ORE_DROP_BASIS_POINTS as i64)
            .push_opcode(OP_LESSTHAN)
            .push_opcode(OP_IF)
            .push_opcode(OP_DROP)
            .push_int(MaterialDrop::IronOre as i64)
            .push_opcode(OP_ELSE)
            .push_int((IRON_ORE_DROP_BASIS_POINTS + STONE_DROP_BASIS_POINTS) as i64)
            .push_opcode(OP_LESSTHAN)
            .push_opcode(OP_IF)
            .push_int(MaterialDrop::Stone as i64)
            .push_opcode(OP_ELSE)
            .push_int(MaterialDrop::None as i64)
            .push_opcode(OP_ENDIF)
            .push_opcode(OP_ENDIF)
            .push_opcode(OP_ELSE)
            .push_int(STONE_DROP_BASIS_POINTS as i64)
            .push_opcode(OP_LESSTHAN)
            .push_opcode(OP_IF)
            .push_int(MaterialDrop::Stone as i64)
            .push_opcode(OP_ELSE)
            .push_int(MaterialDrop::None as i64)
            .push_opcode(OP_ENDIF)
            .push_opcode(OP_ENDIF);
    builder
}

fn push_luck_credit_value(builder: Builder, input_index: Option<usize>) -> Builder {
    let builder = builder.push_int(PLAYER_LUCK_CREDIT_PACKET_TYPE.into());
    let builder = match input_index {
        Some(index) => builder
            .push_int(index as i64)
            .push_opcode(op::INSPECTINPUTPACKET),
        None => builder.push_opcode(op::INSPECTPACKET),
    };
    builder
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SIZE)
        .push_int(PLAYER_LUCK_CREDIT_LEN as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_opcode(op::BIN2NUM)
        .push_opcode(OP_DUP)
        .push_int(PLAYER_LUCK_CREDIT_LEN as i64)
        .push_opcode(op::NUM2BIN)
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int(-1)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY)
        .push_opcode(OP_DUP)
        .push_int((MAX_LUCK_CREDIT + 1) as i64)
        .push_opcode(OP_LESSTHAN)
        .push_opcode(OP_VERIFY)
}
/// Require direct activation to seed its roll from the personalized player
/// P2TR witness program and start with canonical credit. The PLAYER_ID AssetId
/// txid equals the state input outpoint txid only on the first recursive spend.
pub(crate) fn push_canonical_initial_player_luck(
    builder: Builder,
    player_input_index: usize,
) -> Builder {
    let domain = PushBytesBuf::try_from(PLAYER_ROLL_DOMAIN.to_vec())
        .expect("player-roll domain is a bounded push");
    let builder = builder
        .push_int(crate::protocol::PLAYER_ID_ASSET_GROUP_INDEX as i64)
        .push_opcode(op::INSPECTASSETGROUPASSETID)
        .push_int(crate::protocol::PLAYER_ID_ASSET_GROUP_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(player_input_index as i64)
        .push_opcode(op::INSPECTINPUTOUTPOINT)
        .push_opcode(OP_DROP)
        .push_opcode(OP_EQUAL)
        .push_opcode(OP_IF)
        .push_slice(domain)
        .push_int(player_input_index as i64)
        .push_opcode(op::INSPECTINPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_CAT)
        .push_opcode(OP_SHA256)
        .push_int(PLAYER_ROLL_PACKET_TYPE.into())
        .push_int(player_input_index as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_luck_credit_value(builder, Some(player_input_index))
        .push_int(INITIAL_LUCK_CREDIT as i64)
        .push_opcode(OP_EQUALVERIFY);
    push_player_axe_value(builder, Some(player_input_index))
        .push_int(AxeTier::None as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_ENDIF)
}

/// Verify and advance the player-bound roll and bounded luck credit, leaving
/// the canonical reward bit on the stack. XP comes only from the soulbound
/// asset balance.
pub(crate) fn push_advanced_player_luck(
    builder: Builder,
    player_input_index: usize,
    xp_asset: AssetId,
) -> Builder {
    let builder = push_player_axe_value(builder, Some(player_input_index));
    let builder = push_player_axe_value(builder, None).push_opcode(OP_EQUALVERIFY);
    let builder = push_player_roll_bucket(builder, player_input_index);
    let builder = push_log_drop_basis_points(builder, player_input_index, xp_asset)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_LESSTHAN);
    let builder = push_luck_credit_value(builder, Some(player_input_index))
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_ADD)
        .push_opcode(OP_DUP)
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_DUP)
        .push_int(MAX_LUCK_CREDIT as i64)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_IF)
        .push_opcode(OP_2DROP)
        .push_int(1)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DUP)
        .push_int(CHOP_ROLL_BASIS_POINTS as i64)
        .push_opcode(OP_LESSTHAN)
        .push_opcode(OP_IF)
        .push_opcode(OP_2DROP)
        .push_int(0)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_DROP)
        .push_opcode(OP_ENDIF)
        .push_opcode(OP_ENDIF)
        .push_opcode(OP_DUP)
        .push_opcode(OP_IF)
        .push_opcode(OP_FROMALTSTACK)
        .push_int(CHOP_ROLL_BASIS_POINTS as i64)
        .push_opcode(OP_SUB)
        .push_opcode(OP_ELSE)
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_ENDIF);
    push_luck_credit_value(builder, None).push_opcode(OP_EQUALVERIFY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::hex::DisplayHex;
    use bitcoin::secp256k1::{Keypair, SecretKey};
    use bitcoin::{Address, OutPoint, Txid};

    fn asset(byte: u8, group_index: u16) -> AssetId {
        AssetId {
            txid: Txid::from_byte_array([byte; 32]),
            group_index,
        }
    }

    fn xonly(secp: &Secp256k1<bitcoin::secp256k1::All>, byte: u8) -> XOnlyPublicKey {
        Keypair::from_secret_key(secp, &SecretKey::from_slice(&[byte; 32]).unwrap())
            .x_only_public_key()
            .0
    }

    fn player_script(byte: u8) -> ScriptBuf {
        let secp = Secp256k1::new();
        Address::p2tr(&secp, xonly(&secp, byte), None, Network::Regtest).script_pubkey()
    }

    fn state() -> PlayerState {
        PlayerState {
            luck: PlayerLuck::initial(&player_script(7)).unwrap(),
            axe: crate::player::AxeTier::None,
        }
    }

    fn transition(success: bool) -> PlayerChopTransition {
        let tree_state = crate::tree::TreeState {
            tree_id: 417,
            x: 7,
            y: 13,
        };
        let player_xp = 4;
        let mut previous_state = state();
        previous_state.luck.credit = PlayerLuckCredit::new(
            CHOP_ROLL_BASIS_POINTS - log_drop_basis_points(player_xp, crate::player::AxeTier::None),
        )
        .unwrap();
        loop {
            let (next_luck, drop) = previous_state
                .luck
                .advance(player_xp, crate::player::AxeTier::None);
            if drop == success {
                let reward = u64::from(success);
                let next_state = PlayerState {
                    luck: next_luck,
                    axe: crate::player::AxeTier::None,
                };
                let material = material_drop(next_luck.roll, player_xp, success);
                let stone_reward = u64::from(material == MaterialDrop::Stone);
                let iron_ore_reward = u64::from(material == MaterialDrop::IronOre);
                return PlayerChopTransition {
                    previous_state,
                    next_state,
                    previous_tree_state: tree_state,
                    next_tree_state: tree_state,
                    previous_tree_health: crate::tree::TreeHealth::new(5).unwrap(),
                    next_tree_health: crate::tree::TreeHealth::new(5 - reward).unwrap(),
                    success,
                    state_logs_before: 9,
                    state_logs_after: 9 + reward,
                    state_xp_balance_before: player_xp,
                    state_xp_balance_after: player_xp + reward,
                    state_stone_before: 2,
                    state_stone_after: 2 + stone_reward,
                    state_iron_ore_before: 1,
                    state_iron_ore_after: 1 + iron_ore_reward,
                    tree_markers_before: 1,
                    tree_markers_after: 1,
                    tree_logs_before: 5,
                    tree_logs_after: 5 - reward,
                    tree_xp_balance_before: 5,
                    tree_xp_balance_after: 5 - reward,
                    tree_stone_before: 5,
                    tree_stone_after: 5 - stone_reward,
                    tree_iron_ore_before: 5,
                    tree_iron_ore_after: 5 - iron_ore_reward,
                    state_value_before: 330,
                    state_value_after: 330,
                    tree_value_before: 330,
                    tree_value_after: 330,
                    dust_sats: 330,
                };
            }
            previous_state.luck.roll = next_luck.roll;
        }
    }

    #[test]
    fn player_state_packet_set_is_minimal() {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn::default()],
            output: Vec::new(),
        })
        .unwrap();
        attach_player_state_packets(&mut psbt, state()).unwrap();
        let packets = player_packet_payloads(&psbt.unsigned_tx).unwrap();
        assert_eq!(packets[0].unwrap().len(), PLAYER_ROLL_LEN);
        assert_eq!(packets[1].unwrap().len(), PLAYER_LUCK_CREDIT_LEN);
        assert_eq!(packets[2].unwrap(), AxeTier::None.encode());
        let mut packet_types = Vec::new();
        for output in &psbt.unsigned_tx.output {
            if let Some(extension) = ark_core::extension::extension_payload(&output.script_pubkey) {
                packet_types.extend(
                    ark_core::extension::iter_packets(extension)
                        .unwrap()
                        .into_iter()
                        .map(|(packet_type, _)| packet_type),
                );
            }
        }
        packet_types.sort_unstable();
        assert_eq!(
            packet_types,
            vec![
                PLAYER_ROLL_PACKET_TYPE,
                PLAYER_LUCK_CREDIT_PACKET_TYPE,
                PLAYER_AXE_PACKET_TYPE,
            ]
        );
    }
    #[test]
    fn player_luck_vectors_and_initial_rate_are_stable() {
        let initial = PlayerLuck::initial(&player_script(7)).unwrap();
        assert_eq!(
            initial.roll.encode().to_lower_hex_string(),
            "d4a984b5774124103b9d038e1ac8cb77cd554bcef95e2fad0c1bdd86c40e5cd7"
        );
        assert_eq!(
            initial.credit.encode().to_lower_hex_string(),
            "401f00000000000000"
        );
        let (next, success) = initial.advance(0, crate::player::AxeTier::None);
        assert_eq!(
            next.roll.encode().to_lower_hex_string(),
            "0e89adc39d4766f1e3cb6aaa62d2531db41f014971e9e1c2fe169fe2fd71a836"
        );
        assert_eq!(next.roll.bucket(), 990);
        assert_eq!(success, next.roll.bucket() < BASE_LOG_DROP_BASIS_POINTS);
        assert_eq!(
            initial.credit.value() + BASE_LOG_DROP_BASIS_POINTS,
            LUCK_WINDOW_BASIS_POINTS
        );
        assert!(success);
        assert_eq!(next.credit.value(), 0);

        let mut negative_zero = [0_u8; PLAYER_LUCK_CREDIT_LEN];
        negative_zero[PLAYER_LUCK_CREDIT_LEN - 1] = 0x80;
        assert!(PlayerLuckCredit::decode(&negative_zero).is_err());
        let mut over_max = [0_u8; PLAYER_LUCK_CREDIT_LEN];
        over_max[..8].copy_from_slice(&(MAX_LUCK_CREDIT + 1).to_le_bytes());
        assert!(PlayerLuckCredit::decode(&over_max).is_err());
    }

    #[test]
    fn axe_recipes_drop_bonuses_and_material_vectors_are_stable() {
        assert_eq!(
            AXE_RECIPES,
            [
                AxeRecipe {
                    axe: AxeTier::Wooden,
                    required_level: 1,
                    log_cost: 1,
                    stone_cost: 0,
                    iron_ore_cost: 0,
                },
                AxeRecipe {
                    axe: AxeTier::Stone,
                    required_level: 5,
                    log_cost: 2,
                    stone_cost: 2,
                    iron_ore_cost: 0,
                },
                AxeRecipe {
                    axe: AxeTier::Iron,
                    required_level: 15,
                    log_cost: 5,
                    stone_cost: 0,
                    iron_ore_cost: 2,
                },
            ]
        );
        assert_eq!(AXE_RECIPES.map(AxeRecipe::required_xp_balance), [0, 16, 97]);
        for (axe, base, capped) in [
            (AxeTier::None, 2_000, 3_000),
            (AxeTier::Wooden, 2_200, 3_200),
            (AxeTier::Stone, 2_500, 3_500),
            (AxeTier::Iron, 2_800, 3_800),
        ] {
            assert_eq!(log_drop_basis_points(0, axe), base);
            assert_eq!(log_drop_basis_points(u64::MAX, axe), capped);
            assert_eq!(AxeTier::decode(&axe.encode()).unwrap(), axe);
        }
        assert!(AxeTier::decode(&[4]).is_err());

        let initial = PlayerLuck::initial(&player_script(7)).unwrap().roll;
        assert_eq!(initial.material_bucket(), 5_857);
        let no_material = initial.next();
        assert_eq!(no_material.material_bucket(), 8_780);
        assert_eq!(
            material_drop(no_material, IRON_ORE_UNLOCK_XP_BALANCE, true),
            MaterialDrop::None
        );
        let stone = no_material.next();
        assert_eq!(stone.material_bucket(), 922);
        assert_eq!(material_drop(stone, 0, true), MaterialDrop::Stone);
        assert_eq!(
            material_drop(stone, IRON_ORE_UNLOCK_XP_BALANCE, true),
            MaterialDrop::Stone
        );

        let mut iron = initial;
        for _ in 0..106 {
            iron = iron.next();
        }
        assert_eq!(iron.material_bucket(), 179);
        assert_eq!(IRON_ORE_UNLOCK_XP_BALANCE, 47);
        assert_eq!(
            material_drop(iron, IRON_ORE_UNLOCK_XP_BALANCE - 1, true),
            MaterialDrop::Stone
        );
        assert_eq!(
            material_drop(iron, IRON_ORE_UNLOCK_XP_BALANCE, true),
            MaterialDrop::IronOre
        );
        assert_eq!(
            material_drop(iron, IRON_ORE_UNLOCK_XP_BALANCE, false),
            MaterialDrop::None
        );
    }

    #[test]
    fn luck_credit_bounds_long_runs_above_and_below_expectation() {
        let xp_balances = [
            0,
            LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS[0],
            LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS[1],
            LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS[2],
            LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS[3],
            LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS[4],
        ];
        for (index, xp_balance) in xp_balances.into_iter().enumerate() {
            let mut luck = PlayerLuck::initial(&player_script(index as u8 + 1)).unwrap();
            let initial_credit = luck.credit.value();
            let rate = log_drop_basis_points(xp_balance, crate::player::AxeTier::None);
            let mut successes = 0_u64;
            let mut miss_run = 0_u64;
            let mut success_run = 0_u64;
            let mut max_miss_run = 0_u64;
            let mut max_success_run = 0_u64;
            for _ in 0..100_000 {
                let previous_credit = luck.credit.value();
                let (next, success) = luck.advance(xp_balance, crate::player::AxeTier::None);
                assert!(next.credit.value() <= MAX_LUCK_CREDIT);
                assert_eq!(
                    next.credit.value() + u64::from(success) * CHOP_ROLL_BASIS_POINTS,
                    previous_credit + rate
                );
                if success {
                    successes += 1;
                    success_run += 1;
                    miss_run = 0;
                    max_success_run = max_success_run.max(success_run);
                } else {
                    miss_run += 1;
                    success_run = 0;
                    max_miss_run = max_miss_run.max(miss_run);
                }
                luck = next;
            }
            assert_eq!(
                successes * CHOP_ROLL_BASIS_POINTS + luck.credit.value(),
                initial_credit + 100_000 * rate
            );
            assert!(
                max_miss_run <= 10,
                "XP asset balance {xp_balance}: miss run {max_miss_run}"
            );
            assert!(
                max_success_run <= 2,
                "XP asset balance {xp_balance}: success run {max_success_run}"
            );
        }
    }

    #[test]
    fn changing_tree_target_cannot_change_the_player_reward() {
        for success in [false, true] {
            let baseline = transition(success);
            for tree_id in [417, 418, 2_516] {
                let mut targeted = baseline;
                targeted.previous_tree_state.tree_id = tree_id;
                targeted.next_tree_state.tree_id = tree_id;
                targeted.validate().unwrap();
                assert_eq!(targeted.next_state.luck, baseline.next_state.luck);
                assert_eq!(targeted.success, baseline.success);
            }
        }
    }

    #[test]
    fn successful_chop_moves_log_and_asset_backed_xp() {
        let valid = transition(true);
        valid.validate().unwrap();

        let mut missing_xp = valid;
        missing_xp.state_xp_balance_after -= 1;
        assert!(missing_xp.validate().is_err());
        let mut forged_luck = valid;
        forged_luck.next_state.luck.credit =
            PlayerLuckCredit::new(forged_luck.next_state.luck.credit.value() + 1).unwrap();
        assert!(forged_luck.validate().is_err());
        let mut missing_log = valid;
        missing_log.state_logs_after -= 1;
        assert!(missing_log.validate().is_err());
        let mut forged_stone = valid;
        forged_stone.state_stone_after += 1;
        assert!(forged_stone.validate().is_err());
        let mut forged_iron_ore = valid;
        forged_iron_ore.state_iron_ore_after += 1;
        assert!(forged_iron_ore.validate().is_err());
        let mut changed_axe = valid;
        changed_axe.next_state.axe = AxeTier::Wooden;
        assert!(changed_axe.validate().is_err());
    }

    #[test]
    fn missed_chop_preserves_inventory_and_asset_backed_xp() {
        let valid = transition(false);
        valid.validate().unwrap();
        let mut forged = valid;
        forged.state_xp_balance_after += 1;
        assert!(forged.validate().is_err());
        let mut forged_material = valid;
        forged_material.tree_stone_after -= 1;
        assert!(forged_material.validate().is_err());
    }

    #[test]
    fn player_contract_supports_owner_and_watchtower_renewal() {
        let secp = Secp256k1::new();
        let owner = xonly(&secp, 3);
        let operator = xonly(&secp, 4);
        let emulator = xonly(&secp, 5);
        let rollover = xonly(&secp, 7);
        let tree_script =
            Address::p2tr(&secp, xonly(&secp, 6), None, Network::Regtest).script_pubkey();
        let contract = build_player_contract(
            &secp,
            owner,
            operator,
            emulator,
            rollover,
            Sequence::from_height(144),
            Network::Regtest,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            &tree_script,
        )
        .unwrap();
        assert_eq!(contract.tree_script_pubkey, tree_script);
        assert_eq!(contract.vtxo.tapscripts().len(), 6);
        assert!(contract.chop_arkade_script.len() <= 10_000);
        let craft_emulator =
            ark_script::compute_arkade_script_public_key(&emulator, &contract.craft_arkade_script)
                .unwrap();
        assert_eq!(contract.craft_tweaked_emulator, craft_emulator);
        let craft_signers =
            ark_core::script::extract_checksig_pubkeys(&contract.craft_spend_script);
        assert_eq!(craft_signers.len(), 3);
        assert!(craft_signers.contains(&owner));
        assert!(craft_signers.contains(&operator));
        assert!(craft_signers.contains(&craft_emulator));
        assert!(!craft_signers.contains(&rollover));

        let renewal_emulator = ark_script::compute_arkade_script_public_key(
            &emulator,
            &contract.renewal_arkade_script,
        )
        .unwrap();
        let owner_signers =
            ark_core::script::extract_checksig_pubkeys(&contract.renewal_spend_script);
        assert_eq!(owner_signers.len(), 3);
        assert!(owner_signers.contains(&owner));
        assert!(owner_signers.contains(&operator));
        assert!(owner_signers.contains(&renewal_emulator));
        let watchtower_signers =
            ark_core::script::extract_checksig_pubkeys(&contract.watchtower_renewal_spend_script);
        assert!(watchtower_signers.contains(&rollover));
        assert!(!watchtower_signers.contains(&owner));

        let player_asset = asset(9, 0);
        let record = crate::arkade::VtxoRecord {
            outpoint: OutPoint {
                txid: Txid::from_byte_array([9; 32]),
                vout: 0,
            },
            script: contract.vtxo.script_pubkey(),
            amount_sats: 330,
            assets: vec![
                Asset {
                    asset_id: player_asset,
                    amount: 1,
                },
                Asset {
                    asset_id: contract.log_asset,
                    amount: 2,
                },
                Asset {
                    asset_id: contract.xp_asset,
                    amount: 4,
                },
            ],
            created_at: Some(1),
            expires_at: Some(i64::MAX),
            is_preconfirmed: false,
            is_swept: false,
            spent_by: None,
            settled_by: None,
            is_unrolled: false,
            is_spent: false,
        };
        validate_player_state_record(&record, &contract, player_asset).unwrap();
        let mut decoy = record.clone();
        decoy.assets[0].asset_id = asset(8, 0);
        assert!(validate_player_state_record(&decoy, &contract, player_asset).is_err());
        validate_player_state_record(&decoy, &contract, asset(8, 0)).unwrap();
        let mut leaked = record;
        leaked.assets.push(Asset {
            asset_id: asset(2, 0),
            amount: 1,
        });
        assert!(validate_player_state_record(&leaked, &contract, player_asset).is_err());
    }

    #[test]
    fn woodcutting_xp_scale_and_level_boundaries_are_exact() {
        assert_eq!(woodcutting_xp(0), 0);
        assert_eq!(woodcutting_xp(1), 25);
        assert_eq!(woodcutting_xp(3), 75);
        assert_eq!(woodcutting_xp(4), 100);
        assert_eq!(woodcutting_xp(u64::MAX), u64::MAX);
        for (xp, level) in [
            (0, 1),
            (82, 1),
            (83, 2),
            (1_153, 9),
            (1_154, 10),
            (4_470, 20),
            (13_363, 30),
            (37_224, 40),
            (101_333, 50),
            (13_034_430, 98),
            (13_034_431, 99),
            (u64::MAX, 99),
        ] {
            assert_eq!(level_from_xp(xp), level, "Woodcutting XP {xp}");
        }
        assert_eq!(level_from_xp(woodcutting_xp(3)), 1);
        assert_eq!(level_from_xp(woodcutting_xp(4)), 2);
        assert_eq!(xp_for_level(1), Some(0));
        assert_eq!(xp_for_level(50), Some(101_333));
        assert_eq!(xp_for_level(99), Some(13_034_431));
        assert_eq!(xp_for_level(0), None);
        assert_eq!(xp_for_level(100), None);
        assert_eq!(
            LEVEL_LOG_DROP_XP_BALANCE_THRESHOLDS,
            [47, 179, 535, 1_489, 4_054]
        );
        for (xp_balance, basis_points) in [
            (0, 2_000),
            (46, 2_000),
            (47, 2_200),
            (178, 2_200),
            (179, 2_400),
            (534, 2_400),
            (535, 2_600),
            (1_488, 2_600),
            (1_489, 2_800),
            (4_053, 2_800),
            (4_054, 3_000),
            (u64::MAX, 3_000),
        ] {
            assert_eq!(
                log_drop_basis_points(xp_balance, AxeTier::None),
                basis_points,
                "XP asset balance {xp_balance}"
            );
        }
    }

    #[test]
    fn withdraw_script_moves_log_only_and_keeps_xp_soulbound() {
        let withdraw = player_withdraw_covenant_script(
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
        )
        .unwrap();
        assert!(withdraw.len() <= 10_000);
        let asm = ark_script::to_asm(&withdraw).unwrap();
        // Two-input/four-output shape pinned by the covenant.
        assert!(asm.contains("OP_INSPECTNUMINPUTS OP_PUSHNUM_2 OP_EQUALVERIFY"));
        assert!(asm.contains("OP_INSPECTNUMOUTPUTS OP_PUSHNUM_4 OP_EQUALVERIFY"));
        // LOG conservation relation: state-in minus state-out equals the
        // destination amount.
        assert!(asm.contains("OP_SUB"));
        // XP preservation uses the optional-lookup ROT-equality pattern, so
        // the XP balance can never leave the player state.
        assert!(asm.contains("OP_ROT"));

        let secp = Secp256k1::new();
        let owner = xonly(&secp, 3);
        let operator = xonly(&secp, 4);
        let emulator = xonly(&secp, 5);
        let rollover = xonly(&secp, 7);
        let tree_script =
            Address::p2tr(&secp, xonly(&secp, 6), None, Network::Regtest).script_pubkey();
        let contract = build_player_contract(
            &secp,
            owner,
            operator,
            emulator,
            rollover,
            Sequence::from_height(144),
            Network::Regtest,
            asset(2, 0),
            asset(2, 1),
            asset(2, 2),
            asset(2, 3),
            asset(2, 4),
            330,
            &tree_script,
        )
        .unwrap();
        let tweaked = ark_script::compute_arkade_script_public_key(
            &emulator,
            &contract.withdraw_arkade_script,
        )
        .unwrap();
        let signers = ark_core::script::extract_checksig_pubkeys(&contract.withdraw_spend_script);
        assert_eq!(signers.len(), 3);
        assert!(signers.contains(&owner));
        assert!(signers.contains(&operator));
        assert!(signers.contains(&tweaked));
    }

    #[test]
    fn craft_covenant_pins_single_state_recipe_burn_shape() {
        let craft =
            player_craft_covenant_script(asset(2, 1), asset(2, 2), asset(2, 3), asset(2, 4), 330)
                .unwrap();
        assert!(craft.len() <= 10_000);
        let asm = ark_script::to_asm(&craft).unwrap();
        assert!(asm.contains("OP_INSPECTVERSION OP_PUSHNUM_3 OP_EQUALVERIFY"));
        assert!(asm.contains("OP_INSPECTNUMINPUTS OP_PUSHNUM_1 OP_EQUALVERIFY"));
        assert!(asm.contains("OP_INSPECTNUMOUTPUTS OP_PUSHNUM_3 OP_EQUALVERIFY"));
        assert!(asm.contains("OP_INSPECTOUTPUTVALUE"));
        assert!(asm.contains("OP_INSPECTINPUTVALUE"));
        for recipe in &AXE_RECIPES[1..] {
            let threshold = recipe.required_xp_balance().saturating_sub(1) as i64;
            let exact_gate = Builder::new()
                .push_int(threshold)
                .push_opcode(OP_GREATERTHAN)
                .push_opcode(OP_VERIFY)
                .into_script();
            assert!(
                craft
                    .as_bytes()
                    .windows(exact_gate.len())
                    .any(|window| window == exact_gate.as_bytes()),
                "{} level gate is absent",
                recipe.axe.display_name()
            );
            let early_gate = Builder::new()
                .push_int(1)
                .push_opcode(OP_ADD)
                .push_int(threshold)
                .push_opcode(OP_GREATERTHAN)
                .push_opcode(OP_VERIFY)
                .into_script();
            assert!(
                !craft
                    .as_bytes()
                    .windows(early_gate.len())
                    .any(|window| window == early_gate.as_bytes()),
                "{} level gate permits one XP unit too early",
                recipe.axe.display_name()
            );
        }
        assert!(player_craft_covenant_script(
            asset(2, 1),
            asset(2, 2),
            asset(2, 2),
            asset(2, 4),
            330,
        )
        .is_err());
    }

    #[test]
    fn player_contract_rejects_tweaked_emulator_collision() {
        let secp = Secp256k1::new();
        let operator = xonly(&secp, 4);
        let emulator = xonly(&secp, 5);
        let rollover = xonly(&secp, 7);
        let tree_script =
            Address::p2tr(&secp, xonly(&secp, 6), None, Network::Regtest).script_pubkey();
        let (tree_asset, log_asset, xp_asset) = (asset(2, 0), asset(2, 1), asset(2, 2));
        let chop = player_chop_covenant_script(
            log_asset,
            xp_asset,
            asset(2, 3),
            asset(2, 4),
            330,
            &tree_script,
        )
        .unwrap();
        let renewal =
            player_renewal_covenant_script(log_asset, xp_asset, asset(2, 3), asset(2, 4), 330)
                .unwrap();
        let withdraw =
            player_withdraw_covenant_script(log_asset, xp_asset, asset(2, 3), asset(2, 4), 330)
                .unwrap();
        let craft =
            player_craft_covenant_script(log_asset, xp_asset, asset(2, 3), asset(2, 4), 330)
                .unwrap();
        // An owner key equal to a script-tweaked emulator key could satisfy
        // the emulator position without executing the covenant.
        for script in [&chop, &renewal, &withdraw, &craft] {
            let owner = ark_script::compute_arkade_script_public_key(&emulator, script).unwrap();
            let error = build_player_contract(
                &secp,
                owner,
                operator,
                emulator,
                rollover,
                Sequence::from_height(144),
                Network::Regtest,
                tree_asset,
                log_asset,
                xp_asset,
                asset(2, 3),
                asset(2, 4),
                330,
                &tree_script,
            )
            .err()
            .unwrap();
            assert_eq!(
                error.to_string(),
                "tweaked emulator collides with a player signer"
            );
        }
    }
}
