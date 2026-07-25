#![forbid(unsafe_code)]

use flashshell_syntax::{
    Diagnostic, LabelStyle, Severity, SourceFile, SourceId, SpanError, render_diagnostic,
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
