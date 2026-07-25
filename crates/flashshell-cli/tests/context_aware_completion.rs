#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use flashshell_cli::completion::{CompletionCatalog, CompletionEngine, CompletionKind};
use flashshell_runtime::command::{Carrier, CommandRegistry, CommandSignature};
use flashshell_runtime::{BindingMutability, Callable, ScopeStack, Value};

#[derive(Debug)]
struct NamedFunction;

impl Callable for NamedFunction {
    fn family(&self) -> &'static str {
        "function"
    }

    fn display(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<function>")
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[test]
fn command_heads_order_runtime_sources_and_deduplicate_first_wins() {
    let mut registry = CommandRegistry::new();
    registry.register(CommandSignature::new(
        "alpha",
        [Carrier::Empty],
        Carrier::Empty,
    ));
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "alpine",
            BindingMutability::Immutable,
            Value::Callable(Arc::new(NamedFunction)),
        )
        .expect("unique function");
    let catalog = CompletionCatalog::from_runtime(&registry, &scope)
        .with_external_commands(["alpha", "awk", "zsh"]);

    let completions = CompletionEngine::new(catalog).complete("a", 1);
    assert_eq!(
        completions
            .iter()
            .map(|completion| (completion.value(), completion.kind()))
            .collect::<Vec<_>>(),
        [
            ("alpha", CompletionKind::InternalCommand),
            ("alpine", CompletionKind::Function),
            ("awk", CompletionKind::ExternalCommand),
        ]
    );
    assert!(completions.iter().all(|completion| {
        completion.replacement() == (0..1) && completion.append_whitespace()
    }));

    let middle = CompletionEngine::new(
        CompletionCatalog::from_runtime(&registry, &scope).with_external_commands(["awk"]),
    )
    .complete("alZZ", 2);
    assert_eq!(middle[0].value(), "alpha");
    assert_eq!(middle[0].replacement(), 0..4);
}

#[test]
fn variable_completion_uses_visible_scope_and_replaces_the_dollar_word() {
    let registry = CommandRegistry::new();
    let mut scope = ScopeStack::new();
    scope
        .declare("name", BindingMutability::Immutable, Value::Null)
        .expect("unique binding");
    scope
        .declare("native", BindingMutability::Immutable, Value::Null)
        .expect("unique binding");
    let engine = CompletionEngine::new(CompletionCatalog::from_runtime(&registry, &scope));
    let source = "echo λ $na";

    let completions = engine.complete(source, source.len());
    assert_eq!(
        completions
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["$name", "$native"]
    );
    assert!(completions.iter().all(|completion| {
        completion.kind() == CompletionKind::Variable
            && completion.replacement() == ((source.len() - 3)..source.len())
            && !completion.append_whitespace()
    }));
    assert!(engine.complete(source, 6).is_empty());
}

#[test]
fn flags_come_only_from_the_matching_internal_signature() {
    let mut registry = CommandRegistry::new();
    registry.register(
        CommandSignature::new("query", [Carrier::Empty], Carrier::Value).with_flags([
            "--all",
            "--ascii",
            "--verbose",
        ]),
    );
    let scope = ScopeStack::new();
    let engine = CompletionEngine::new(CompletionCatalog::from_runtime(&registry, &scope));

    let source = "query --a";
    let completions = engine.complete(source, source.len());
    assert_eq!(
        completions
            .iter()
            .map(|completion| (completion.value(), completion.kind()))
            .collect::<Vec<_>>(),
        [
            ("--all", CompletionKind::Flag),
            ("--ascii", CompletionKind::Flag),
        ]
    );
    assert!(engine.complete("external --a", 12).is_empty());
}

#[test]
fn external_forcing_and_path_contexts_use_only_their_host_snapshots() {
    let mut registry = CommandRegistry::new();
    registry.register(CommandSignature::new(
        "git",
        [Carrier::Empty],
        Carrier::ByteStream,
    ));
    let scope = ScopeStack::new();
    let catalog = CompletionCatalog::from_runtime(&registry, &scope)
        .with_external_commands(["git", "git-lfs"])
        .with_paths(["output.log", "outbox/", "./docs/", "./downloads/"]);
    let engine = CompletionEngine::new(catalog);

    let forced = engine.complete("^gi", 3);
    assert_eq!(
        forced
            .iter()
            .map(|completion| (completion.value(), completion.kind()))
            .collect::<Vec<_>>(),
        [
            ("git", CompletionKind::ExternalCommand),
            ("git-lfs", CompletionKind::ExternalCommand),
        ]
    );
    assert!(
        forced
            .iter()
            .all(|completion| completion.replacement() == (1..3))
    );

    let redirect = "echo > out";
    assert_eq!(
        engine
            .complete(redirect, redirect.len())
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["outbox/", "output.log"]
    );
    let path = "cat ./do";
    assert_eq!(
        engine
            .complete(path, path.len())
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["./docs/", "./downloads/"]
    );
}
