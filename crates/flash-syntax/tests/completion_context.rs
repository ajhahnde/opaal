#![forbid(unsafe_code)]

use flash_syntax::{CompletionContext, PathCompletionStyle, completion_target};

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
    assert_eq!(
        variable.context(),
        &CompletionContext::Variable { braced: false }
    );
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
    assert_eq!(
        redirect.context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::Bare,
            glob_pattern: false,
            interpolated: false,
        }
    );
    assert_eq!(redirect.prefix(), "./ou");

    let expression = completion_target("let value = int(3.9)", 15)
        .expect("a call callee is an expression completion context");
    assert_eq!(expression.context(), &CompletionContext::Expression);
    assert_eq!(expression.replacement(), 12..15);
    assert_eq!(expression.prefix(), "int");
}

#[test]
fn quoted_and_glob_paths_retain_decoded_prefixes_and_source_styles() {
    let single = completion_target("echo 'two wo", 12).expect("an open quote is classifiable");
    assert_eq!(
        single.context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::SingleQuoted,
            glob_pattern: false,
            interpolated: false,
        }
    );
    assert_eq!(single.replacement(), 5..12);
    assert_eq!(single.prefix(), "two wo");

    let double = completion_target("echo \"quo\\\"", 11).expect("a quoted escape is decoded");
    assert_eq!(
        double.context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::DoubleQuoted,
            glob_pattern: false,
            interpolated: false,
        }
    );
    assert_eq!(double.prefix(), "quo\"");

    let glob_source = "let files = glob('scripts/**/*.fs')";
    let glob_cursor = glob_source.find("')").unwrap();
    let glob =
        completion_target(glob_source, glob_cursor).expect("a glob literal has a path context");
    assert_eq!(
        glob.context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::SingleQuoted,
            glob_pattern: true,
            interpolated: false,
        }
    );
    assert_eq!(glob.prefix(), "scripts/**/*.fs");
}

#[test]
fn executable_and_interpolated_paths_keep_their_grammar_boundaries() {
    let executable = completion_target("^./to", 5).unwrap();
    assert_eq!(
        executable.context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::Bare,
            glob_pattern: false,
            interpolated: false,
        }
    );
    assert_eq!(executable.replacement(), 1..5);

    let interpolated = completion_target("cat $dir/fi", 11).unwrap();
    assert_eq!(
        interpolated.context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::Bare,
            glob_pattern: false,
            interpolated: true,
        }
    );
    assert_eq!(interpolated.replacement(), 8..11);
    assert_eq!(interpolated.prefix(), "/fi");

    let braced = completion_target("cat ${na}", 8).unwrap();
    assert_eq!(
        braced.context(),
        &CompletionContext::Variable { braced: true }
    );
    assert_eq!(braced.replacement(), 6..8);
    assert_eq!(braced.prefix(), "na");

    let quoted_source = "cat \"${dir}/fi\"";
    let quoted_cursor = quoted_source.rfind('"').unwrap();
    let quoted = completion_target(quoted_source, quoted_cursor).unwrap();
    assert_eq!(
        quoted.context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::DoubleQuotedFragment,
            glob_pattern: false,
            interpolated: true,
        }
    );
    assert_eq!(quoted.prefix(), "/fi");
}

#[test]
fn classification_is_checked_but_does_not_require_a_complete_ast() {
    assert!(
        completion_target("é", 1).is_none(),
        "cursor splits a UTF-8 scalar"
    );
    assert_eq!(
        completion_target("echo 'quoted'", 8).unwrap().context(),
        &CompletionContext::Path {
            style: PathCompletionStyle::SingleQuoted,
            glob_pattern: false,
            interpolated: false,
        }
    );
    assert_eq!(
        completion_target("echo value", 5).unwrap().context(),
        &CompletionContext::None
    );

    let modifier = completion_target("let value = $(by", 16)
        .expect("an incomplete modifier remains classifiable");
    assert_eq!(
        modifier.context(),
        &CompletionContext::CommandSubstitutionModifier
    );
    assert_eq!(modifier.prefix(), "by");

    let incomplete = completion_target("echo $(text: ec", 15)
        .expect("a command after a modifier still has a token context");
    assert_eq!(
        incomplete.context(),
        &CompletionContext::Command {
            forced_external: false,
        }
    );
    assert_eq!(incomplete.prefix(), "ec");
}
