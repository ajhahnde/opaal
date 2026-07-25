#![forbid(unsafe_code)]

//! Explicit list spread (`...$name`) expands one bound `List` into zero or more
//! command arguments, and every ineligible spread value or element is a
//! source-spanned runtime error. Ordinary-word list rejection is covered by
//! `expansion.rs`; this file covers the spread item.

use std::ffi::OsStr;

use flashshell_runtime::eval::{ExpandedWord, RuntimeErrorKind, expand_spread};
use flashshell_runtime::{BindingMutability, ScopeError, ScopeStack, Value};
use flashshell_syntax::{
    CommandItemKind, CommandStage, ParseOutcome, SourceFile, SourceId, Span, StageKind,
    StatementKind, VariableReference, parse,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

fn command(file: &SourceFile) -> CommandStage {
    let script = match parse(file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let statement = &script.statements()[0];
    let StatementKind::Job(job) = statement.kind() else {
        panic!("expected a bare command statement");
    };
    let pipeline = &job.chain.or_terms()[0].and_terms()[0];
    let StageKind::Command(stage) = pipeline.stages()[0].kind() else {
        panic!("expected a command stage");
    };
    stage.clone()
}

/// The first spread item of a bare command: its variable reference and the whole
/// `...$name` item span.
fn spread_item(stage: &CommandStage) -> (VariableReference, Span) {
    for item in &stage.items {
        if let CommandItemKind::Spread(variable) = item.kind() {
            return (*variable, item.span());
        }
    }
    panic!("command has no spread item");
}

/// Expands the first spread item of a single-command source in the given scope.
fn expand_in(text: &str, scope: &mut ScopeStack) -> Result<Vec<ExpandedWord>, RuntimeErrorKind> {
    let file = source(text);
    let stage = command(&file);
    let (variable, item_span) = spread_item(&stage);
    expand_spread(&variable, item_span, &file, scope).map_err(|error| error.kind().clone())
}

fn bind(scope: &mut ScopeStack, name: &str, value: Value) {
    scope
        .declare(name, BindingMutability::Immutable, value)
        .unwrap();
}

fn values(words: &[ExpandedWord]) -> Vec<&OsStr> {
    words.iter().map(ExpandedWord::value).collect()
}

#[test]
fn spread_expands_a_list_into_multiple_arguments() {
    let mut scope = ScopeStack::new();
    bind(
        &mut scope,
        "args",
        Value::list(vec![Value::string("status"), Value::string("--short")]),
    );
    let words = expand_in("git ...$args", &mut scope).unwrap();
    assert_eq!(
        values(&words),
        [OsStr::new("status"), OsStr::new("--short")]
    );
}

#[test]
fn an_empty_list_spread_contributes_no_arguments() {
    let mut scope = ScopeStack::new();
    bind(&mut scope, "none", Value::list(vec![]));
    let words = expand_in("git ...$none", &mut scope).unwrap();
    assert!(words.is_empty());
}

#[test]
fn each_element_uses_its_canonical_word_encoding() {
    let mut scope = ScopeStack::new();
    bind(
        &mut scope,
        "mix",
        Value::list(vec![
            Value::Int(-7),
            Value::Bool(true),
            Value::Path(flashshell_runtime::NativePath::new("/etc/hosts")),
        ]),
    );
    let words = expand_in("show ...$mix", &mut scope).unwrap();
    assert_eq!(
        values(&words),
        [
            OsStr::new("-7"),
            OsStr::new("true"),
            OsStr::new("/etc/hosts"),
        ]
    );
}

#[test]
fn an_empty_element_is_one_argument_without_provenance() {
    let mut scope = ScopeStack::new();
    bind(&mut scope, "args", Value::list(vec![Value::string("")]));
    let words = expand_in("show ...$args", &mut scope).unwrap();
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].value(), OsStr::new(""));
    assert!(words[0].parts().is_empty());
}

#[test]
fn a_non_list_spread_value_is_an_error_at_the_item() {
    let mut scope = ScopeStack::new();
    bind(&mut scope, "one", Value::string("solo"));
    let file = source("git ...$one");
    let stage = command(&file);
    let (variable, item_span) = spread_item(&stage);
    let error = expand_spread(&variable, item_span, &file, &mut scope).unwrap_err();
    assert_eq!(
        error.kind(),
        &RuntimeErrorKind::SpreadValueNotList { actual: "string" }
    );
    assert_eq!(error.span(), item_span);
}

#[test]
fn an_ineligible_element_reports_its_zero_based_index() {
    let mut scope = ScopeStack::new();
    bind(
        &mut scope,
        "args",
        Value::list(vec![Value::Int(1), Value::Null]),
    );
    assert_eq!(
        expand_in("show ...$args", &mut scope).unwrap_err(),
        RuntimeErrorKind::SpreadElementNotWordEligible {
            index: 1,
            actual: "null",
        }
    );
}

#[test]
fn spread_never_recursively_flattens_a_nested_list() {
    let mut scope = ScopeStack::new();
    bind(
        &mut scope,
        "args",
        Value::list(vec![Value::list(vec![Value::Int(1)])]),
    );
    assert_eq!(
        expand_in("show ...$args", &mut scope).unwrap_err(),
        RuntimeErrorKind::SpreadElementNotWordEligible {
            index: 0,
            actual: "list",
        }
    );
}

#[test]
fn an_unknown_spread_binding_is_a_scope_error() {
    let mut scope = ScopeStack::new();
    assert_eq!(
        expand_in("git ...$missing", &mut scope).unwrap_err(),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding("missing".to_owned()))
    );
}

#[test]
fn each_spread_word_carries_the_whole_item_span() {
    let mut scope = ScopeStack::new();
    bind(&mut scope, "args", Value::list(vec![Value::string("x")]));
    let file = source("git ...$args");
    let stage = command(&file);
    let (variable, item_span) = spread_item(&stage);
    let words = expand_spread(&variable, item_span, &file, &mut scope).unwrap();
    assert_eq!(words[0].span(), item_span);
    assert_eq!(words[0].parts(), [item_span]);
}
