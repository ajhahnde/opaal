#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_runtime::authority::CapabilityRequest;
use opaal_runtime::help::{ModuleHelpCatalog, render_module_operation_help};
use opaal_runtime::module::ModuleEffect;
use opaal_runtime::operation::OperationPurity;
use support::{load, try_load};

#[test]
fn seven_shared_signatures_reexports_help_and_effects_are_nonexecuting() {
    let program = load(
        "import './api.opaal' as api\nlet input: Bytes = api::io::read_stdin(4)\nio::print('text')\nio::eprintln('text')",
    );
    let catalog = ModuleHelpCatalog::snapshot(&program);
    for (name, signature, effect, request) in [
        (
            "read_stdin",
            "(max_bytes: Int) -> Bytes",
            "stdin.read",
            CapabilityRequest::stdin_read(),
        ),
        (
            "print",
            "(text: String) -> Null",
            "stdout.write",
            CapabilityRequest::stdout_write(),
        ),
        (
            "println",
            "(text: String) -> Null",
            "stdout.write",
            CapabilityRequest::stdout_write(),
        ),
        (
            "eprint",
            "(text: String) -> Null",
            "stderr.write",
            CapabilityRequest::stderr_write(),
        ),
        (
            "eprintln",
            "(text: String) -> Null",
            "stderr.write",
            CapabilityRequest::stderr_write(),
        ),
        (
            "write_stdout",
            "(bytes: Bytes) -> Null",
            "stdout.write",
            CapabilityRequest::stdout_write(),
        ),
        (
            "write_stderr",
            "(bytes: Bytes) -> Null",
            "stderr.write",
            CapabilityRequest::stderr_write(),
        ),
    ] {
        let entry = catalog
            .query(program.graph().root(), &format!("api::io::{name}"))
            .unwrap();
        let descriptor = entry.operation().unwrap();
        assert_eq!(
            descriptor.purity(),
            OperationPurity::RequiresAuthorityContract
        );
        assert_eq!(
            descriptor.signature_labels(),
            [format!("std::io::{name}{signature}")]
        );
        assert_eq!(descriptor.downstream().capability_request(), Some(&request));
        assert!(!descriptor.supports_value_pipeline());
        let help = String::from_utf8(render_module_operation_help(descriptor)).unwrap();
        assert!(help.contains(&format!("effect: {effect} (evaluation)")));
        assert!(help.contains("cannot be rolled back or automatically replayed"));
    }
    let effects = program
        .effects()
        .direct(program.graph().root())
        .occurrences();
    for expected in [
        ModuleEffect::StdinRead,
        ModuleEffect::StdoutWrite,
        ModuleEffect::StderrWrite,
    ] {
        assert!(effects.iter().any(|site| site.effect() == expected));
    }
    for name in ["capture", "prompt", "open", "flush"] {
        assert!(
            catalog
                .query(program.graph().root(), &format!("io::{name}"))
                .is_none()
        );
    }
}

#[test]
fn wrong_types_arity_generics_and_result_types_fail_without_endpoint_access() {
    for body in [
        "io::read_stdin('4')",
        "io::println()",
        "io::print(1)",
        "io::eprintln(1)",
        "io::print[Int]('x')",
        "io::write_stdout('x')",
        "io::write_stderr('x')",
        "let wrong: String = io::read_stdin(0)",
        "let wrong: Int = io::print('x')",
    ] {
        assert!(try_load(body).is_err(), "{body}");
    }
    assert!(try_load("io::read_stdin(-1)").is_ok());
    let program = load("def wrapper() -> Null { return io::eprint('text') }\nwrapper()");
    assert!(
        program
            .effects()
            .direct(program.graph().root())
            .occurrences()
            .iter()
            .any(|site| site.effect() == ModuleEffect::StderrWrite)
    );
    assert!(try_load("action run() -> Null effects { stdin.read(1); } { return null }").is_err());
    assert!(try_load("action run() -> Null effects { stdout.write(); stderr.write(); } { io::print('x')\nreturn io::eprintln('y') }").is_ok());
}
