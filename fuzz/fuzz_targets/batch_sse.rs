#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| woodland::fuzzing::batch_sse(data));
