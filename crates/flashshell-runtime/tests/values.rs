#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::sync::Arc;

use flashshell_runtime::{
    ByteSize, Duration, FiniteFloat, NativePath, Record, Signal, Status, Table, TableError, Value,
};

#[test]
fn finite_floats_are_normalized_and_compare_exactly_with_integers() {
    assert!(FiniteFloat::new(f64::NAN).is_err());
    assert!(FiniteFloat::new(f64::INFINITY).is_err());
    assert!(FiniteFloat::new(f64::NEG_INFINITY).is_err());

    let positive_zero = FiniteFloat::new(0.0).unwrap();
    let negative_zero = FiniteFloat::new(-0.0).unwrap();
    assert_eq!(positive_zero, negative_zero);
    assert_eq!(positive_zero.get().to_bits(), 0.0_f64.to_bits());
    assert_eq!(format!("{:?}", Value::from(negative_zero)), "0.0");

    assert_eq!(Value::Int(42), Value::from(FiniteFloat::new(42.0).unwrap()));
    assert_ne!(Value::Int(42), Value::from(FiniteFloat::new(42.5).unwrap()));
    assert_eq!(
        Value::Int(9_007_199_254_740_992),
        Value::from(FiniteFloat::new(9_007_199_254_740_992.0).unwrap())
    );
    assert_ne!(
        Value::Int(9_007_199_254_740_993),
        Value::from(FiniteFloat::new(9_007_199_254_740_992.0).unwrap())
    );
    assert_eq!(
        Value::Int(i64::MIN),
        Value::from(FiniteFloat::new(i64::MIN as f64).unwrap())
    );
    assert_ne!(
        Value::Int(i64::MAX),
        Value::from(FiniteFloat::new(i64::MAX as f64).unwrap())
    );
}

#[test]
fn lists_and_records_are_immutable_ordered_values() {
    let source = vec![Value::Int(1), Value::string("two")];
    let list = Value::list(source.clone());
    drop(source);
    assert_eq!(
        list.as_list().unwrap(),
        &[Value::Int(1), Value::string("two")]
    );

    let record = Record::new(vec![
        ("first".to_owned(), Value::Int(1)),
        ("second".to_owned(), Value::Bool(true)),
    ])
    .unwrap();
    assert_eq!(record.get("first"), Some(&Value::Int(1)));
    assert_eq!(
        record
            .entries()
            .iter()
            .map(|(key, _)| key.as_ref())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        format!("{:?}", Value::from(record.clone())),
        r#"{"first": 1, "second": true}"#
    );

    let duplicate = Record::new(vec![
        ("name".to_owned(), Value::Int(1)),
        ("name".to_owned(), Value::Int(2)),
    ])
    .unwrap_err();
    assert_eq!(duplicate.key(), "name");
    assert_eq!(duplicate.index(), 1);

    let reversed = Record::new(vec![
        ("second".to_owned(), Value::Bool(true)),
        ("first".to_owned(), Value::Int(1)),
    ])
    .unwrap();
    assert_ne!(record, reversed);
}

#[test]
fn statuses_enforce_leaf_and_aggregate_invariants() {
    let success = Status::exit(0, Duration::from_nanos(12)).unwrap();
    let failure = Status::exit(7, Duration::from_nanos(20)).unwrap();
    let signal = Signal::new(Some(9), Some("SIGKILL".to_owned())).unwrap();
    let signaled = Status::signaled(signal, Duration::from_nanos(3)).unwrap();

    assert!(success.is_ok());
    assert!(!failure.is_ok());
    assert!(!signaled.is_ok());
    assert_eq!(format!("{success}"), "success");
    assert_eq!(format!("{failure}"), "exit 7");
    assert_eq!(format!("{signaled}"), "signal SIGKILL (9)");
    assert_eq!(
        format!("{success:?}"),
        "status(code: 0, signal: null, stages: [], duration: 12ns)"
    );
    assert!(Signal::new(None, None).is_err());
    assert!(Status::exit(0, Duration::from_nanos(-1)).is_err());

    let aggregate = Status::aggregate(
        vec![success.clone(), failure.clone(), signaled.clone()],
        1,
        Duration::from_nanos(50),
    )
    .unwrap();
    assert_eq!(aggregate.code(), Some(7));
    assert_eq!(aggregate.signal(), None);
    assert_eq!(aggregate.stages(), &[success, failure, signaled]);
    assert_eq!(aggregate.duration(), Duration::from_nanos(50));
    assert!(!aggregate.is_ok());
    assert!(Status::aggregate(Vec::new(), 0, Duration::ZERO).is_err());
    assert!(
        Status::aggregate(
            vec![Status::exit(0, Duration::ZERO).unwrap()],
            1,
            Duration::ZERO
        )
        .is_err()
    );
    assert!(Status::aggregate(vec![aggregate], 0, Duration::from_nanos(1)).is_err());
}

#[test]
fn debug_and_display_forms_are_deterministic_and_type_revealing() {
    assert_forms(Value::Null, "null", "null");
    assert_forms(Value::Bool(true), "true", "true");
    assert_forms(Value::Int(-12), "-12", "-12");
    assert_forms(Value::from(FiniteFloat::new(3.0).unwrap()), "3.0", "3.0");
    assert_forms(Value::string("line\n\""), r#""line\n\"""#, "line\n\"");
    assert_forms(
        Value::bytes([b'A', b'Z', 0, 0xff, b'\\', b'"']),
        r#"bytes"AZ\x00\xFF\\\"""#,
        r#"AZ\x00\xFF\\\""#,
    );
    assert_forms(
        Value::from(Duration::from_nanos(-5)),
        "duration(-5ns)",
        "-5ns",
    );
    assert_forms(Value::from(ByteSize::new(42)), "size(42b)", "42b");
    assert_forms(
        Value::list(vec![Value::Int(1), Value::string("x")]),
        r#"[1, "x"]"#,
        r#"[1, "x"]"#,
    );
}

#[cfg(unix)]
#[test]
fn paths_preserve_and_escape_native_unix_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let path = NativePath::new(OsString::from_vec(b"/tmp/\xff".to_vec()));
    assert_eq!(path.as_os_str().as_encoded_bytes(), b"/tmp/\xff");
    assert_forms(Value::from(path), r#"path"/tmp/\xFF""#, r#"/tmp/\xFF"#);
}

#[test]
fn tables_are_rectangular_with_unique_ordered_columns() {
    let table = Table::new(
        vec!["name".to_owned(), "size".to_owned()],
        vec![
            vec![Value::string("a"), Value::Int(1)],
            vec![Value::string("b"), Value::Int(2)],
        ],
    )
    .unwrap();

    assert_eq!(
        table
            .columns()
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>(),
        ["name", "size"]
    );
    assert_eq!(table.rows().len(), 2);
    assert_eq!(table.column_index("size"), Some(1));
    assert_eq!(table.column_index("absent"), None);
    assert_eq!(table.get(1, "name"), Some(&Value::string("b")));
    assert_eq!(table.get(2, "name"), None);
    assert_eq!(table.get(0, "absent"), None);

    assert_forms(
        Value::from(table),
        r#"table(columns: ["name", "size"], rows: [["a", 1], ["b", 2]])"#,
        r#"table(columns: ["name", "size"], rows: [["a", 1], ["b", 2]])"#,
    );

    // A table with no rows still carries its declared columns, so an empty
    // result keeps its shape instead of collapsing to an empty list.
    let empty = Table::new(vec!["only".to_owned()], Vec::new()).unwrap();
    assert_eq!(empty.rows(), &[] as &[Arc<[Value]>]);
    assert_eq!(
        format!("{:?}", Value::from(empty)),
        r#"table(columns: ["only"], rows: [])"#
    );

    // A table with neither columns nor rows is legal and remains rectangular.
    let nothing = Table::new(Vec::new(), Vec::new()).unwrap();
    assert_eq!(
        format!("{:?}", Value::from(nothing)),
        "table(columns: [], rows: [])"
    );
}

#[test]
fn table_construction_rejects_duplicate_columns_and_ragged_rows() {
    let duplicate = Table::new(vec!["name".to_owned(), "name".to_owned()], Vec::new()).unwrap_err();
    assert_eq!(
        duplicate,
        TableError::DuplicateColumn {
            column: "name".to_owned(),
            index: 1,
        }
    );

    let short = Table::new(
        vec!["a".to_owned(), "b".to_owned()],
        vec![vec![Value::Int(1)]],
    )
    .unwrap_err();
    assert_eq!(
        short,
        TableError::RowWidth {
            row: 0,
            expected: 2,
            actual: 1,
        }
    );

    let long = Table::new(
        vec!["a".to_owned()],
        vec![vec![Value::Int(1)], vec![Value::Int(1), Value::Int(2)]],
    )
    .unwrap_err();
    assert_eq!(
        long,
        TableError::RowWidth {
            row: 1,
            expected: 1,
            actual: 2,
        }
    );
}

#[test]
fn table_equality_compares_ordered_columns_and_every_cell_recursively() {
    let columns = vec!["first".to_owned(), "second".to_owned()];
    let rows = vec![vec![Value::Int(1), Value::list(vec![Value::Int(2)])]];
    let table = Table::new(columns.clone(), rows.clone()).unwrap();

    assert_eq!(table, Table::new(columns.clone(), rows.clone()).unwrap());

    // Column order is observable, exactly as record field order is.
    let reordered = Table::new(
        vec!["second".to_owned(), "first".to_owned()],
        vec![vec![Value::list(vec![Value::Int(2)]), Value::Int(1)]],
    )
    .unwrap();
    assert_ne!(table, reordered);

    // Cells compare recursively, so a nested difference is a table difference.
    let nested = Table::new(
        columns.clone(),
        vec![vec![Value::Int(1), Value::list(vec![Value::Int(3)])]],
    )
    .unwrap();
    assert_ne!(table, nested);

    // The numeric equality domain reaches into cells like everywhere else.
    let promoted = Table::new(
        columns,
        vec![vec![
            Value::from(FiniteFloat::new(1.0).unwrap()),
            Value::list(vec![Value::Int(2)]),
        ]],
    )
    .unwrap();
    assert_eq!(table, promoted);
}

#[test]
fn tables_built_from_records_fill_absent_fields_with_explicit_nulls() {
    let first = Record::new(vec![
        ("name".to_owned(), Value::string("a")),
        ("size".to_owned(), Value::Int(1)),
    ])
    .unwrap();
    let second = Record::new(vec![
        ("size".to_owned(), Value::Int(2)),
        ("mode".to_owned(), Value::string("rw")),
    ])
    .unwrap();

    let table = Table::from_records([first, second]);

    // Columns are the union in first-seen order across the records — not sorted,
    // and not the keys of the first record alone.
    assert_eq!(
        table
            .columns()
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>(),
        ["name", "size", "mode"]
    );
    assert_eq!(
        format!("{:?}", Value::from(table)),
        r#"table(columns: ["name", "size", "mode"], rows: [["a", 1, null], [null, 2, "rw"]])"#
    );

    // No records means no columns and no rows, not a one-row empty table.
    let empty = Table::from_records([]);
    assert!(empty.columns().is_empty());
    assert!(empty.rows().is_empty());
}

fn assert_forms(value: Value, debug: &str, display: &str) {
    assert_eq!(format!("{value:?}"), debug);
    assert_eq!(value.to_string(), display);
}
