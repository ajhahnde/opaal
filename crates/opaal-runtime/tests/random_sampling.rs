#![forbid(unsafe_code)]
#[path = "support/random.rs"]
mod support;
use opaal_runtime::Value;
use support::Harness;

#[test]
fn reference_words_rejection_lattice_bytes_and_no_draw() {
    let mut h = Harness::new(&[0, 5, u64::MAX], &[0, 1, 2]);
    assert_eq!(
        h.invoke("int", &[Value::Int(0), Value::Int(3)]).unwrap(),
        Value::Int(2)
    );
    let Value::Float(fraction) = h.invoke("float", &[]).unwrap() else {
        panic!("Float expected")
    };
    assert_eq!(fraction.get(), 1.0 - 2_f64.powi(-53));
    assert_eq!(
        h.invoke("bytes", &[Value::Int(3)]).unwrap(),
        Value::bytes(vec![0, 1, 2])
    );
    assert_eq!(
        h.invoke("bytes", &[Value::Int(0)]).unwrap(),
        Value::bytes(Vec::new())
    );
    assert_eq!(
        h.invoke("int", &[Value::Int(7), Value::Int(8)]).unwrap(),
        Value::Int(7)
    );
    assert_eq!(h.fills(), [8, 8, 8, 3]);
    assert_eq!(h.state.consumed_bytes(), 27);
}

#[test]
fn full_signed_intervals_power_of_two_crossing_zero_and_endpoints() {
    for (min, max, word, expected) in [
        (i64::MIN, i64::MAX, u64::MAX, i64::MIN),
        (i64::MIN, i64::MAX, u64::MAX - 1, i64::MAX - 1),
        (i64::MIN, 0, u64::MAX, -1),
        (-2, 2, 0, -2),
        (-2, 2, 3, 1),
        (0, 8, 7, 7),
        (i64::MAX - 1, i64::MAX, 0, i64::MAX - 1),
    ] {
        let mut h = Harness::new(&[word], &[]);
        assert_eq!(
            h.invoke("int", &[Value::Int(min), Value::Int(max)])
                .unwrap(),
            Value::Int(expected)
        );
    }
    let mut h = Harness::new(&[0], &[]);
    let Value::Float(zero) = h.invoke("float", &[]).unwrap() else {
        panic!("Float expected")
    };
    assert_eq!(zero.get().to_bits(), 0);
}

#[test]
fn exhaustive_reduced_word_model_has_equal_counts_for_every_interval() {
    for width in 1..=255_u16 {
        let threshold = 256 % width;
        let mut counts = vec![0; usize::from(width)];
        for word in threshold..256 {
            counts[usize::from(word % width)] += 1;
        }
        assert!(counts.iter().all(|count| *count == counts[0]));
    }
}

#[test]
fn domains_and_wrong_signatures_fail_before_entropy() {
    let mut h = Harness::new(&[], &[]);
    for args in [
        [Value::Int(4), Value::Int(4)],
        [Value::Int(4), Value::Int(3)],
    ] {
        assert_eq!(h.invoke("int", &args).unwrap_err().code(), "RANDOM001");
    }
    for count in [-1, 1048577] {
        assert_eq!(
            h.invoke("bytes", &[Value::Int(count)]).unwrap_err().code(),
            "RANDOM001"
        );
    }
    for (name, args) in [
        ("seed", vec![]),
        ("int", vec![Value::Int(1)]),
        ("bytes", vec![Value::string("1")]),
    ] {
        assert_eq!(h.invoke(name, &args).unwrap_err().code(), "OPERATION001");
    }
    assert!(h.fills().is_empty());
}
