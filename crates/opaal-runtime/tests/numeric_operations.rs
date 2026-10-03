#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;
use opaal_runtime::Value;
use support::{float, session_run, value};

#[test]
fn homogeneous_int_edges_bounds_and_checked_abs() {
    for (source, expected) in [
        ("math::abs(-9223372036854775807)", i64::MAX),
        (
            "math::min((-9223372036854775807 - 1), 9223372036854775807)",
            i64::MIN,
        ),
        (
            "math::max((-9223372036854775807 - 1), 9223372036854775807)",
            i64::MAX,
        ),
        ("math::min(3, 3)", 3),
        ("math::max(3, 3)", 3),
        ("math::clamp(0, -2, 2)", 0),
        ("math::clamp(-3, -2, 2)", -2),
        ("math::clamp(3, -2, 2)", 2),
        ("math::clamp(3, 2, 2)", 2),
        ("math::clamp(2, -2, 2)", 2),
        ("math::clamp(-2, -2, 2)", -2),
    ] {
        assert_eq!(value(source), Value::Int(expected), "{source}");
    }
    assert!(
        session_run("math::abs(input)", Value::Int(i64::MIN))
            .unwrap_err()
            .render()
            .contains("overflow")
    );
}

#[test]
fn finite_float_vectors_and_rounding_preserve_family_and_positive_zero() {
    for (source, expected) in [
        ("math::abs(-2.5)", 2.5_f64),
        ("math::min(2.5, 3.5)", 2.5),
        ("math::max(2.5, 3.5)", 3.5),
        ("math::min(2.5, 2.5)", 2.5),
        ("math::max(2.5, 2.5)", 2.5),
        ("math::clamp(2.5, 1.0, 2.0)", 2.0),
        ("math::clamp(-2.5, 1.0, 2.0)", 1.0),
        ("math::clamp(2.5, 2.0, 2.0)", 2.0),
        ("math::floor(-2.1)", -3.0),
        ("math::ceil(-2.7)", -2.0),
        ("math::round(0.5)", 1.0),
        ("math::round(-0.5)", -1.0),
        ("math::round(2.5)", 3.0),
        ("math::round(-2.5)", -3.0),
        ("math::round(-0.1)", 0.0),
        ("math::ceil(-0.1)", 0.0),
        ("math::sqrt(-0.0)", 0.0),
        ("math::abs(-0.0)", 0.0),
    ] {
        let Value::Float(actual) = value(source) else {
            panic!("Float expected")
        };
        assert_eq!(actual.get().to_bits(), expected.to_bits(), "{source}");
    }
    for name in ["abs", "floor", "ceil", "round"] {
        assert_eq!(
            session_run(&format!("math::{name}(input)"), float(f64::MAX)).unwrap(),
            float(f64::MAX)
        );
    }
    let tiny = f64::from_bits(1);
    assert_eq!(
        session_run("math::floor(input)", float(tiny)).unwrap(),
        float(0.0)
    );
    assert_eq!(
        session_run("math::ceil(input)", float(tiny)).unwrap(),
        float(1.0)
    );
    assert_eq!(
        session_run("math::round(input)", float(tiny)).unwrap(),
        float(0.0)
    );
}

#[test]
fn square_root_matches_exact_binary64_vectors_and_rejects_negative_domain() {
    for (input, bits) in [
        (0.0, 0),
        (81.0, 9.0_f64.to_bits()),
        (2.0, 0x3ff6_a09e_667f_3bcd),
        (f64::from_bits(1), 0x1e60_0000_0000_0000),
        (f64::MAX, 0x5fef_ffff_ffff_ffff),
    ] {
        let Value::Float(actual) = session_run("math::sqrt(input)", float(input)).unwrap() else {
            panic!("Float expected")
        };
        assert_eq!(actual.get().to_bits(), bits, "input {input}");
    }
    for source in [
        "math::sqrt(-1.0)",
        "math::clamp(1, 2, 0)",
        "math::clamp(1.0, 2.0, 0.0)",
    ] {
        assert_eq!(
            value(&format!("try {{ {source} }} catch error {{ 7 }}")),
            Value::Int(7)
        );
    }
}
