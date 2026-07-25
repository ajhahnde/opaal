#![forbid(unsafe_code)]

use flashshell_runtime::operation::{
    self, OperationError, add, divide, equal, field, greater, greater_equal, index, less,
    less_equal, member, multiply, negate, not_equal, plus, range, remainder, subtract, to_float,
    to_int,
};
use flashshell_runtime::{FiniteFloat, Range, Record, Value};

fn int(value: i64) -> Value {
    Value::Int(value)
}

fn float(value: f64) -> Value {
    Value::from(FiniteFloat::new(value).unwrap())
}

#[test]
fn arithmetic_is_checked_floored_and_type_promoting() {
    assert_eq!(add(&int(2), &int(3)).unwrap(), int(5));
    assert_eq!(subtract(&int(2), &int(5)).unwrap(), int(-3));
    assert_eq!(multiply(&int(6), &int(7)).unwrap(), int(42));

    // Floored division and divisor-signed remainder: (a / b) * b + (a % b) == a.
    assert_eq!(divide(&int(-7), &int(2)).unwrap(), int(-4));
    assert_eq!(remainder(&int(-7), &int(2)).unwrap(), int(1));
    assert_eq!(divide(&int(7), &int(-2)).unwrap(), int(-4));
    assert_eq!(remainder(&int(7), &int(-2)).unwrap(), int(-1));
    for (a, b) in [(-7, 2), (7, -2), (-7, -2), (7, 2), (0, 3), (-1, 3)] {
        let q = divide(&int(a), &int(b)).unwrap();
        let r = remainder(&int(a), &int(b)).unwrap();
        assert_eq!(add(&multiply(&q, &int(b)).unwrap(), &r).unwrap(), int(a));
    }

    // Mixed operands promote to Float; two Ints stay Int.
    assert_eq!(add(&int(1), &float(0.5)).unwrap(), float(1.5));
    assert_eq!(divide(&int(3), &int(2)).unwrap(), int(1));
    assert_eq!(divide(&float(3.0), &float(2.0)).unwrap(), float(1.5));

    // Unary forms.
    assert_eq!(negate(&int(4)).unwrap(), int(-4));
    assert_eq!(negate(&float(2.5)).unwrap(), float(-2.5));
    assert_eq!(plus(&int(4)).unwrap(), int(4));

    // Overflow, zero division, and non-numeric operands are distinct errors.
    assert!(matches!(
        add(&int(i64::MAX), &int(1)),
        Err(OperationError::IntegerOverflow { .. })
    ));
    assert!(matches!(
        negate(&int(i64::MIN)),
        Err(OperationError::IntegerOverflow { .. })
    ));
    assert!(matches!(
        divide(&int(1), &int(0)),
        Err(OperationError::DivisionByZero { .. })
    ));
    assert!(matches!(
        remainder(&int(1), &int(0)),
        Err(OperationError::DivisionByZero { .. })
    ));
    assert!(matches!(
        divide(&float(1.0), &float(0.0)),
        Err(OperationError::DivisionByZero { .. })
    ));
    assert!(matches!(
        multiply(&float(f64::MAX), &float(f64::MAX)),
        Err(OperationError::NonFiniteFloat)
    ));
    assert!(matches!(
        add(&int(1), &Value::string("x")),
        Err(OperationError::UnsupportedOperands { .. })
    ));
    assert!(matches!(
        negate(&Value::Bool(true)),
        Err(OperationError::UnsupportedOperands { .. })
    ));
}

#[test]
fn comparison_is_exact_and_equality_is_total() {
    // Exact mixed numeric ordering without lossy rounding.
    assert_eq!(less(&int(2), &float(2.5)).unwrap(), Value::Bool(true));
    assert_eq!(greater(&float(2.5), &int(2)).unwrap(), Value::Bool(true));
    assert_eq!(
        greater(&int(9_007_199_254_740_993), &float(9_007_199_254_740_992.0)).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        less_equal(&int(9_007_199_254_740_992), &float(9_007_199_254_740_992.0)).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        greater_equal(&Value::string("b"), &Value::string("a")).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        operation::order(&Value::string("a"), &Value::string("b")).unwrap(),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        less(
            &Value::list(vec![int(1), int(2)]),
            &Value::list(vec![int(1), int(3)]),
        )
        .unwrap(),
        Value::Bool(true)
    );

    // Cross-domain ordering is a type error; equality is total and never errors.
    assert!(matches!(
        less(&int(1), &Value::string("a")),
        Err(OperationError::UnsupportedOperands { .. })
    ));
    assert!(matches!(
        less(&Value::Bool(true), &Value::Bool(false)),
        Err(OperationError::UnsupportedOperands { .. })
    ));
    assert_eq!(equal(&int(2), &float(2.0)), Value::Bool(true));
    assert_eq!(equal(&int(2), &Value::string("2")), Value::Bool(false));
    assert_eq!(
        not_equal(&Value::Null, &Value::Bool(false)),
        Value::Bool(true)
    );
}

#[test]
fn membership_covers_ranges_lists_strings_and_records() {
    let list = Value::list(vec![int(1), Value::string("x")]);
    let record = Value::from(Record::new(vec![("name".to_owned(), int(1))]).unwrap());
    let span = Value::from(Range::new(0, 5, false));

    assert_eq!(member(&int(3), &span), Ok(Value::Bool(true)));
    assert_eq!(member(&int(5), &span), Ok(Value::Bool(false)));
    assert_eq!(member(&Value::string("x"), &list), Ok(Value::Bool(true)));
    assert_eq!(member(&int(2), &list), Ok(Value::Bool(false)));
    assert_eq!(
        member(&Value::string("ell"), &Value::string("hello")),
        Ok(Value::Bool(true))
    );
    assert_eq!(
        member(&Value::string("name"), &record),
        Ok(Value::Bool(true))
    );
    assert_eq!(
        member(&Value::string("age"), &record),
        Ok(Value::Bool(false))
    );
    assert!(matches!(
        member(&int(1), &Value::string("hello")),
        Err(OperationError::UnsupportedOperands { .. })
    ));
}

#[test]
fn indexing_and_member_access_reject_absent_and_negative_positions() {
    let list = Value::list(vec![int(10), int(20), int(30)]);
    let record = Record::new(vec![
        ("name".to_owned(), Value::string("fsh")),
        ("count".to_owned(), int(2)),
    ])
    .unwrap();
    let record_value = Value::from(record);

    assert_eq!(index(&list, &int(0)).unwrap(), int(10));
    assert_eq!(index(&list, &int(2)).unwrap(), int(30));
    assert_eq!(
        index(&Value::string("héllo"), &int(1)).unwrap(),
        Value::string("é")
    );
    assert_eq!(
        index(&record_value, &Value::string("count")).unwrap(),
        int(2)
    );
    assert_eq!(field(&record_value, "name").unwrap(), Value::string("fsh"));

    assert!(matches!(
        index(&list, &int(3)),
        Err(OperationError::IndexOutOfRange {
            index: 3,
            length: 3
        })
    ));
    assert!(matches!(
        index(&list, &int(-1)),
        Err(OperationError::NegativeIndex { index: -1 })
    ));
    assert!(matches!(
        index(&Value::string("hi"), &int(5)),
        Err(OperationError::IndexOutOfRange { .. })
    ));
    assert!(matches!(
        index(&record_value, &Value::string("missing")),
        Err(OperationError::MissingKey { .. })
    ));
    assert!(matches!(
        field(&record_value, "missing"),
        Err(OperationError::MissingField { .. })
    ));
    assert!(matches!(
        field(&int(1), "name"),
        Err(OperationError::UnsupportedOperands { .. })
    ));
    assert!(matches!(
        index(&int(1), &int(0)),
        Err(OperationError::UnsupportedOperands { .. })
    ));
}

#[test]
fn ranges_require_int_endpoints_and_allow_empty() {
    assert_eq!(
        range(&int(0), &int(3), false).unwrap(),
        Value::from(Range::new(0, 3, false))
    );
    assert_eq!(
        range(&int(0), &int(3), true).unwrap(),
        Value::from(Range::new(0, 3, true))
    );
    let empty = range(&int(5), &int(2), false).unwrap();
    assert_eq!(empty, Value::from(Range::new(5, 2, false)));
    let Value::Range(empty_range) = empty else {
        panic!("expected a range value");
    };
    assert!(empty_range.is_empty());
    assert_eq!(
        format!("{:?}", range(&int(0), &int(3), true).unwrap()),
        "0..=3"
    );

    assert!(matches!(
        range(&int(0), &float(3.0), false),
        Err(OperationError::UnsupportedOperands { .. })
    ));
}

#[test]
fn numeric_conversions_truncate_and_widen() {
    assert_eq!(to_int(&float(3.9)).unwrap(), int(3));
    assert_eq!(to_int(&float(-3.9)).unwrap(), int(-3));
    assert_eq!(to_int(&int(7)).unwrap(), int(7));
    assert_eq!(to_float(&int(7)).unwrap(), float(7.0));
    assert_eq!(to_float(&float(1.5)).unwrap(), float(1.5));

    assert!(matches!(
        to_int(&float(1e30)),
        Err(OperationError::ConversionOutOfRange { .. })
    ));
    assert!(matches!(
        to_int(&Value::string("3")),
        Err(OperationError::UnsupportedOperands { .. })
    ));
    assert!(matches!(
        to_float(&Value::Null),
        Err(OperationError::UnsupportedOperands { .. })
    ));
}
