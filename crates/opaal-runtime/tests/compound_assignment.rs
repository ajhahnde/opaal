#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;

use opaal_runtime::Value;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancelReason, CancellationToken, ResourceBudget};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::query::SemanticHover;
use support::{completed, load, run, try_load, value};

#[test]
fn compound_assignment_uses_original_type_and_mutability_checks() {
    for source in [
        "mut total: Int = 7\ntotal += 0.5",
        "missing += 1",
        "let total = 7\ntotal += 1",
        "status += 1",
        "argv += 1",
        "math += 1",
        "import std::math::{abs}\nabs += 1",
        "def total() { return 1 }\ntotal += 1",
        "mut total = 7\ndef update() { total += 1 }",
    ] {
        assert!(
            try_load(source).is_err(),
            "invalid target/type accepted: {source}"
        );
    }
    assert_eq!(
        value("mut total = 7\ntotal += 0.5\ntotal"),
        support::float(7.5)
    );
    assert_eq!(
        value(
            "def amount() -> Any { return 0.5 }\nmut total: Int = 7\ntry { total += amount() } catch failure { null }\ntotal"
        ),
        Value::Int(7)
    );
    assert_eq!(
        value(
            "def amount() -> Any { return 'private-sentinel' }\nmut total = 7\ntry { total += amount() } catch failure { null }\ntotal"
        ),
        Value::Int(7)
    );
}

#[test]
fn compound_target_queries_share_one_original_declaration_location() {
    let source = "# 界🙂\nmut total: Int = 7\ntotal += 2\ntotal";
    let program = load(source);
    let entry = program
        .sources()
        .entries()
        .find(|entry| entry.module() == program.graph().root())
        .unwrap();
    let text = entry.source().text();
    let offset = text.find("total +=").unwrap();
    let commands = standard_registry();
    let queries = program.semantic_queries(&commands);
    let definition = queries
        .definition_at(program.graph().root(), offset)
        .unwrap();
    assert_eq!(entry.source().slice(definition.span()).unwrap(), "total");
    assert!(definition.span().start() < offset);
    assert!(matches!(
        queries.hover_at(program.graph().root(), offset),
        Some(SemanticHover::Binding(_))
    ));
    let references = queries.references_to(&definition, false);
    assert_eq!(
        references.len(),
        2,
        "read/write at the same token is one editor location"
    );
    assert_eq!(references[0].span().start(), offset);
}

#[test]
fn compound_and_expanded_forms_charge_the_same_evaluation_steps_and_cancel() {
    let compound = load("mut total = 7\ntotal += math::abs(-2)\ntotal");
    let expanded = load("mut total = 7\ntotal = total + math::abs(-2)\ntotal");
    let mut first_success = None;
    for steps in 0..40 {
        let a = run(
            &compound,
            ResourceBudget::steps(steps),
            CancellationToken::never(),
        );
        let b = run(
            &expanded,
            ResourceBudget::steps(steps),
            CancellationToken::never(),
        );
        assert_eq!(
            matches!(a.primary(), PrimaryOutcome::Completed(_)),
            matches!(b.primary(), PrimaryOutcome::Completed(_)),
            "steps {steps}"
        );
        if matches!(a.primary(), PrimaryOutcome::Completed(_)) {
            assert_eq!(completed(&a), Value::Int(9));
            first_success.get_or_insert(steps);
        }
    }
    assert!(first_success.is_some_and(|steps| steps > 0));
    let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&observed);
    let outcome = run(
        &compound,
        ResourceBudget::steps(10_000),
        CancellationToken::from_fn(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            false
        }),
    );
    assert_eq!(completed(&outcome), Value::Int(9));
    let polls = observed.load(std::sync::atomic::Ordering::Relaxed);
    assert!(polls > 0);
    for schedule in 0..polls {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let token = CancellationToken::from_fn(move || {
            polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= schedule
        });
        let outcome = run(&compound, ResourceBudget::steps(10_000), token);
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(c) if c.reason() == CancelReason::Requested),
            "schedule {schedule}: {:?}",
            outcome.primary()
        );
    }
}
