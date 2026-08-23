#![no_main]

use flash_runtime::eval::expand_word;
use flash_runtime::{BindingMutability, ScopeStack, Value};
use flash_syntax::{
    CommandItemKind, ParseOutcome, RedirectionKind, SourceFile, SourceId, StageKind, StatementKind,
    Word, parse,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = SourceFile::from_bytes(SourceId::new(0), "fuzz", data.to_vec()) else {
        return;
    };
    let ParseOutcome::Complete(script) = parse(&source) else {
        return;
    };

    for statement in script.statements() {
        let StatementKind::Job(job) = statement.kind() else {
            continue;
        };
        for or_term in job.chain.or_terms() {
            for pipeline in or_term.and_terms() {
                for stage in pipeline.stages() {
                    let StageKind::Command(command) = stage.kind() else {
                        continue;
                    };
                    exercise_word(command.head.word(), &source);
                    for item in &command.items {
                        match item.kind() {
                            CommandItemKind::Word(word) => exercise_word(word, &source),
                            CommandItemKind::Redirection(redirection) => match redirection.kind() {
                                RedirectionKind::Input { target, .. } => {
                                    exercise_word(target, &source);
                                }
                                RedirectionKind::File(file) => {
                                    exercise_word(&file.target, &source);
                                }
                                RedirectionKind::Duplicate { .. }
                                | RedirectionKind::Close { .. } => {}
                            },
                            CommandItemKind::Spread(_) | CommandItemKind::Closure(_) => {}
                        }
                    }
                }
            }
        }
    }
});

fn exercise_word(word: &Word, source: &SourceFile) {
    let mut scope = seeded_scope();
    let result = expand_word(word, source, &mut scope);

    if let Ok(expanded) = result {
        assert_eq!(expanded.span(), word.span());
        for part in expanded.parts() {
            assert_eq!(part.source_id(), word.span().source_id());
            assert!(part.start() >= word.span().start());
            assert!(part.end() <= word.span().end());
        }
    }
}

fn seeded_scope() -> ScopeStack {
    let mut scope = ScopeStack::new();
    for (name, value) in [
        ("name", Value::string("Flash")),
        ("count", Value::Int(-7)),
        ("flag", Value::Bool(true)),
        ("nothing", Value::Null),
        (
            "items",
            Value::list(vec![Value::Int(1), Value::string("two")]),
        ),
    ] {
        scope
            .declare(name, BindingMutability::Immutable, value)
            .expect("the fixed expansion seed bindings are unique");
    }
    scope
}
