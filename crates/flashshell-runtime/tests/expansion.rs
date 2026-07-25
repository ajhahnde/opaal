#![forbid(unsafe_code)]

//! Ordinary-word expansion of bare, single-quoted, double-quoted, `$name`, and
//! `${expression}` parts into one platform-native command argument.

use std::ffi::{OsStr, OsString};

use flashshell_runtime::eval::{ExpandedWord, RuntimeErrorKind, expand_word};
use flashshell_runtime::{BindingMutability, ByteSize, Duration, ScopeError, ScopeStack, Value};
use flashshell_syntax::{
    CommandItemKind, CommandStage, ParseOutcome, SourceFile, SourceId, StageKind, StatementKind,
    Word, parse,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

/// Parses one bare command statement and returns its command stage.
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

/// The nth item word (argument) of a bare command; index 0 is the first argument.
fn nth_arg_word(stage: &CommandStage, index: usize) -> Word {
    let mut seen = 0;
    for item in &stage.items {
        if let CommandItemKind::Word(word) = item.kind() {
            if seen == index {
                return word.clone();
            }
            seen += 1;
        }
    }
    panic!("command has no argument word at index {index}");
}

/// Expands the first argument word of a single-command source in a fresh scope.
fn expand(text: &str) -> Result<ExpandedWord, RuntimeErrorKind> {
    let mut scope = ScopeStack::new();
    expand_in(text, &mut scope)
}

fn expand_in(text: &str, scope: &mut ScopeStack) -> Result<ExpandedWord, RuntimeErrorKind> {
    let file = source(text);
    let stage = command(&file);
    let word = nth_arg_word(&stage, 0);
    expand_word(&word, &file, scope).map_err(|error| error.kind().clone())
}

fn value_of(text: &str) -> OsString {
    expand(text)
        .unwrap_or_else(|kind| panic!("expansion failed for {text:?}: {kind:?}"))
        .value()
        .to_os_string()
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

#[test]
fn bare_parts_and_escapes_form_one_literal_argument() {
    assert_eq!(value_of("show one\\ word"), os("one word"));
    assert_eq!(value_of("show \\#literal"), os("#literal"));
    assert_eq!(value_of("show a\\|b"), os("a|b"));
    assert_eq!(value_of("show pre\\npost"), os("prenpost")); // bare `\n` is literal `n`
}

#[test]
fn single_quotes_are_exact_and_may_be_empty() {
    assert_eq!(value_of("show 'literal $name'"), os("literal $name"));
    assert_eq!(value_of("show ''"), os(""));
    // An empty quoted part still produces one argument alongside adjacent parts.
    assert_eq!(value_of("show 'x'''"), os("x"));
}

#[test]
fn double_quotes_decode_escapes_and_join_adjacent_parts() {
    assert_eq!(value_of("show \"a\\tb\""), os("a\tb"));
    assert_eq!(value_of("show \"line\\nfeed\""), os("line\nfeed"));
    assert_eq!(value_of("show \"\\u{41}\\u{1F600}\""), os("A\u{1F600}"));
    assert_eq!(value_of("show \"\\$name\""), os("$name"));
    assert_eq!(value_of("show \"x\"''\"y\""), os("xy"));
}

#[test]
fn scalar_interpolation_encodes_each_family_canonically() {
    let mut scope = ScopeStack::new();
    scope
        .declare("name", BindingMutability::Immutable, Value::string("Flash"))
        .unwrap();
    scope
        .declare("count", BindingMutability::Immutable, Value::Int(-7))
        .unwrap();
    scope
        .declare(
            "ratio",
            BindingMutability::Immutable,
            Value::Float(flashshell_runtime::FiniteFloat::new(1.5).unwrap()),
        )
        .unwrap();
    scope
        .declare("flag", BindingMutability::Immutable, Value::Bool(true))
        .unwrap();
    scope
        .declare(
            "wait",
            BindingMutability::Immutable,
            Value::Duration(Duration::from_nanos(500)),
        )
        .unwrap();
    scope
        .declare(
            "room",
            BindingMutability::Immutable,
            Value::ByteSize(ByteSize::new(1024)),
        )
        .unwrap();

    assert_eq!(
        expand_in("show pre${$count}post", &mut scope)
            .unwrap()
            .value(),
        OsStr::new("pre-7post")
    );
    assert_eq!(
        expand_in("show $name", &mut scope).unwrap().value(),
        OsStr::new("Flash")
    );
    assert_eq!(
        expand_in("show $ratio", &mut scope).unwrap().value(),
        OsStr::new("1.5")
    );
    assert_eq!(
        expand_in("show $flag", &mut scope).unwrap().value(),
        OsStr::new("true")
    );
    assert_eq!(
        expand_in("show $wait", &mut scope).unwrap().value(),
        OsStr::new("500ns")
    );
    assert_eq!(
        expand_in("show $room", &mut scope).unwrap().value(),
        OsStr::new("1024b")
    );
}

#[test]
fn braced_interpolation_evaluates_an_expression_once() {
    assert_eq!(value_of("show pre${1 + 1}post"), os("pre2post"));
    assert_eq!(value_of("show ${'x'}"), os("x"));
}

#[test]
fn path_interpolation_preserves_native_units() {
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "p",
            BindingMutability::Immutable,
            Value::Path(flashshell_runtime::NativePath::new("/etc/hosts")),
        )
        .unwrap();
    assert_eq!(
        expand_in("show $p", &mut scope).unwrap().value(),
        OsStr::new("/etc/hosts")
    );
}

#[test]
fn ineligible_interpolated_values_are_word_errors() {
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "items",
            BindingMutability::Immutable,
            Value::list(vec![Value::Int(1)]),
        )
        .unwrap();
    scope
        .declare("nothing", BindingMutability::Immutable, Value::Null)
        .unwrap();

    assert_eq!(
        expand_in("show $items", &mut scope).unwrap_err(),
        RuntimeErrorKind::WordValueNotWordEligible { actual: "list" }
    );
    assert_eq!(
        expand_in("show $nothing", &mut scope).unwrap_err(),
        RuntimeErrorKind::WordValueNotWordEligible { actual: "null" }
    );
    // A bare literal `null` is also ineligible in word position.
    assert_eq!(
        expand("show ${null}").unwrap_err(),
        RuntimeErrorKind::WordValueNotWordEligible { actual: "null" }
    );
}

#[test]
fn an_unknown_binding_in_a_word_is_a_scope_error() {
    assert_eq!(
        expand("show $missing").unwrap_err(),
        RuntimeErrorKind::Scope(ScopeError::UnknownBinding("missing".to_owned()))
    );
}

#[test]
fn provenance_records_each_contributing_part() {
    // `pre${$count}post` has three contributing parts once `count` is bound.
    let mut scope = ScopeStack::new();
    scope
        .declare("count", BindingMutability::Immutable, Value::Int(2))
        .unwrap();
    let file = source("show pre${$count}post");
    let stage = command(&file);
    let word = nth_arg_word(&stage, 0);
    let expanded = expand_word(&word, &file, &mut scope).unwrap();
    assert_eq!(expanded.value(), OsStr::new("pre2post"));
    assert_eq!(expanded.parts().len(), 3);
    // The whole-word span is retained for diagnostics.
    assert_eq!(expanded.span(), word.span());
}
