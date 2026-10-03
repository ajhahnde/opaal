#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;
use opaal_runtime::{Record, Value};
use support::{float, value};

#[test]
fn complete_report_and_caught_domain_have_exact_values_under_pure_limits() {
    let expected = Value::Record(
        Record::new(vec![
            ("percent".into(), float(93.0)),
            ("delta".into(), Value::Int(3)),
            ("scale".into(), float(9.0)),
            ("low".into(), Value::Int(4)),
            ("high".into(), Value::Int(9)),
            ("floor".into(), float(2.0)),
            ("ceil".into(), float(3.0)),
        ])
        .unwrap(),
    );
    assert_eq!(
        value("import './report.opaal' as report\nreport::build()"),
        expected
    );
    assert_eq!(
        value("import './report.opaal' as report\nreport::fallback()"),
        float(0.0)
    );
    assert_eq!(
        value(
            "import std::data as data\nimport './report.opaal' as report\ndata::json_encode(report::build())"
        ),
        Value::bytes(
            include_bytes!("../../../tests/golden/numeric-operations/expected-report.json")
                .to_vec()
        )
    );
}
