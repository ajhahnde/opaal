#![forbid(unsafe_code)]

use opaal_syntax::{
    FormatOutcome, ModuleImportSource, ParseOutcome, SourceFile, SourceId, StatementKind,
    format_source_opaal, parse_opaal,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(7_001), "actions.opaal", text)
}

#[test]
fn actions_effects_tasks_and_project_imports_have_one_ast() {
    let file = source(
        "import project::context as project\n\
         import project::tools as tools\n\
         ## Check one candidate.\n\
         action readiness(candidate: String) -> String\n\
         effects {\n\
             filesystem.read(project::root);\n\
             process.run(tools::git);\n\
         }\n\
         {\n\
             return candidate\n\
         }\n\
         task release = readiness\n",
    );
    let parsed = parse_opaal(&file);
    let ParseOutcome::Complete(script) = parsed else {
        panic!("expected complete action source: {parsed:?}");
    };
    let StatementKind::ModuleImport(import) = script.statements()[0].kind() else {
        panic!("expected project import");
    };
    assert!(matches!(import.source, ModuleImportSource::Project { .. }));
    let StatementKind::Action(action) = script.statements()[2].kind() else {
        panic!("expected action declaration");
    };
    assert_eq!(action.parameters.len(), 1);
    assert_eq!(action.effects.len(), 2);
    assert!(action.documentation.is_some());
    assert!(matches!(
        script.statements()[3].kind(),
        StatementKind::Task(_)
    ));
}

#[test]
fn action_formatting_is_idempotent_and_preserves_effect_order() {
    let file = source(
        "action check(candidate:String)->String effects{process.run(tools::git);clock.wall;}\n{ return candidate }\n",
    );
    let FormatOutcome::Complete(first) = format_source_opaal(&file) else {
        panic!("expected complete formatting");
    };
    let second_file = source(&first);
    let FormatOutcome::Complete(second) = format_source_opaal(&second_file) else {
        panic!("expected formatted source to remain complete");
    };
    assert_eq!(first, second);
    assert_eq!(
        first,
        concat!(
            "action check(candidate: String) -> String\n",
            "effects {\n",
            "    process.run(tools::git);\n",
            "    clock.wall;\n",
            "}\n",
            "{\n",
            "    return candidate\n",
            "}\n",
        )
    );
}

#[test]
fn a_short_action_signature_is_canonicalized_to_one_line() {
    let file = source("action check(\n  candidate: String\n) ->\nString\neffects {}\n{}\n");
    let FormatOutcome::Complete(formatted) = format_source_opaal(&file) else {
        panic!("expected complete formatting");
    };
    assert!(
        formatted.starts_with("action check(candidate: String) -> String\neffects"),
        "{formatted:?}"
    );
}

#[test]
fn malformed_action_forms_do_not_fall_back_to_commands() {
    for text in [
        "action missing() effects {} {}\n",
        "action missing() -> String {}\n",
        "action missing() -> String effects { clock.wall } {}\n",
        "task broken readiness\n",
    ] {
        assert!(
            matches!(parse_opaal(&source(text)), ParseOutcome::Invalid(_)),
            "{text:?}"
        );
    }
}
