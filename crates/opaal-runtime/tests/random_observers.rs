#![forbid(unsafe_code)]
#[path = "support/random.rs"]
mod support;

use opaal_runtime::authority::CapabilityRequest;
use opaal_runtime::help::{ModuleHelpCatalog, render_module_operation_help};
use opaal_runtime::module::ModuleEffect;
use opaal_runtime::operation::OperationPurity;
use support::{load, try_load};

#[test]
fn catalog_checker_help_and_reexports_share_exact_effectful_signatures() {
    let program = load(
        "import './api.opaal' as api\nlet shard: Int = api::random::int(0, 4)\nlet fraction: Float = random::float()\nlet bytes: Bytes = random::bytes(16)",
    );
    let catalog = ModuleHelpCatalog::snapshot(&program);
    for (name, suffix) in [
        ("int", "(min: Int, max: Int) -> Int"),
        ("float", "() -> Float"),
        ("bytes", "(count: Int) -> Bytes"),
    ] {
        let entry = catalog
            .query(program.graph().root(), &format!("api::random::{name}"))
            .unwrap();
        let descriptor = entry.operation().unwrap();
        descriptor.validate().unwrap();
        assert_eq!(
            descriptor.purity(),
            OperationPurity::RequiresAuthorityContract
        );
        assert_eq!(
            descriptor.signature_labels(),
            [format!("std::random::{name}{suffix}")]
        );
        assert!(
            descriptor
                .downstream()
                .effects()
                .contains(&CapabilityRequest::entropy_system())
        );
        assert_eq!(
            descriptor.downstream().capability_request(),
            Some(&CapabilityRequest::entropy_system())
        );
        assert!(!descriptor.supports_value_pipeline());
        let help = String::from_utf8(render_module_operation_help(descriptor)).unwrap();
        assert!(help.contains("entropy.system"));
        assert!(
            help.contains("Pure functions, callbacks, initializers and default embeddings refuse")
        );
    }
    for name in ["seed", "state", "shuffle", "distribution"] {
        assert!(
            catalog
                .query(program.graph().root(), &format!("random::{name}"))
                .is_none()
        );
    }
    let direct = program
        .effects()
        .direct(program.graph().root())
        .occurrences();
    assert_eq!(
        direct
            .iter()
            .filter(|site| site.effect() == ModuleEffect::EntropySystem)
            .count(),
        3
    );
}

#[test]
fn wrong_types_arity_generics_unknown_names_and_results_are_checked_without_drawing() {
    for body in [
        "random::int(0.0, 4)",
        "random::int(0, 4.0)",
        "random::int(0)",
        "random::float(1)",
        "random::bytes('3')",
        "random::bytes[Int](3)",
        "random::unknown()",
        "let wrong: Int = random::float()",
        "let wrong: String = random::bytes(0)",
    ] {
        assert!(try_load(body).is_err(), "{body}");
    }
    // Domain checks belong to runtime; static observers do not draw.
    assert!(try_load("random::int(4, 4)\nrandom::bytes(-1)").is_ok());
}

#[test]
fn initializer_summary_traces_wrappers_to_the_canonical_entropy_effect() {
    let program = load("def sample() -> Int { return random::int(0, 4) }\nlet shard = sample()");
    assert!(
        program
            .effects()
            .direct(program.graph().root())
            .occurrences()
            .iter()
            .any(|site| site.effect() == ModuleEffect::EntropySystem)
    );
    let program =
        load("action sample() -> Int effects { entropy.system(); } { return random::int(0, 4) }");
    let action = program
        .actions()
        .action(program.graph().root(), "sample")
        .unwrap();
    assert_eq!(action.effects()[0].capability(), "entropy.system");
    assert!(action.effects()[0].arguments().is_empty());
    assert!(
        try_load("action sample() -> Int effects { entropy.system(1); } { return 1 }").is_err()
    );
}
