//! Arbitrary bytes as a chunk, the first byte choosing the compatibility setting.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (compat, source) = lunita_fuzz::split_compat(data);
    lunita_fuzz::run(source, compat);
});
