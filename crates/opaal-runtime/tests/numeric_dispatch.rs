#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;
use opaal_runtime::Value;
use support::{float, session_run, try_load, value};

#[test]
fn every_known_tuple_position_rejects_mixed_families_and_wrong_arity() {
    for source in [
        "math::min(1, 2.0)",
        "math::min(1.0, 2)",
        "math::max(1, 2.0)",
        "math::clamp(1.0, 0, 2)",
        "math::clamp(1, 0.0, 2)",
        "math::clamp(1, 0, 2.0)",
        "math::floor(1)",
        "math::ceil(1)",
        "math::round(1)",
        "math::sqrt(1)",
        "math::abs('x')",
        "math::min(1)",
        "math::abs(1, 2)",
        "math::min[Int](1, 2)",
        "math::sqrt[Float](1.0)",
    ] {
        assert!(try_load(source).is_err(), "accepted {source}");
    }
}

#[test]
fn dynamic_and_unresolved_inputs_keep_candidates_until_concrete_dispatch() {
    for (input, expected) in [("1", Value::Int(1)), ("1.0", float(1.0))] {
        assert_eq!(
            value(&format!("let x: Any = {input}\nmath::abs(x)")),
            expected
        );
    }
    for source in [
        "let x: Any = 'x'\nmath::abs(x)",
        "let x: Any = 2.0\nmath::min(1, x)",
        "let x: Any = 1.0\nmath::clamp(x, 0, 2)",
    ] {
        assert!(
            try_load(source).is_ok(),
            "dynamic input must defer: {source}"
        );
        assert!(
            session_run(source, Value::Null).is_err(),
            "concrete mismatch: {source}"
        );
    }
    for source in [
        "let x: Any = 1\nmath::min(x, 'x')",
        "let x: Any = 1\nmath::clamp(x, 0, 2.0)",
        "let x: Any = 1\nmath::clamp(1, x, 2.0)",
    ] {
        assert!(try_load(source).is_err(), "known contradiction: {source}");
    }
    for source in [
        "def forward[T](x: T) { return math::abs(x) }\nforward(3)",
        "def forward[T](x: T) { return math::min(x, x) }\nforward(3)",
    ] {
        assert_eq!(value(source), Value::Int(3));
    }
    assert_eq!(
        value("def forward[T](x: T) -> Float { return math::sqrt(x) }\nforward(81.0)"),
        float(9.0)
    );
    assert!(
        session_run(
            "let x: Any = 2\nlet result: Float = math::abs(x)",
            Value::Null
        )
        .is_err()
    );
    assert!(try_load("let result: Float = math::abs(2)").is_err());
    for source in [
        "let x: Any = 1.0\nlet result: Float = math::min(x, 2.0)\nresult",
        "let x: Any = 2.0\nlet result: Float = math::min(1.0, x)\nresult",
        "let x: Any = 1.0\nmath::clamp(x, 0.0, 2.0)",
        "let x: Any = 0.0\nmath::clamp(1.0, x, 2.0)",
        "let x: Any = 2.0\nmath::clamp(1.0, 0.0, x)",
        "def forward[T](x: T) -> T { return math::abs(x) }\nforward(-1.0)",
    ] {
        assert_eq!(value(source), float(1.0), "{source}");
    }
    assert_eq!(
        value("let a: Any = -1\nlet b: Any = 2\nmath::min(a, b)"),
        Value::Int(-1)
    );
}

#[test]
fn aliases_callbacks_length_and_argument_order_preserve_released_contracts() {
    assert_eq!(
        value("import './api.opaal' as api\napi::math::max(2.0, 3.0)"),
        float(3.0)
    );
    assert_eq!(
        value("import std::list as list\nlist::map([-2, 3], {|x: Int| -> Int math::abs(x)})"),
        Value::list(vec![Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        value("import std::value as values\nvalues::length[Int]([1, 2])"),
        Value::Int(2)
    );
    let error = session_run("def left() -> Int { throw 'left argument' }\ndef right() -> Int { throw 'right argument' }\nmath::min(left(), right())", Value::Null).unwrap_err().render().to_owned();
    assert!(error.contains("left argument"));
    assert!(!error.contains("right argument"));
    assert!(try_load("math::mean(1.0)").is_err());
    assert!(try_load("[1] | math::abs").is_err());
}
