//! Lua programs generated from the bytes (see `lunita_fuzz::grammar`), the first byte choosing
//! the compatibility setting.

#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use lunita::Compat;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let compat = match u.arbitrary::<u8>() {
        Ok(b) if b & 1 == 1 => Compat::Lua51,
        _ => Compat::Default,
    };
    if let Ok(source) = lunita_fuzz::grammar::program(&mut u, lunita_fuzz::seeds()) {
        lunita_fuzz::run(source.as_bytes(), compat);
    }
});
