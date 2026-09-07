#![no_main]

use opaal_syntax::{SourceFile, SourceId, parse_opaal};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = SourceFile::from_bytes(SourceId::new(0), "fuzz", data.to_vec()) else {
        return;
    };
    let _ = parse_opaal(&source);
});
