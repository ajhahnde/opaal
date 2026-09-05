#![no_main]

use flash_syntax::{SourceFile, SourceId, parse, parse_v2, parse_v2_submission};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = SourceFile::from_bytes(SourceId::new(0), "fuzz", data.to_vec()) else {
        return;
    };
    let _ = parse(&source);
    let _ = parse_v2(&source);
    let _ = parse_v2_submission(&source);
});
