//! Lua programs generated from the bytes (see `piccolo_fuzz::grammar`), the first byte choosing
//! the compatibility setting.

#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use piccolo::Compat;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let compat = match u.arbitrary::<u8>() {
        Ok(b) if b & 1 == 1 => Compat::Lua51,
        _ => Compat::Default,
    };
    if let Ok(source) = piccolo_fuzz::grammar::program(&mut u, piccolo_fuzz::seeds()) {
        piccolo_fuzz::run(source.as_bytes(), compat);
    }
});
