//! Acceptance coverage for width-aware terminal table presentation.
//!
//! Presentation consumes an already-materialized table. It is deliberately
//! separate from `Value::Display`, format serialization, and terminal I/O.

use flashshell_runtime::presentation::render_table;
use flashshell_runtime::{ByteSize, Duration, FiniteFloat, Record, Table, Value};
use unicode_width::UnicodeWidthStr;

fn table(columns: &[&str], rows: Vec<Vec<Value>>) -> Table {
    Table::new(
        columns.iter().map(|column| (*column).to_owned()).collect(),
        rows,
    )
    .expect("test table should be rectangular")
}

#[test]
fn renders_a_stable_frame_with_per_cell_alignment() {
    let input = table(
        &["name", "count", "size"],
        vec![
            vec![
                Value::string("alpha"),
                Value::Int(7),
                Value::from(ByteSize::new(12)),
            ],
            vec![
                Value::string("b"),
                Value::Int(1_024),
                Value::from(ByteSize::new(3)),
            ],
        ],
    );

    assert_eq!(
        render_table(&input, 80),
        "name  | count | size\n------+-------+-----\nalpha |     7 |  12b\nb     |  1024 |   3b"
    );
}

#[test]
fn aligns_every_numeric_and_unit_family_to_the_right() {
    let input = table(
        &["int", "float", "duration", "bytes"],
        vec![vec![
            Value::Int(-2),
            Value::from(FiniteFloat::new(1.5).expect("finite")),
            Value::from(Duration::from_nanos(9)),
            Value::from(ByteSize::new(4)),
        ]],
    );

    assert_eq!(
        render_table(&input, 80),
        "int | float | duration | bytes\n----+-------+----------+------\n -2 |   1.5 |      9ns |    4b"
    );
}

#[test]
fn unicode_terminal_width_controls_padding() {
    let input = table(
        &["word", "n"],
        vec![
            vec![Value::string("界"), Value::Int(1)],
            vec![Value::string("e\u{301}"), Value::Int(22)],
        ],
    );
    let rendered = render_table(&input, 80);

    assert_eq!(
        rendered,
        "word | n \n-----+---\n界   |  1\ne\u{301}    | 22"
    );
    for line in rendered.lines() {
        assert_eq!(UnicodeWidthStr::width(line), 9);
    }
}

#[test]
fn width_pressure_is_balanced_and_every_line_fits() {
    let input = table(
        &["first", "second", "third"],
        vec![vec![
            Value::string("abcdefgh"),
            Value::string("ijklmnop"),
            Value::string("qrstuvwx"),
        ]],
    );
    let rendered = render_table(&input, 18);

    assert_eq!(
        rendered,
        "fir… | sec… | thi…\n-----+------+-----\nabc… | ijk… | qrs…"
    );
    assert!(
        rendered
            .lines()
            .all(|line| UnicodeWidthStr::width(line) <= 18),
        "{rendered:?}"
    );
}

#[test]
fn controls_cannot_inject_rows_or_terminal_sequences() {
    let input = table(
        &["value"],
        vec![vec![Value::string("a\nb\r\t\0\u{1b}[31m")]],
    );
    let rendered = render_table(&input, 80);

    assert_eq!(
        rendered,
        "value               \n--------------------\na\\nb\\r\\t\\0\\u{1B}[31m"
    );
    assert_eq!(rendered.lines().count(), 3);
    assert!(!rendered.contains('\u{1b}'));
}

#[test]
fn compound_cells_use_deterministic_value_display_without_serializing() {
    let record = Record::new(vec![
        ("x".to_owned(), Value::Int(1)),
        ("y".to_owned(), Value::string("two")),
    ])
    .expect("record keys are unique");
    let input = table(
        &["value"],
        vec![
            vec![Value::from(record)],
            vec![Value::list(vec![Value::Null])],
        ],
    );

    assert_eq!(
        render_table(&input, 80),
        "value               \n--------------------\n{\"x\": 1, \"y\": \"two\"}\n[null]              "
    );
}

#[test]
fn empty_shapes_and_structurally_impossible_widths_are_explicit() {
    let shaped_empty = table(&["name", "size"], Vec::new());
    assert_eq!(render_table(&shaped_empty, 80), "name | size\n-----+-----");

    let no_columns = table(&[], vec![vec![]]);
    assert_eq!(render_table(&no_columns, 80), "(empty table)");
    assert_eq!(render_table(&no_columns, 5), "(emp…");

    let too_narrow = table(&["a", "b"], vec![vec![Value::Int(1), Value::Int(2)]]);
    assert_eq!(render_table(&too_narrow, 4), "(ta…");
    assert_eq!(render_table(&too_narrow, 1), "…");
    assert_eq!(render_table(&too_narrow, 0), "");
}

#[test]
fn rendering_is_read_only_and_has_no_trailing_newline() {
    let input = table(&["x"], vec![vec![Value::Int(1)]]);
    let before = input.clone();
    let rendered = render_table(&input, 80);

    assert_eq!(input, before);
    assert_eq!(rendered, "x\n-\n1");
    assert!(!rendered.ends_with('\n'));
}
