//! Parser entrypoints used only by `cargo fuzz` targets.

pub fn asset_packet(data: &[u8]) {
    crate::asset_packet::fuzz_payload(data);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn batch_sse(data: &[u8]) {
    crate::batch::fuzz_sse_frame(data);
}
