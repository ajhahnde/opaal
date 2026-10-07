#![no_main]

use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};
use libfuzzer_sys::fuzz_target;

#[path = "../../crates/opaal-syntax/tests/support/formatting.rs"]
mod formatting;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = SourceFile::from_bytes(SourceId::new(0), "fuzz", data.to_vec()) else {
        return;
    };
    if matches!(parse_opaal(&source), ParseOutcome::Complete(_)) {
        formatting::assert_roundtrip(&source);
    }
});
