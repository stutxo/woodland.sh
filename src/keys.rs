//! Browser-held Arkade identity.
//!
//! The key type is storage-agnostic. The WASM API accepts an existing secret,
//! while the browser persists one wallet per local Arkade server.

use anyhow::{anyhow, Context, Result};
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::Keypair;
use bitcoin::secp256k1::Message;
use bitcoin::secp256k1::{schnorr, All};
use bitcoin::XOnlyPublicKey;

#[derive(Clone)]
pub struct Keys {
    pub secp: Secp256k1<All>,
    pub keypair: Keypair,
}

impl Keys {
    pub fn generate() -> Result<Self> {
        let mut rng = rand::rngs::OsRng;
        let secp = Secp256k1::new();
        Ok(Self {
            keypair: Keypair::new(&secp, &mut rng),
            secp,
        })
    }

    pub fn from_hex(secret_hex: &str) -> Result<Self> {
        let bytes: [u8; 32] = bitcoin::hex::FromHex::from_hex(secret_hex)
            .map_err(|e| anyhow!("invalid key hex: {e}"))?;
        let secp = Secp256k1::new();
        let keypair =
            Keypair::from_seckey_slice(&secp, &bytes).context("invalid secret key bytes")?;
        Ok(Self { secp, keypair })
    }

    pub fn secret_hex(&self) -> String {
        use bitcoin::hex::DisplayHex;
        self.keypair
            .secret_key()
            .secret_bytes()
            .to_lower_hex_string()
    }

    pub fn owner_pk(&self) -> XOnlyPublicKey {
        self.keypair.x_only_public_key().0
    }

    /// Sign a taproot script-path sighash. Mirrors the SDK client's sign fn:
    /// one schnorr signature over the message, tagged with our x-only pubkey.
    /// Auxiliary randomness hedges the deterministic nonce against fault and
    /// side-channel nonce-recovery on long-lived online signers.
    pub fn sign_msg(&self, msg: &Message) -> Vec<(schnorr::Signature, XOnlyPublicKey)> {
        let mut aux_rand = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut aux_rand);
        let sig = self
            .secp
            .sign_schnorr_with_aux_rand(msg, &self.keypair, &aux_rand);
        vec![(sig, self.owner_pk())]
    }
}
