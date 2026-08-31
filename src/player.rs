//! Recursive player state for atomic tree chopping.
//!
//! A player owns one 330-sat recursive state VTXO. Harvested LOG and conserved
//! XP remain in that state; the numeric XP packet must equal its XP asset
//! balance, so permissionless activation cannot forge gameplay progress.

use crate::protocol::{
    CHOP_ANCHOR_OUTPUT_INDEX, CHOP_EXTENSION_OUTPUT_INDEX, CHOP_INPUT_COUNT, CHOP_OUTPUT_COUNT,
    PLAYER_IDENTITY_PACKET_TYPE, PLAYER_LUCK_CREDIT_PACKET_TYPE, PLAYER_POSITION_PACKET_TYPE,
    PLAYER_ROLL_PACKET_TYPE, PLAYER_STATE_INPUT_INDEX, PLAYER_STATE_OUTPUT_INDEX,
    PLAYER_XP_PACKET_TYPE, RENEWAL_STATE_INPUT_INDEX, RENEWAL_STATE_OUTPUT_INDEX, TREE_INPUT_INDEX,
    TREE_OUTPUT_INDEX,
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

const PLAYER_IDENTITY_MAGIC: &[u8; 2] = b"PI";
const PLAYER_IDENTITY_VERSION: u8 = 1;
const PLAYER_IDENTITY_LEN: usize = 35;
const PLAYER_POSITION_MAGIC: &[u8; 2] = b"PP";
const PLAYER_POSITION_VERSION: u8 = 1;
const PLAYER_POSITION_LEN: usize = 7;
const PLAYER_XP_LEN: usize = 9;
const PLAYER_ROLL_LEN: usize = 32;
const PLAYER_LUCK_CREDIT_LEN: usize = 9;
const P2A_PROGRAM: [u8; 2] = [0x4e, 0x73];

/// Immutable identity preserved by every transition of one player covenant.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerIdentity {
    pub player_id: [u8; 32],
}

impl PlayerIdentity {
    pub fn encode(self) -> [u8; PLAYER_IDENTITY_LEN] {
        let mut encoded = [0_u8; PLAYER_IDENTITY_LEN];
        encoded[..2].copy_from_slice(PLAYER_IDENTITY_MAGIC);
        encoded[2] = PLAYER_IDENTITY_VERSION;
        encoded[3..].copy_from_slice(&self.player_id);
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() != PLAYER_IDENTITY_LEN
            || &encoded[..2] != PLAYER_IDENTITY_MAGIC
            || encoded[2] != PLAYER_IDENTITY_VERSION
        {
            return Err(anyhow!("invalid player identity packet"));
        }

        Ok(Self {
            player_id: encoded[3..]
                .try_into()
                .expect("validated player identity length"),
        })
    }
}

pub fn derive_player_identity(owner: XOnlyPublicKey, genesis_txid: Txid) -> PlayerIdentity {
    let mut preimage = b"woodland.sh/PlayerIdentity/v1".to_vec();
    preimage.extend_from_slice(&owner.serialize());
    preimage.extend_from_slice(genesis_txid.as_byte_array());
    PlayerIdentity {
        player_id: sha256::Hash::hash(&preimage).to_byte_array(),
    }
}

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

/// Current map coordinate. A chop preserves this packet byte-for-byte; a
/// separate movement transition can later constrain position changes.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerPosition {
    pub x: u16,
    pub y: u16,
}

impl PlayerPosition {
    pub fn encode(self) -> [u8; PLAYER_POSITION_LEN] {
        let mut encoded = [0_u8; PLAYER_POSITION_LEN];
        encoded[..2].copy_from_slice(PLAYER_POSITION_MAGIC);
        encoded[2] = PLAYER_POSITION_VERSION;
        encoded[3..5].copy_from_slice(&self.x.to_le_bytes());
        encoded[5..7].copy_from_slice(&self.y.to_le_bytes());
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() != PLAYER_POSITION_LEN
            || &encoded[..2] != PLAYER_POSITION_MAGIC
            || encoded[2] != PLAYER_POSITION_VERSION
        {
            return Err(anyhow!("invalid player position packet"));
        }

        Ok(Self {
            x: u16::from_le_bytes(encoded[3..5].try_into().expect("fixed player position")),
            y: u16::from_le_bytes(encoded[5..7].try_into().expect("fixed player position")),
        })
    }
}

/// XP held by a recursive player-state VTXO.
/// XP is the numeric counter on canonical PLAYER state. Every increment is
/// paired with one transferred world XP unit by the tree covenant.
pub const PLAYER_LEVEL_CURVE: &str = "woodland-xp-v1";
pub const MAX_PLAYER_LEVEL: u64 = 99;

/// Canonical protocol XP thresholds for levels 1 through 99.
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

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlayerXp(u64);

impl PlayerXp {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
    pub fn encode(self) -> [u8; PLAYER_XP_LEN] {
        let mut encoded = [0_u8; PLAYER_XP_LEN];
        encoded[..8].copy_from_slice(&self.0.to_le_bytes());
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() != PLAYER_XP_LEN || encoded[8] != 0 {
            return Err(anyhow!("invalid player XP packet"));
        }
        Ok(Self(u64::from_le_bytes(
            encoded[..8].try_into().expect("fixed player XP"),
        )))
    }

    pub fn increment(self) -> Result<Self> {
        let next = self
            .0
            .checked_add(1)
            .ok_or_else(|| anyhow!("player XP overflow"))?;
        Ok(Self(next))
    }

    pub fn level(self) -> u64 {
        level_from_xp(self.0)
    }
}

impl TryFrom<u64> for PlayerXp {
    type Error = anyhow::Error;

    fn try_from(value: u64) -> Result<Self> {
        Ok(Self::new(value))
    }
}

impl From<PlayerXp> for u64 {
    fn from(value: PlayerXp) -> Self {
        value.value()
    }
}

impl serde::Serialize for PlayerXp {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PlayerXp {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::new(value))
    }
}

pub const CHOP_ROLL_BASIS_POINTS: u64 = 10_000;
pub const BASE_LOG_DROP_BASIS_POINTS: u64 = 2_000;
pub const LEVEL_LOG_DROP_BONUS_BASIS_POINTS: u64 = 200;
pub const LEVEL_LOG_DROP_XP_THRESHOLDS: [u64; 5] = [1_154, 4_470, 13_363, 37_224, 101_333];
pub const MAX_LEVEL_LOG_DROP_BASIS_POINTS: u64 = 3_000;
pub const LUCK_WINDOW_BASIS_POINTS: u64 = 10_000;
pub const MAX_LUCK_CREDIT: u64 = LUCK_WINDOW_BASIS_POINTS * 2;
pub const INITIAL_LUCK_CREDIT: u64 = LUCK_WINDOW_BASIS_POINTS - BASE_LOG_DROP_BASIS_POINTS;
const PLAYER_ROLL_DOMAIN: &[u8] = b"woodland.sh/player-roll/v1";

pub const fn log_drop_basis_points(player_xp: u64) -> u64 {
    let mut basis_points = BASE_LOG_DROP_BASIS_POINTS;
    let mut index = 0;
    while index < LEVEL_LOG_DROP_XP_THRESHOLDS.len() {
        if player_xp >= LEVEL_LOG_DROP_XP_THRESHOLDS[index] {
            basis_points += LEVEL_LOG_DROP_BONUS_BASIS_POINTS;
        }
        index += 1;
    }
    if basis_points > MAX_LEVEL_LOG_DROP_BASIS_POINTS {
        MAX_LEVEL_LOG_DROP_BASIS_POINTS
    } else {
        basis_points
    }
}

/// Public deterministic entropy bound to one recursive player lineage.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct PlayerRoll([u8; PLAYER_ROLL_LEN]);

impl PlayerRoll {
    pub fn initial(identity: PlayerIdentity) -> Self {
        let mut engine = sha256::Hash::engine();
        engine.input(PLAYER_ROLL_DOMAIN);
        engine.input(&identity.encode());
        Self(sha256::Hash::from_engine(engine).to_byte_array())
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
    pub fn initial(identity: PlayerIdentity) -> Self {
        Self {
            roll: PlayerRoll::initial(identity),
            credit: PlayerLuckCredit::initial(),
        }
    }

    pub fn advance(self, player_xp: u64) -> (Self, bool) {
        let next_roll = self.roll.next();
        let drop_basis_points = log_drop_basis_points(player_xp);
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

/// Level is derived, never committed as a second mutable state value. XP can
/// continue increasing at level 99 without changing the displayed level.
pub fn level_from_xp(xp: u64) -> u64 {
    XP_FOR_LEVEL.partition_point(|threshold| *threshold <= xp) as u64
}

pub fn xp_for_level(level: u64) -> Option<u64> {
    let index = usize::try_from(level.checked_sub(1)?).ok()?;
    XP_FOR_LEVEL.get(index).copied()
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    pub identity: PlayerIdentity,
    pub position: PlayerPosition,
    pub luck: PlayerLuck,
    pub xp: PlayerXp,
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
    pub tree_markers_before: u64,
    pub tree_markers_after: u64,
    pub tree_logs_before: u64,
    pub tree_logs_after: u64,
    pub tree_xp_balance_before: u64,
    pub tree_xp_balance_after: u64,
    pub state_value_before: u64,
    pub state_value_after: u64,
    pub tree_value_before: u64,
    pub tree_value_after: u64,
    pub dust_sats: u64,
}

impl PlayerChopTransition {
    pub fn validate(self) -> Result<()> {
        if self.next_state.identity != self.previous_state.identity {
            return Err(anyhow!("player identity changed during chop"));
        }
        if self.next_state.position != self.previous_state.position {
            return Err(anyhow!("player position changed during chop"));
        }
        let (expected_luck, expected_success) = self
            .previous_state
            .luck
            .advance(self.previous_state.xp.value());
        if self.next_state.luck != expected_luck || self.success != expected_success {
            return Err(anyhow!("player chop luck transition is invalid"));
        }
        let reward = u64::from(self.success);
        if self.previous_state.xp.value() != self.state_xp_balance_before
            || self.next_state.xp.value() != self.state_xp_balance_after
            || self.state_xp_balance_after
                != self
                    .state_xp_balance_before
                    .checked_add(reward)
                    .ok_or_else(|| anyhow!("player XP asset overflow"))?
        {
            return Err(anyhow!(
                "player XP counter must equal its conserved XP asset balance"
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
    /// Owner-authorized LOG withdrawal; XP is soulbound and cannot move.
    pub withdraw_spend_script: ScriptBuf,
    pub withdraw_arkade_script: ScriptBuf,
    pub owner: XOnlyPublicKey,
    pub operator: XOnlyPublicKey,
    pub emulator: XOnlyPublicKey,
    pub chop_tweaked_emulator: XOnlyPublicKey,
    pub log_asset: AssetId,
    pub xp_asset: AssetId,
    pub dust_sats: u64,
    pub tree_script_pubkey: ScriptBuf,
}

/// Build a personalized recursive player contract. Activation supplies one
/// unique uncontrolled PLAYER_ID asset, which the covenant preserves
/// dynamically without committing its post-transaction AssetId into P2TR.
///
/// Tapleaf authority is deliberately split:
///
/// - chop: owner + operator + covenant-tweaked emulator;
/// - owner renewal: owner + operator + covenant-tweaked emulator;
/// - watchtower renewal: rollover + operator + covenant-tweaked emulator;
/// - LOG withdrawal: owner + operator + covenant-tweaked emulator.
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
    if [tree_asset, log_asset, xp_asset]
        .into_iter()
        .collect::<HashSet<_>>()
        .len()
        != 3
    {
        return Err(anyhow!("TREE, LOG, and XP assets must differ"));
    }
    let chop_arkade_script =
        player_chop_covenant_script(log_asset, xp_asset, dust_sats, tree_script)?;
    let renewal_arkade_script = player_renewal_covenant_script(log_asset, xp_asset, dust_sats)?;
    let withdraw_arkade_script = player_withdraw_covenant_script(log_asset, xp_asset, dust_sats)?;
    let chop_tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, &chop_arkade_script)
            .context("derive player emulator signer")?;
    let renewal_tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, &renewal_arkade_script)
            .context("derive player emulator signer")?;
    let withdraw_tweaked_emulator =
        ark_script::compute_arkade_script_public_key(&emulator_pk, &withdraw_arkade_script)
            .context("derive player emulator signer")?;
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
    ])
    .context("build player Arkade tapleaves")?;
    let [chop_spend_script, renewal_spend_script, watchtower_renewal_spend_script, withdraw_spend_script] =
        processed.scripts.as_slice()
    else {
        return Err(anyhow!(
            "player contract must have chop, renewal, and withdraw leaves"
        ));
    };
    let chop_spend_script = chop_spend_script.clone();
    let renewal_spend_script = renewal_spend_script.clone();
    let watchtower_renewal_spend_script = watchtower_renewal_spend_script.clone();
    let withdraw_spend_script = withdraw_spend_script.clone();
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
        owner: owner_pk,
        operator: operator_pk,
        emulator: emulator_pk,
        chop_tweaked_emulator,
        log_asset,
        xp_asset,
        dust_sats,
        tree_script_pubkey: tree_script.clone(),
    })
}

/// Covenant for both player batch-renewal paths. It permits only an exact
/// self-send preserving P2TR, value, packets, PLAYER_ID, LOG, and XP.
pub fn player_renewal_covenant_script(
    log_asset: AssetId,
    xp_asset: AssetId,
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
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_IDENTITY_PACKET_TYPE);
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_POSITION_PACKET_TYPE);
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_ROLL_PACKET_TYPE);
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_LUCK_CREDIT_PACKET_TYPE);
    let builder = crate::tree::push_equal_state_packet(builder, PLAYER_XP_PACKET_TYPE);
    let builder = crate::tree::push_renewal_asset_shell(builder)?;
    let builder = crate::tree::push_player_marker_group(
        builder,
        RENEWAL_STATE_INPUT_INDEX,
        RENEWAL_STATE_OUTPUT_INDEX,
    );
    let builder = crate::tree::push_player_renewal_group_set(builder, log_asset, xp_asset);

    let builder = crate::tree::push_optional_input_asset_lookup(
        builder,
        RENEWAL_STATE_INPUT_INDEX,
        log_asset,
    );
    let builder = crate::tree::push_optional_output_asset_lookup(
        builder,
        RENEWAL_STATE_OUTPUT_INDEX,
        log_asset,
    )
    .push_opcode(OP_ROT)
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
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUAL)
        .into_script())
}

/// Covenant for the owner-authorized LOG withdrawal leaf. Only LOG may leave:
/// the player state must exit with its exact P2TR, dust, PLAYER_ID, identity,
/// position, luck, XP packet, and XP balance intact, so XP is soulbound and
/// can never move. The destination is the owner's choice, funded entirely by
/// the wallet input.
///
/// Canonical shape:
///
/// ```text
/// vin 0 player state | vin 1 wallet funding
/// vout 0 player state | vout 1 LOG destination | vout 2 extension | vout 3 anchor
/// groups 0..2 PLAYER_ID | LOG | XP
/// ```
pub fn player_withdraw_covenant_script(
    log_asset: AssetId,
    xp_asset: AssetId,
    dust_sats: u64,
) -> Result<ScriptBuf> {
    if log_asset == xp_asset {
        return Err(anyhow!("LOG and XP assets must differ"));
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
    // Identity, position, luck, and the XP counter are preserved byte-for-byte.
    let builder = crate::tree::push_equal_state_packet_at(
        builder,
        PLAYER_IDENTITY_PACKET_TYPE,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
    );
    let builder = crate::tree::push_equal_state_packet_at(
        builder,
        PLAYER_POSITION_PACKET_TYPE,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
    );
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
        PLAYER_XP_PACKET_TYPE,
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
    let builder = crate::tree::push_player_renewal_group_set(builder, log_asset, xp_asset);

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
    // XP is soulbound: presence and amount are preserved exactly.
    let builder = crate::tree::push_optional_input_asset_lookup(
        builder,
        crate::protocol::WITHDRAW_STATE_INPUT_INDEX,
        xp_asset,
    );
    Ok(crate::tree::push_optional_output_asset_lookup(
        builder,
        crate::protocol::WITHDRAW_STATE_OUTPUT_INDEX,
        xp_asset,
    )
    .push_opcode(OP_ROT)
    .push_opcode(OP_EQUALVERIFY)
    .push_opcode(OP_EQUAL)
    .into_script())
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
    let log_entries = record
        .assets
        .iter()
        .filter(|asset| asset.asset_id == contract.log_asset)
        .count();
    let xp_entries = record
        .assets
        .iter()
        .filter(|asset| asset.asset_id == contract.xp_asset)
        .count();
    let canonical_assets = (1..=3).contains(&record.assets.len())
        && marker_entries == 1
        && log_entries <= 1
        && xp_entries <= 1
        && record.assets.iter().all(|asset| {
            asset.asset_id == player_asset
                || asset.asset_id == contract.log_asset
                || asset.asset_id == contract.xp_asset
        });
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

/// Build the personalized half of the atomic PLAYER/TREE chop.
///
/// The player state owns its LOG and XP inventory directly. The shared tree
/// covenant enforces the reciprocal transfers and player-luck transition.
///
/// This half proves owner continuity: the player P2TR/value, identity,
/// position, PLAYER_ID, and XP backing survive, and both tree endpoints are the
/// manifest-pinned shared contract. The tree half computes the public
/// player-bound reward and enforces the reciprocal TREE/LOG/XP and health deltas.
pub fn player_chop_covenant_script(
    log_asset: AssetId,
    xp_asset: AssetId,
    dust_sats: u64,
    tree_script: &ScriptBuf,
) -> Result<ScriptBuf> {
    if log_asset == xp_asset {
        return Err(anyhow!("LOG and XP assets must differ"));
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
    let builder = Builder::new()
        .push_opcode(op::PUSHCURRENTINPUTINDEX)
        .push_int(PLAYER_STATE_INPUT_INDEX as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMINPUTS)
        .push_int(CHOP_INPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMOUTPUTS)
        .push_int(CHOP_OUTPUT_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op::INSPECTNUMASSETGROUPS)
        .push_int(crate::protocol::CHOP_ASSET_GROUP_COUNT as i64)
        .push_opcode(OP_EQUALVERIFY)
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
        .push_opcode(OP_DROP)
        // Require the merged extension and canonical SDK anchor.
        .push_int(i64::from(CHOP_EXTENSION_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(CHOP_EXTENSION_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(-1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DROP)
        .push_int(i64::from(CHOP_ANCHOR_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_slice(anchor_program)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(i64::from(CHOP_ANCHOR_OUTPUT_INDEX))
        .push_opcode(op::INSPECTOUTPUTVALUE)
        .push_int(0)
        .push_opcode(OP_EQUALVERIFY);
    let builder = push_canonical_initial_player_luck(builder, PLAYER_STATE_INPUT_INDEX);

    let builder = push_preserved_player_packet(
        builder,
        PLAYER_IDENTITY_PACKET_TYPE,
        PLAYER_STATE_INPUT_INDEX,
    );
    let builder = push_preserved_player_packet(
        builder,
        PLAYER_POSITION_PACKET_TYPE,
        PLAYER_STATE_INPUT_INDEX,
    );
    let builder = push_advanced_player_luck(builder, PLAYER_STATE_INPUT_INDEX).push_opcode(OP_DROP);
    let builder = crate::tree::push_player_marker_group(
        builder,
        PLAYER_STATE_INPUT_INDEX,
        PLAYER_STATE_OUTPUT_INDEX,
    );
    let builder = crate::tree::push_transfer_group_shell(builder, log_asset);
    let builder = crate::tree::push_transfer_group_shell(builder, xp_asset);

    let builder = crate::tree::push_canonical_player_asset_counts(builder, log_asset, xp_asset);

    // The numeric XP packet exactly equals the conserved XP asset balance.
    let builder =
        crate::tree::push_optional_input_asset_lookup(builder, PLAYER_STATE_INPUT_INDEX, xp_asset)
            .push_opcode(OP_DROP);
    let builder =
        push_player_input_xp(builder, PLAYER_STATE_INPUT_INDEX).push_opcode(OP_EQUALVERIFY);
    let builder = crate::tree::push_optional_output_asset_lookup(
        builder,
        PLAYER_STATE_OUTPUT_INDEX,
        xp_asset,
    )
    .push_opcode(OP_DROP);
    Ok(push_xp_packet_value(builder, None)
        .push_opcode(OP_EQUAL)
        .into_script())
}

/// Atomically attach all player packets, preserving the SDK convention that
/// the final transaction output is the P2A anchor.
pub fn attach_player_state_packets(psbt: &mut Psbt, state: PlayerState) -> Result<()> {
    let mut updated = psbt.clone();
    ark_core::extension::add_packet_to_psbt(
        &mut updated,
        PLAYER_IDENTITY_PACKET_TYPE,
        &state.identity.encode(),
    )
    .context("attach player identity packet")?;
    ark_core::extension::add_packet_to_psbt(
        &mut updated,
        PLAYER_POSITION_PACKET_TYPE,
        &state.position.encode(),
    )
    .context("attach player position packet")?;
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
        PLAYER_XP_PACKET_TYPE,
        &state.xp.encode(),
    )
    .context("attach player XP packet")?;
    *psbt = updated;
    Ok(())
}

/// Decode complete player state from a transaction. No player packets means
/// `None`; a partial set is rejected rather than silently defaulted.
pub fn player_state_from_tx(tx: &Transaction) -> Result<Option<PlayerState>> {
    let [identity, position, roll, credit, xp] = player_packet_payloads(tx)?;

    match (identity, position, roll, credit, xp) {
        (None, None, None, None, None) => Ok(None),
        (Some(identity), Some(position), Some(roll), Some(credit), Some(xp)) => {
            Ok(Some(PlayerState {
                identity: PlayerIdentity::decode(identity)?,
                position: PlayerPosition::decode(position)?,
                luck: PlayerLuck {
                    roll: PlayerRoll::decode(roll)?,
                    credit: PlayerLuckCredit::decode(credit)?,
                },
                xp: PlayerXp::decode(xp)?,
            }))
        }
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
    next_state: PlayerState,
    block_height: u32,
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
    let previous_stump_height =
        crate::tree::tree_stump_height_from_tx(previous_input_txs[TREE_INPUT_INDEX])?
            .ok_or_else(|| anyhow!("previous tree transaction has no stump height"))?;
    if previous_tree_health.value() == 0 || previous_stump_height.value() != 0 {
        return Err(anyhow!("cannot chop a stump"));
    }
    let (expected_luck, success) = previous_state.luck.advance(previous_state.xp.value());
    let expected_xp = if success {
        previous_state.xp.increment()?
    } else {
        previous_state.xp
    };
    if next_state.identity != previous_state.identity
        || next_state.position != previous_state.position
        || next_state.luck != expected_luck
        || next_state.xp != expected_xp
    {
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
    let next_stump_height = if next_health.value() == 0 {
        crate::tree::TreeStumpHeight::new(u64::from(block_height))?
    } else {
        previous_stump_height
    };
    let block_witness = crate::tree::block_attestation_witness(block_height)?;
    attach_player_state_packets(&mut updated, next_state)?;
    crate::tree::attach_tree_state_packet(&mut updated, tree_state)?;
    crate::tree::attach_tree_health_packet(&mut updated, next_health)?;
    crate::tree::attach_tree_stump_height_packet(&mut updated, next_stump_height)?;
    let packet = ark_core::introspector::packet::Packet::new(vec![
        ark_core::introspector::packet::IntrospectorEntry {
            vin: PLAYER_STATE_INPUT_INDEX as u16,
            script: contract.chop_arkade_script.clone(),
            witness: bitcoin::Witness::default(),
        },
        ark_core::introspector::packet::IntrospectorEntry {
            vin: TREE_INPUT_INDEX as u16,
            script: tree_contract.chop_arkade_script.clone(),
            witness: block_witness,
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

fn player_packet_payloads(tx: &Transaction) -> Result<[Option<&[u8]>; 5]> {
    let mut packets = [None, None, None, None, None];
    for output in &tx.output {
        let Some(extension) = ark_core::extension::extension_payload(&output.script_pubkey) else {
            continue;
        };
        for (packet_type, payload) in ark_core::extension::iter_packets(extension)
            .context("decode ARK extension containing player state")?
        {
            let slot = match packet_type {
                PLAYER_IDENTITY_PACKET_TYPE => &mut packets[0],
                PLAYER_POSITION_PACKET_TYPE => &mut packets[1],
                PLAYER_ROLL_PACKET_TYPE => &mut packets[2],
                PLAYER_LUCK_CREDIT_PACKET_TYPE => &mut packets[3],
                PLAYER_XP_PACKET_TYPE => &mut packets[4],
                _ => continue,
            };
            if slot.replace(payload).is_some() {
                return Err(anyhow!("duplicate player packet type {packet_type}"));
            }
        }
    }
    Ok(packets)
}

pub(crate) fn push_preserved_player_packet(
    builder: Builder,
    packet_type: u8,
    input_index: usize,
) -> Builder {
    builder
        .push_int(packet_type.into())
        .push_int(input_index as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_int(packet_type.into())
        .push_opcode(op::INSPECTPACKET)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_EQUALVERIFY)
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

fn push_log_drop_basis_points(builder: Builder, player_input_index: usize) -> Builder {
    let builder = push_player_input_xp(builder, player_input_index)
        .push_int(1)
        .push_opcode(OP_ADD)
        .push_int(BASE_LOG_DROP_BASIS_POINTS as i64);
    let mut builder = builder;
    for threshold in LEVEL_LOG_DROP_XP_THRESHOLDS {
        builder = builder
            .push_opcode(bitcoin::opcodes::all::OP_OVER)
            .push_int(threshold as i64)
            .push_opcode(OP_GREATERTHAN)
            .push_opcode(OP_IF)
            .push_int(LEVEL_LOG_DROP_BONUS_BASIS_POINTS as i64)
            .push_opcode(OP_ADD)
            .push_opcode(OP_ENDIF);
    }
    builder
        .push_opcode(OP_TOALTSTACK)
        .push_opcode(OP_DROP)
        .push_opcode(OP_FROMALTSTACK)
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
/// Require a direct issuance-to-state activation to start from the
/// identity-derived roll and canonical credit. In that canonical shape, the
/// PLAYER_ID AssetId txid equals the state input's outpoint txid only on the
/// first recursive spend. Checking every player leaf prevents a malformed
/// direct activation from being laundered through renewal or withdrawal. It
/// cannot prove ancestry before the marker entered this covenant.
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
        .push_int(PLAYER_IDENTITY_PACKET_TYPE.into())
        .push_int(player_input_index as i64)
        .push_opcode(op::INSPECTINPUTPACKET)
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
    push_luck_credit_value(builder, Some(player_input_index))
        .push_int(INITIAL_LUCK_CREDIT as i64)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_ENDIF)
}

/// Verify and advance the player-bound roll and bounded luck credit, leaving
/// the canonical reward bit on the stack.
pub(crate) fn push_advanced_player_luck(builder: Builder, player_input_index: usize) -> Builder {
    let builder = push_player_roll_bucket(builder, player_input_index);
    let builder = push_log_drop_basis_points(builder, player_input_index)
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

fn push_xp_packet_value(builder: Builder, input_index: Option<usize>) -> Builder {
    let builder = builder.push_int(PLAYER_XP_PACKET_TYPE.into());
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
        .push_int(PLAYER_XP_LEN as i64)
        .push_opcode(OP_EQUALVERIFY)
        // Round-trip the fixed-width payload through BigNum to reject alternate
        // encodings such as negative zero before using the numeric value.
        .push_opcode(OP_DUP)
        .push_opcode(op::BIN2NUM)
        .push_opcode(OP_DUP)
        .push_int(PLAYER_XP_LEN as i64)
        .push_opcode(op::NUM2BIN)
        .push_opcode(OP_ROT)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_DUP)
        .push_int(-1)
        .push_opcode(OP_GREATERTHAN)
        .push_opcode(OP_VERIFY)
}

pub(crate) fn push_player_input_xp(builder: Builder, input_index: usize) -> Builder {
    push_xp_packet_value(builder, Some(input_index))
}
pub(crate) fn push_player_output_xp(builder: Builder) -> Builder {
    push_xp_packet_value(builder, None)
}

/// Leave the numeric `(input_xp, output_xp)` packet values on the stack.
pub(crate) fn push_player_xp_amounts(builder: Builder, input_index: usize) -> Builder {
    let builder = push_player_input_xp(builder, input_index);
    push_player_output_xp(builder)
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

    fn state(xp: u64) -> PlayerState {
        let identity = PlayerIdentity { player_id: [7; 32] };
        PlayerState {
            identity,
            position: PlayerPosition { x: 3, y: 17 },
            luck: PlayerLuck::initial(identity),
            xp: PlayerXp::new(xp),
        }
    }

    fn transition(success: bool) -> PlayerChopTransition {
        let tree_state = crate::tree::TreeState {
            tree_id: 417,
            x: 7,
            y: 13,
        };
        let mut previous_state = state(4);
        previous_state.luck.credit = PlayerLuckCredit::new(
            CHOP_ROLL_BASIS_POINTS - log_drop_basis_points(previous_state.xp.value()),
        )
        .unwrap();
        loop {
            let (next_luck, drop) = previous_state.luck.advance(previous_state.xp.value());
            if drop == success {
                let reward = u64::from(success);
                let mut next_state = previous_state;
                next_state.luck = next_luck;
                next_state.xp = PlayerXp::new(previous_state.xp.value() + reward);
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
                    state_xp_balance_before: 4,
                    state_xp_balance_after: 4 + reward,
                    tree_markers_before: 1,
                    tree_markers_after: 1,
                    tree_logs_before: 5,
                    tree_logs_after: 5 - reward,
                    tree_xp_balance_before: 5,
                    tree_xp_balance_after: 5 - reward,
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
    fn player_packet_vectors_are_stable() {
        assert_eq!(
            PlayerIdentity { player_id: [7; 32] }
                .encode()
                .to_lower_hex_string(),
            format!("504901{}", "07".repeat(32))
        );
        assert_eq!(
            PlayerPosition { x: 3, y: 17 }
                .encode()
                .to_lower_hex_string(),
            "50500103001100"
        );
        assert_eq!(
            PlayerXp::new(83).encode().to_lower_hex_string(),
            "530000000000000000"
        );
        let mut negative_zero = [0_u8; PLAYER_XP_LEN];
        negative_zero[PLAYER_XP_LEN - 1] = 0x80;
        assert!(PlayerXp::decode(&negative_zero).is_err());
    }
    #[test]
    fn player_luck_vectors_and_initial_rate_are_stable() {
        let identity = PlayerIdentity { player_id: [7; 32] };
        let initial = PlayerLuck::initial(identity);
        assert_eq!(
            initial.roll.encode().to_lower_hex_string(),
            "2f84c0ca7bc7f7af921caf1a33a8751601baff69e6bb0da963af2cf97ac54a24"
        );
        assert_eq!(
            initial.credit.encode().to_lower_hex_string(),
            "401f00000000000000"
        );
        let (next, success) = initial.advance(0);
        assert_eq!(
            next.roll.encode().to_lower_hex_string(),
            "baae20577f224870603acdea2ea14398d861dcfdac463cb0a601b028bd0b2bee"
        );
        assert_eq!(next.roll.bucket(), 5_066);
        assert_eq!(success, next.roll.bucket() < BASE_LOG_DROP_BASIS_POINTS);
        assert_eq!(
            initial.credit.value() + BASE_LOG_DROP_BASIS_POINTS,
            LUCK_WINDOW_BASIS_POINTS
        );
        assert_eq!(next.credit.value(), LUCK_WINDOW_BASIS_POINTS);

        let mut negative_zero = [0_u8; PLAYER_LUCK_CREDIT_LEN];
        negative_zero[PLAYER_LUCK_CREDIT_LEN - 1] = 0x80;
        assert!(PlayerLuckCredit::decode(&negative_zero).is_err());
        let mut over_max = [0_u8; PLAYER_LUCK_CREDIT_LEN];
        over_max[..8].copy_from_slice(&(MAX_LUCK_CREDIT + 1).to_le_bytes());
        assert!(PlayerLuckCredit::decode(&over_max).is_err());
    }

    #[test]
    fn luck_credit_bounds_long_runs_above_and_below_expectation() {
        for (index, xp) in [0, 1_154, 4_470, 13_363, 37_224, 101_333]
            .into_iter()
            .enumerate()
        {
            let identity = PlayerIdentity {
                player_id: [index as u8 + 1; 32],
            };
            let mut luck = PlayerLuck::initial(identity);
            let initial_credit = luck.credit.value();
            let rate = log_drop_basis_points(xp);
            let mut successes = 0_u64;
            let mut miss_run = 0_u64;
            let mut success_run = 0_u64;
            let mut max_miss_run = 0_u64;
            let mut max_success_run = 0_u64;
            for _ in 0..100_000 {
                let previous_credit = luck.credit.value();
                let (next, success) = luck.advance(xp);
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
            assert!(max_miss_run <= 10, "XP {xp}: miss run {max_miss_run}");
            assert!(
                max_success_run <= 2,
                "XP {xp}: success run {max_success_run}"
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
    fn successful_chop_moves_log_and_xp_into_player_state() {
        let valid = transition(true);
        valid.validate().unwrap();

        let mut missing_xp = valid;
        missing_xp.state_xp_balance_after -= 1;
        assert!(missing_xp.validate().is_err());
        let mut forged_xp = valid;
        forged_xp.next_state.xp = PlayerXp::new(6);
        assert!(forged_xp.validate().is_err());
        let mut missing_log = valid;
        missing_log.state_logs_after -= 1;
        assert!(missing_log.validate().is_err());
    }

    #[test]
    fn missed_chop_preserves_inventory_and_xp() {
        let valid = transition(false);
        valid.validate().unwrap();
        let mut forged = valid;
        forged.next_state.xp = PlayerXp::new(5);
        assert!(forged.validate().is_err());
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
            330,
            &tree_script,
        )
        .unwrap();
        assert_eq!(contract.tree_script_pubkey, tree_script);
        assert_eq!(contract.vtxo.tapscripts().len(), 5);
        assert!(contract.chop_arkade_script.len() <= 10_000);

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
    fn level_boundaries_are_exact() {
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
            assert_eq!(level_from_xp(xp), level, "XP {xp}");
        }
        assert_eq!(xp_for_level(1), Some(0));
        assert_eq!(xp_for_level(50), Some(101_333));
        assert_eq!(xp_for_level(99), Some(13_034_431));
        assert_eq!(xp_for_level(0), None);
        assert_eq!(xp_for_level(100), None);
        for (xp, basis_points) in [
            (0, 2_000),
            (1_153, 2_000),
            (1_154, 2_200),
            (4_469, 2_200),
            (4_470, 2_400),
            (13_362, 2_400),
            (13_363, 2_600),
            (37_223, 2_600),
            (37_224, 2_800),
            (101_332, 2_800),
            (101_333, 3_000),
            (u64::MAX, 3_000),
        ] {
            assert_eq!(log_drop_basis_points(xp), basis_points, "XP {xp}");
        }
    }

    #[test]
    fn withdraw_script_moves_log_only_and_keeps_xp_soulbound() {
        let withdraw = player_withdraw_covenant_script(asset(2, 1), asset(2, 2), 330).unwrap();
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
    fn player_contract_rejects_tweaked_emulator_collision() {
        let secp = Secp256k1::new();
        let operator = xonly(&secp, 4);
        let emulator = xonly(&secp, 5);
        let rollover = xonly(&secp, 7);
        let tree_script =
            Address::p2tr(&secp, xonly(&secp, 6), None, Network::Regtest).script_pubkey();
        let (tree_asset, log_asset, xp_asset) = (asset(2, 0), asset(2, 1), asset(2, 2));
        let chop = player_chop_covenant_script(log_asset, xp_asset, 330, &tree_script).unwrap();
        let renewal = player_renewal_covenant_script(log_asset, xp_asset, 330).unwrap();
        // An owner key equal to a script-tweaked emulator key could satisfy
        // the emulator position without executing the covenant.
        for script in [&chop, &renewal] {
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
