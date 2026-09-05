#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use flash_cli::completion::{CompletionCatalog, CompletionEngine, CompletionKind};
use flash_runtime::command::{
    Carrier, CommandLifecycle, CommandNamespaceEntry, CommandRegistry, CommandSignature,
};
use flash_runtime::{BindingMutability, Callable, ScopeStack, Value};
use flash_syntax::LanguageMajor;

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
    assert_eq!(engine.complete("echo $sta", 9)[0].value(), "$status");
}

#[test]
fn command_substitution_modifier_completion_is_exact_and_contextual() {
    let engine = CompletionEngine::new(CompletionCatalog::from_runtime(
        &CommandRegistry::new(),
        &ScopeStack::new(),
    ));

    let all = engine.complete("let value = $(", 14);
    assert_eq!(
        all.iter()
            .map(|completion| (completion.value(), completion.kind()))
            .collect::<Vec<_>>(),
        [
            ("bytes:", CompletionKind::CommandCaptureModifier),
            ("text:", CompletionKind::CommandCaptureModifier),
        ]
    );
    assert!(all.iter().all(|completion| completion.append_whitespace()));

    let bytes = engine.complete("let value = $(by", 16);
    assert_eq!(bytes.len(), 1);
    assert_eq!(bytes[0].value(), "bytes:");
    assert_eq!(bytes[0].replacement(), 14..16);

    assert!(engine.complete("^tool by", 8).is_empty());
}

#[test]
fn expression_completion_uses_intrinsics_and_respects_lexical_shadowing() {
    let registry = CommandRegistry::new();
    let source = "let value = fl(3.9)";
    let cursor = source.find('(').unwrap();
    let intrinsic = CompletionEngine::new(CompletionCatalog::from_runtime(
        &registry,
        &ScopeStack::new(),
    ))
    .complete(source, cursor);
    assert_eq!(
        intrinsic
            .iter()
            .map(|completion| (completion.value(), completion.kind()))
            .collect::<Vec<_>>(),
        [("float", CompletionKind::Intrinsic)]
    );
    assert_eq!(intrinsic[0].replacement(), 12..14);
    assert!(!intrinsic[0].append_whitespace());

    let env_source = "let home = en('HOME')";
    let env_cursor = env_source.find('(').unwrap();
    let env = CompletionEngine::new(CompletionCatalog::from_runtime(
        &registry,
        &ScopeStack::new(),
    ))
    .complete(env_source, env_cursor);
    assert_eq!(env[0].value(), "env");
    assert_eq!(env[0].kind(), CompletionKind::Intrinsic);

    let glob_source = "let files = gl('*.fsh')";
    let glob_cursor = glob_source.find('(').unwrap();
    let glob = CompletionEngine::new(CompletionCatalog::from_runtime(
        &registry,
        &ScopeStack::new(),
    ))
    .complete(glob_source, glob_cursor);
    assert_eq!(glob[0].value(), "glob");
    assert_eq!(glob[0].kind(), CompletionKind::Intrinsic);

    let mut value_shadow = ScopeStack::new();
    value_shadow
        .declare("float", BindingMutability::Immutable, Value::Null)
        .expect("unique shadow");
    assert!(
        CompletionEngine::new(CompletionCatalog::from_runtime(&registry, &value_shadow))
            .complete(source, cursor)
            .is_empty()
    );

    let mut function_shadow = ScopeStack::new();
    function_shadow
        .declare(
            "float",
            BindingMutability::Immutable,
            Value::Callable(Arc::new(NamedFunction)),
        )
        .expect("unique function shadow");
    let function =
        CompletionEngine::new(CompletionCatalog::from_runtime(&registry, &function_shadow))
            .complete(source, cursor);
    assert_eq!(function[0].kind(), CompletionKind::Function);
    assert!(!function[0].append_whitespace());
}

#[test]
fn v2_operation_completion_uses_the_retained_qualified_spelling() {
    let catalog = CompletionCatalog::new()
        .with_language(LanguageMajor::V2)
        .with_operations(["value::length"]);
    let engine = CompletionEngine::new(catalog);

    let expression = "value::le([1])";
    let cursor = expression.find('(').unwrap();
    let completions = engine.complete(expression, cursor);
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].value(), "value::length");
    assert_eq!(completions[0].kind(), CompletionKind::Operation);
    assert_eq!(completions[0].replacement(), 0..9);
    assert!(!completions[0].append_whitespace());

    let pipeline = "[1] | value::le";
    let completions = engine.complete(pipeline, pipeline.len());
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].value(), "value::length");
    assert_eq!(completions[0].kind(), CompletionKind::Operation);
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
fn aliases_reuse_canonical_flags_and_reserved_names_are_not_commands() {
    let registry = CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(
                CommandSignature::new("query", [Carrier::Empty], Carrier::Value)
                    .with_flags(["--all", "--verbose"]),
                CommandLifecycle::introduced(1),
            ),
            CommandNamespaceEntry::alias("ask", "query", CommandLifecycle::introduced(1)),
            CommandNamespaceEntry::reserved("archive", 1, "future command", None),
        ],
    )
    .expect("valid completion namespace");
    let engine = CompletionEngine::new(CompletionCatalog::from_runtime(
        &registry,
        &ScopeStack::new(),
    ));

    assert_eq!(
        engine
            .complete("a", 1)
            .iter()
            .map(|completion| (completion.value(), completion.kind()))
            .collect::<Vec<_>>(),
        [("ask", CompletionKind::InternalCommand)]
    );
    assert_eq!(
        engine
            .complete("ask --", 6)
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["--all", "--verbose"]
    );
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

#[test]
fn path_completion_renders_reversible_bare_and_quoted_source() {
    let catalog = CompletionCatalog::new().with_paths([
        "./two words",
        "quote'name",
        "double\"name",
        "unicode/🚀.fsh",
        "line\nbreak",
    ]);
    let engine = CompletionEngine::new(catalog);

    assert_eq!(
        engine
            .complete("cat ./two", 9)
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["./two\\ words"]
    );
    assert_eq!(
        engine.complete("cat 'quote", 10)[0].value(),
        "'quote'\\''name'"
    );
    assert_eq!(
        engine.complete("cat \"double", 11)[0].value(),
        "\"double\\\"name\""
    );
    assert_eq!(
        engine.complete("cat unicode/", 12)[0].value(),
        "unicode/🚀.fsh"
    );
    assert_eq!(
        engine.complete("cat 'line", 9)[0].value(),
        "\"line\\nbreak\""
    );
}

#[test]
fn wildcard_completion_preserves_the_edited_pattern() {
    let catalog = CompletionCatalog::new().with_paths([
        "scripts/a.fsh",
        "scripts/nested/b.fsh",
        "scripts/nested/deep/c.txt",
        "scripts/.hidden.fsh",
        "scripts/*.fsh",
        "scripts/alpha.fsh",
        "scripts/quo'te.fsh",
    ]);
    let engine = CompletionEngine::new(catalog);
    let source = "let files = glob('scripts/**/*.fs')";
    let cursor = source.find("')").unwrap();
    let completed = engine.complete(source, cursor);

    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].value(), "'scripts/**/*.fsh'");
    assert_eq!(completed[0].replacement(), 17..34);
    assert!(engine.complete("cat scripts/**/.h", 18).is_empty());

    let escaped_source = "let files = glob('scripts/\\*.fs')";
    let escaped_cursor = escaped_source.find("')").unwrap();
    assert_eq!(
        engine.complete(escaped_source, escaped_cursor)[0].value(),
        "'scripts/\\*.fsh'"
    );

    for (source, expected) in [
        (
            "let files = glob('scripts/[a-c]lpha.fs')",
            "'scripts/[a-c]lpha.fsh'",
        ),
        ("let files = glob('scripts/?.fs')", "'scripts/?.fsh'"),
        (
            "let files = glob('scripts/.hidden.fs')",
            "'scripts/.hidden.fsh'",
        ),
        ("let files = glob('scripts/quo')", "\"scripts/quo'te.fsh\""),
    ] {
        let cursor = source.find("')").unwrap();
        assert!(
            engine
                .complete(source, cursor)
                .iter()
                .any(|completion| completion.value() == expected),
            "missing {expected} for {source}"
        );
    }
}

#[test]
fn braced_variables_and_interpolated_path_tails_complete_without_evaluation() {
    let registry = CommandRegistry::new();
    let mut scope = ScopeStack::new();
    scope
        .declare("native", BindingMutability::Immutable, Value::Null)
        .unwrap();
    let catalog = CompletionCatalog::from_runtime(&registry, &scope).with_paths([
        "first",
        "nested/file",
        "other/file",
    ]);
    let engine = CompletionEngine::new(catalog);

    assert_eq!(engine.complete("cat ${na}", 8)[0].value(), "native");
    assert_eq!(
        engine
            .complete("cat $dir/fi", 11)
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["/file", "/first"]
    );
    assert_eq!(
        engine
            .complete("cat $dir/nested/fi", 18)
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["/nested/file"]
    );

    let quoted = "cat \"${dir}/fi\"";
    let cursor = quoted.rfind('"').unwrap();
    assert!(
        engine
            .complete(quoted, cursor)
            .iter()
            .any(|completion| completion.value() == "/first\"")
    );
}
