//! woodland.sh recursive Arkade covenants and verified client services.

/// Transport, manifest, and transaction plumbing behind the public covenant
/// APIs. Downstream clients (bots, alternative browsers, tooling) build on
/// these concrete types rather than reimplementing them.
pub mod arkade;
mod asset_packet;
mod keys;
#[cfg_attr(
    all(target_arch = "wasm32", not(feature = "regtest-e2e")),
    allow(dead_code)
)]
pub mod txbuild;

pub mod player;
mod player_template;
pub mod protocol;
pub mod renewal;
pub mod tree;

#[cfg(all(test, feature = "woodland-app", not(target_arch = "wasm32")))]
mod covenant_tests;

pub mod batch;

#[cfg(feature = "woodland-app")]
pub mod chop;

#[cfg(feature = "fuzzing")]
pub mod fuzzing;

pub mod world;

#[cfg_attr(
    all(target_arch = "wasm32", not(feature = "regtest-e2e")),
    allow(dead_code)
)]
#[cfg(all(feature = "woodland-app", target_arch = "wasm32"))]
mod web_app;

#[cfg(all(feature = "woodland-app", not(target_arch = "wasm32")))]
pub mod client;
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
