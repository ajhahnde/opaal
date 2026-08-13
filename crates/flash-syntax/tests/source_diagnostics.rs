#![forbid(unsafe_code)]

use flash_syntax::{
    Diagnostic, LabelStyle, PositionEncoding, PositionError, Severity, SourceFile, SourceId,
    SpanError, TextPosition, TextRange, render_diagnostic, render_diagnostic_sources,
};

#[test]
fn source_spans_and_locations_use_original_utf8_bytes() {
    let source = SourceFile::new(SourceId::new(7), "examples/demo.fsh", "first\r\né bad\n");

    assert_eq!(source.id(), SourceId::new(7));
    assert_eq!(source.name(), "examples/demo.fsh");
    assert_eq!(source.text(), "first\r\né bad\n");
    assert_eq!(source.line_index().line_count(), 3);

    let bad = source.span(10..13).expect("bad should be a valid span");
    assert_eq!(source.slice(bad).unwrap(), "bad");
    assert_eq!(source.location(bad.start()).unwrap().line(), 2);
    assert_eq!(source.location(bad.start()).unwrap().column(), 3);

    let cr = source.location(5).unwrap();
    let lf = source.location(6).unwrap();
    assert_eq!((cr.line(), cr.column()), (1, 6));
    assert_eq!((lf.line(), lf.column()), (1, 6));
    assert_eq!(
        (
            source.location(source.len()).unwrap().line(),
            source.location(source.len()).unwrap().column(),
        ),
        (3, 1)
    );

    #[allow(
        clippy::reversed_empty_ranges,
        reason = "the test intentionally passes a malformed byte range"
    )]
    let reversed = 4..3;
    assert!(matches!(
        source.span(reversed),
        Err(SpanError::Reversed { .. })
    ));
    assert!(matches!(
        source.span(0..source.len() + 1),
        Err(SpanError::OutOfBounds { .. })
    ));
    assert!(matches!(
        source.span(8..8),
        Err(SpanError::NotCharBoundary { offset: 8 })
    ));

    let other = SourceFile::new(SourceId::new(8), "other.fsh", "bad");
    assert!(matches!(
        source.slice(other.span(0..3).unwrap()),
        Err(SpanError::WrongSource { .. })
    ));
}

#[test]
fn byte_loading_rejects_non_utf8_source() {
    assert!(SourceFile::from_bytes(SourceId::new(1), "bad.fsh", vec![0xff]).is_err());
}

#[test]
fn protocol_positions_are_checked_in_utf8_and_utf16_code_units() {
    let source = SourceFile::new(SourceId::new(2), "unicode.fsh", "aé💡\r\nnext\n");

    let cases = [
        (0, 0, TextPosition::new(0, 0), TextPosition::new(0, 0)),
        (1, 1, TextPosition::new(0, 1), TextPosition::new(0, 1)),
        (3, 3, TextPosition::new(0, 3), TextPosition::new(0, 2)),
        (7, 7, TextPosition::new(0, 7), TextPosition::new(0, 4)),
        (8, 7, TextPosition::new(0, 7), TextPosition::new(0, 4)),
        (9, 9, TextPosition::new(1, 0), TextPosition::new(1, 0)),
        (13, 13, TextPosition::new(1, 4), TextPosition::new(1, 4)),
        (14, 14, TextPosition::new(2, 0), TextPosition::new(2, 0)),
    ];

    for (offset, canonical_offset, utf8, utf16) in cases {
        assert_eq!(
            source.text_position(offset, PositionEncoding::Utf8),
            Ok(utf8)
        );
        assert_eq!(
            source.text_position(offset, PositionEncoding::Utf16),
            Ok(utf16)
        );
        assert_eq!(
            source.byte_offset(utf8, PositionEncoding::Utf8),
            Ok(canonical_offset)
        );
        assert_eq!(
            source.byte_offset(utf16, PositionEncoding::Utf16),
            Ok(canonical_offset)
        );
    }

    let span = source.span(1..7).unwrap();
    assert_eq!(
        source.text_range(span, PositionEncoding::Utf16),
        Ok(TextRange::new(
            TextPosition::new(0, 1),
            TextPosition::new(0, 4)
        ))
    );
}

#[test]
fn protocol_positions_reject_partial_scalars_surrogates_and_out_of_range_values() {
    let source = SourceFile::new(SourceId::new(3), "unicode.fsh", "aé💡\r\n");

    assert!(matches!(
        source.text_position(2, PositionEncoding::Utf8),
        Err(PositionError::NotUtf8Boundary { .. })
    ));
    assert_eq!(
        source.text_position(8, PositionEncoding::Utf16),
        Ok(TextPosition::new(0, 4))
    );
    assert!(matches!(
        source.byte_offset(TextPosition::new(0, 2), PositionEncoding::Utf8),
        Err(PositionError::NotUtf8Boundary { .. })
    ));
    assert!(matches!(
        source.byte_offset(TextPosition::new(0, 3), PositionEncoding::Utf16),
        Err(PositionError::InsideUtf16Scalar { .. })
    ));
    assert!(matches!(
        source.byte_offset(TextPosition::new(0, 5), PositionEncoding::Utf16),
        Err(PositionError::CharacterOutOfBounds { .. })
    ));
    assert!(matches!(
        source.byte_offset(TextPosition::new(2, 0), PositionEncoding::Utf8),
        Err(PositionError::LineOutOfBounds { .. })
    ));
    assert!(matches!(
        source.text_position(source.len() + 1, PositionEncoding::Utf8),
        Err(PositionError::ByteOutOfBounds { .. })
    ));
}

#[test]
fn diagnostics_render_ordered_labels_notes_and_empty_spans() {
    let source = SourceFile::new(SourceId::new(3), "examples/demo.fsh", "first\r\né bad\n");
    let primary = source.span(10..13).unwrap();
    let context = source.span(0..5).unwrap();
    let insertion = source.span(source.len()..source.len()).unwrap();

    let diagnostic = Diagnostic::new(Severity::Error, "FS1001", "unexpected word")
        .with_primary(primary, "not valid here")
        .with_secondary(context, "statement starts here")
        .with_secondary(insertion, "expected input here")
        .with_note("use a quoted word");

    assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
    assert_eq!(diagnostic.labels()[1].style(), LabelStyle::Secondary);
    assert_eq!(diagnostic.notes(), ["use a quoted word"]);

    assert_eq!(
        render_diagnostic(&source, &diagnostic).unwrap(),
        concat!(
            "error[FS1001]: unexpected word\n",
            " --> examples/demo.fsh:2:3\n",
            "  |\n",
            "2 | é bad\n",
            "  |   ^^^ not valid here\n",
            "1 | first\n",
            "  | ----- statement starts here\n",
            "3 | \n",
            "  | - expected input here\n",
            "  = note: use a quoted word\n",
        )
    );
}

#[test]
fn diagnostic_rendering_rejects_a_label_from_another_source() {
    let source = SourceFile::new(SourceId::new(1), "one.fsh", "one");
    let other = SourceFile::new(SourceId::new(2), "two.fsh", "two");
    let diagnostic = Diagnostic::new(Severity::Warning, "FS2001", "cross-source label")
        .with_primary(other.span(0..3).unwrap(), "from two");

    assert!(render_diagnostic(&source, &diagnostic).is_err());
}

#[test]
fn diagnostics_group_labels_from_multiple_sources() {
    let root = SourceFile::new(SourceId::new(1), "root.fsh", "import './lib.fsh'\n");
    let library = SourceFile::new(SourceId::new(2), "lib.fsh", "import './root.fsh'\n");
    let diagnostic = Diagnostic::new(Severity::Error, "MOD002", "module import cycle")
        .with_primary(library.span(7..19).unwrap(), "this import closes the cycle")
        .with_secondary(
            root.span(7..18).unwrap(),
            "cycle continues through this import",
        );

    assert_eq!(
        render_diagnostic_sources([&root, &library], &diagnostic).unwrap(),
        concat!(
            "error[MOD002]: module import cycle\n",
            " --> lib.fsh:1:8\n",
            "  |\n",
            "1 | import './root.fsh'\n",
            "  |        ^^^^^^^^^^^^ this import closes the cycle\n",
            " ::: root.fsh:1:8\n",
            "  |\n",
            "1 | import './lib.fsh'\n",
            "  |        ----------- cycle continues through this import\n",
        )
    );
}
