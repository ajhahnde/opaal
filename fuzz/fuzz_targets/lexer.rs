#![no_main]

use flash_syntax::{SourceFile, SourceId, classify_tokens, lex, lex_v2};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = SourceFile::from_bytes(SourceId::new(0), "fuzz", data.to_vec()) else {
        return;
    };
    for tokens in [lex(&source), lex_v2(&source)] {
        let mut next_start = 0;
        for token in &tokens {
            let span = token.span();
            assert!(!span.is_empty());
            assert_eq!(span.start(), next_start);
            token.text(&source).unwrap();
            next_start = span.end();
        }
        assert_eq!(next_start, source.len());
        classify_tokens(&source, &tokens).unwrap();
    }
});
