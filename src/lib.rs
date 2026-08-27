//! woodland.sh recursive Arkade covenants and verified client services.

#[cfg_attr(
    all(target_arch = "wasm32", not(feature = "regtest-e2e")),
    allow(dead_code)
)]
mod arkade;
mod asset_packet;
mod keys;
#[cfg_attr(
    all(target_arch = "wasm32", not(feature = "regtest-e2e")),
    allow(dead_code)
)]
mod txbuild;

pub mod player;
pub mod protocol;
pub mod renewal;
pub mod tree;

pub mod batch;

#[cfg(all(feature = "woodland-app", any(target_arch = "wasm32", test)))]
mod chop;

#[cfg(feature = "fuzzing")]
pub mod fuzzing;

#[cfg(feature = "woodland-app")]
mod world;

#[cfg_attr(
    all(target_arch = "wasm32", not(feature = "regtest-e2e")),
    allow(dead_code)
)]
#[cfg(all(feature = "woodland-app", target_arch = "wasm32"))]
mod web_app;

#[cfg(all(feature = "woodland-app", not(target_arch = "wasm32")))]
pub mod operator;

#[cfg(all(feature = "server", not(target_arch = "wasm32")))]
pub mod server;

#[cfg(all(feature = "woodland-app", not(target_arch = "wasm32")))]
mod watchtower;

pub use keys::Keys;

#[cfg(all(feature = "woodland-app", not(target_arch = "wasm32")))]
pub(crate) const REGTEST_SERVER: &str = "http://127.0.0.1:7070";

#[cfg(all(feature = "woodland-app", not(target_arch = "wasm32")))]
pub(crate) const REGTEST_EMULATOR: &str = "http://127.0.0.1:7073";
