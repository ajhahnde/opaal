#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;

use opaal_runtime::Value;
use opaal_runtime::eval::{CancellationToken, ResourceBudget};
use opaal_runtime::outcome::PrimaryOutcome;
use support::{completed, load, run, try_load, value};

fn ints(items: &[i64]) -> Value {
    Value::list(items.iter().copied().map(Value::Int).collect())
}

#[test]
fn replacement_preserves_aliases_types_and_lexical_shadowing() {
    assert_eq!(
        value(
            "mut items: List[Int] = [1, 2, 3]\nlet saved = items\nitems[0] = 4\nitems[1] = 5\nitems[2] = 6\n[items, saved]"
        ),
        Value::list(vec![ints(&[4, 5, 6]), ints(&[1, 2, 3])])
    );
    assert_eq!(
        value("mut items = [1]\nif true { mut items = [2]\nitems[0] = 3 }\nitems"),
        ints(&[1])
    );
    assert_eq!(
        value("mut items: List[Any] = [1, 'two']\nitems[0] = false\nitems"),
        Value::list(vec![Value::Bool(false), Value::string("two")])
    );
    assert_eq!(
        value(
            "def replaced[T](input: List[T], index: Int, replacement: T) -> List[T] { mut output: List[T] = input\noutput[index] = replacement\nreturn output }\nreplaced[Int]([1, 2], 1, 8)"
        ),
        ints(&[1, 8])
    );
    assert_eq!(
        value("mut items: List[List[Int]] = [[1], [2]]\nitems[0] = [3,4]\nitems"),
        Value::list(vec![ints(&[3, 4]), ints(&[2])])
    );
    assert_eq!(
        value(
            "type Item = { value: Int }\nmut items: List[Item] = [Item { value: 1 }]\nitems[0] = Item { value: 8 }\nitems[0].value"
        ),
        Value::Int(8)
    );
}

#[test]
fn replacement_supplies_existing_expected_context_for_generic_results() {
    assert_eq!(
        value(
            "def empty[T]() -> List[T] { return [] }\nmut items: List[List[Int]] = [[1]]\nitems[0] = empty()\nitems"
        ),
        Value::list(vec![ints(&[])])
    );
    assert_eq!(
        value(
            "def empty[T]() -> List[T] { return [] }\ndef replace_empty[T](input: List[List[T]]) -> List[List[T]] { mut output: List[List[T]] = input\noutput[0] = empty()\nreturn output }\nreplace_empty[Int]([[1]])"
        ),
        Value::list(vec![ints(&[])])
    );
}

#[test]
fn generic_dynamic_replacement_validates_concrete_type_before_store() {
    let program = "def wrong() -> Any { return 'private-sentinel' }\ndef rejects[T: Equal](input: List[T]) -> Bool {\nmut output: List[T] = input\nlet saved = output\ntry { output[0] = wrong() } catch failure {\nif output == input && saved == input { return true }\n}\nreturn false\n}\n";
    for invocation in ["rejects[Int]([1])", "rejects[List[Int]]([[1]])"] {
        assert_eq!(value(&format!("{program}{invocation}")), Value::Bool(true));
    }
}

#[test]
fn invalid_targets_and_static_types_use_existing_name_checks_and_frozen_codes() {
    for text in [
        "missing[0] = 1",
        "let items = [1]\nitems[0] = 2",
        "status[0] = 2",
        "argv[0] = 2",
        "math[0] = 2",
        "import std::math::{abs}\nabs[0] = 2",
        "def items() { return [1] }\nitems[0] = 2",
        "mut items = [1]\ndef change() { items[0] = 2 }",
    ] {
        assert!(try_load(text).is_err(), "{text}");
    }
    for (text, code) in [
        ("mut items: Int = 1\nitems[0] = 2", "SIG023"),
        (
            "mut items: List[Int] = [1]\nitems['private-index'] = 2",
            "SIG024",
        ),
        (
            "mut items: List[Int] = [1]\nitems[0] = 'private-rhs'",
            "SIG008",
        ),
        ("mut items: List[Int] = [1]\nitems[0] = 1.5", "SIG008"),
    ] {
        let error = try_load(text).unwrap_err();
        let opaal_runtime::module::ModuleProgramError::Signatures(error) = error else {
            panic!("{error:?}")
        };
        assert_eq!(error.diagnostic().code(), code);
        assert!(!error.to_string().contains("private-"));
    }
}

#[test]
fn bounds_and_dynamic_failures_leave_the_original_list_unchanged() {
    for index in ["-1", "1", "9223372036854775807", "dynamic('private-index')"] {
        let text = format!(
            "def dynamic(value: Any) -> Any {{ return value }}\nmut items: List[Int] = [10]\nlet saved = items\ntry {{ items[{index}] = dynamic('private-rhs') }} catch failure {{ null }}\n[items,saved]"
        );
        assert_eq!(value(&text), Value::list(vec![ints(&[10]), ints(&[10])]));
    }
    assert_eq!(
        value(
            "def dynamic() -> Any { return 'private-rhs' }\nmut items: List[Int] = [10]\ntry { items[0] = dynamic() } catch failure { null }\nitems"
        ),
        ints(&[10])
    );
    assert_eq!(
        value("mut items: List[Int] = []\ntry { items[0] = 1 } catch failure { null }\nitems"),
        ints(&[])
    );
    assert_eq!(
        value(
            "mut items: Any = 'private-root'\ntry { items[0] = 1 } catch failure { null }\nitems"
        ),
        Value::string("private-root")
    );
    assert_eq!(
        value("mut items: Any = [1]\nitems[0] = 'two'\nitems"),
        Value::list(vec![Value::string("two")])
    );
}

#[test]
fn original_root_index_and_rhs_query_locations_are_retained() {
    let program = load(
        "# 界🙂\nmut items: List[Int] = [1]\nlet position = 0\nlet replacement = 8\nitems[position] = replacement\nitems",
    );
    let entry = program
        .sources()
        .entries()
        .find(|e| e.module() == program.graph().root())
        .unwrap();
    let commands = opaal_runtime::builtin::standard_registry();
    let queries = program.semantic_queries(&commands);
    for name in ["items", "position", "replacement"] {
        let start = entry.source().text().find("items[position]").unwrap();
        let offset = start + entry.source().text()[start..].find(name).unwrap();
        let definition = queries
            .definition_at(program.graph().root(), offset)
            .unwrap();
        assert_eq!(entry.source().slice(definition.span()).unwrap(), name);
        let refs = queries.references_to(&definition, false);
        assert_eq!(
            refs.iter().filter(|r| r.span().start() == offset).count(),
            1
        );
    }
}

#[test]
fn replacement_has_exact_step_limits_and_cooperative_cancellation() {
    let program = load("mut items: List[List[Int]] = [[1,2],[3,4]]\nitems[0] = [5,6]\nitems");
    let mut first = None;
    for steps in 0..200 {
        let outcome = run(
            &program,
            ResourceBudget::steps(steps),
            CancellationToken::never(),
        );
        if matches!(outcome.primary(), PrimaryOutcome::Completed(_)) {
            assert_eq!(
                completed(&outcome),
                Value::list(vec![ints(&[5, 6]), ints(&[3, 4])])
            );
            first = Some(steps);
            break;
        }
    }
    assert!(first.is_some_and(|n| n > 0));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    let baseline = run(
        &program,
        ResourceBudget::default(),
        CancellationToken::from_fn(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            false
        }),
    );
    assert!(matches!(baseline.primary(), PrimaryOutcome::Completed(_)));
    for at in 0..count.load(std::sync::atomic::Ordering::SeqCst) {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outcome = run(
            &program,
            ResourceBudget::default(),
            CancellationToken::from_fn(move || {
                polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= at
            }),
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)),
            "poll {at}: {:?}",
            outcome.primary()
        );
    }
}
