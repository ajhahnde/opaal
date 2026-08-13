#![forbid(unsafe_code)]

use flash_syntax::{CompletionContext, completion_target};

#[test]
fn command_variable_flag_and_path_contexts_are_source_spanned() {
    let command = completion_target("ec", 2).expect("an incomplete head remains classifiable");
    assert_eq!(
        command.context(),
        &CompletionContext::Command {
            forced_external: false
        }
    );
    assert_eq!(command.replacement(), 0..2);
    assert_eq!(command.prefix(), "ec");

    let forced = completion_target("^gi", 3).expect("a forced command is classifiable");
    assert_eq!(
        forced.context(),
        &CompletionContext::Command {
            forced_external: true
        }
    );
    assert_eq!(forced.replacement(), 1..3);
    assert_eq!(forced.prefix(), "gi");

    let variable = completion_target("echo $na", 8).expect("a variable is classifiable");
    assert_eq!(variable.context(), &CompletionContext::Variable);
    assert_eq!(variable.replacement(), 5..8);
    assert_eq!(variable.prefix(), "$na");

    let flag = completion_target("inspect --a", 11).expect("a flag is classifiable");
    assert_eq!(
        flag.context(),
        &CompletionContext::Flag {
            command: "inspect".into(),
        }
    );
    assert_eq!(flag.replacement(), 8..11);

    let redirect =
        completion_target("echo value > ./ou", 17).expect("a redirect operand is a path context");
    assert_eq!(redirect.context(), &CompletionContext::Path);
    assert_eq!(redirect.prefix(), "./ou");
}

#[test]
fn classification_is_checked_but_does_not_require_a_complete_ast() {
    assert!(
        completion_target("é", 1).is_none(),
        "cursor splits a UTF-8 scalar"
    );
    assert!(completion_target("echo 'quoted'", 8).is_none());
    assert_eq!(
        completion_target("echo value", 5).unwrap().context(),
        &CompletionContext::None
    );

    let incomplete = completion_target("echo $(ec", 9)
        .expect("an incomplete substitution still has a token context");
    assert_eq!(
        incomplete.context(),
        &CompletionContext::Command {
            forced_external: false,
        }
    );
    assert_eq!(incomplete.prefix(), "ec");
}
