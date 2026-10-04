#![forbid(unsafe_code)]
#[path = "support/random.rs"]
mod support;
use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityProfile, Capabilities, FakePlatform,
};
use opaal_runtime::Value;
use opaal_runtime::authority::{AuthorityContext, AuthorityVerdict, EffectSet};
use opaal_runtime::operational::ModuleError;
use opaal_runtime::operational::random::{RandomLimits, RandomState};
use support::Harness;

struct NoExecutables;
impl opaal_runtime::resolve::ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("random refusal must not probe executables")
    }
}

#[test]
fn default_script_and_retained_session_embeddings_refuse_without_a_binding() {
    use opaal_runtime::builtin::standard_registry;
    use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
    use opaal_runtime::outcome::{PrimaryOutcome, RefusalReason};
    use opaal_runtime::plan::SessionOptions;
    use opaal_runtime::script::{
        execute_ambient_module_program_outcome, execute_module_program_outcome_with_limits,
    };
    use opaal_runtime::session::{Session, SubmitOutcome};
    use opaal_runtime::{Environment, HostEnvironmentLimits, NativeSessionSnapshot};
    use std::path::Path;
    use std::sync::Arc;

    for body in ["random::int(7, 8)", "random::bytes(0)", "random::float()"] {
        let program = support::load(body);
        let outcome = execute_module_program_outcome_with_limits(
            &program,
            &[],
            Path::new("/random"),
            &mut Environment::new(),
            &standard_registry(),
            &NoExecutables,
            &SessionOptions::default(),
            &FakePlatform::full(),
            Arc::new(FakeClock::new()),
            &mut Vec::new(),
            &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::opaal()),
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Refused(refusal) if refusal.reason() == RefusalReason::Unsupported)
        );
        let snapshot = NativeSessionSnapshot::from_snapshot(
            "/random",
            [("PATH", "/tools")],
            HostEnvironmentLimits::OPAAL,
        )
        .unwrap();
        let outcome = execute_ambient_module_program_outcome(
            &program,
            &[],
            snapshot,
            &standard_registry(),
            &NoExecutables,
            &SessionOptions::default(),
            &FakePlatform::full(),
            Arc::new(FakeClock::new()),
            &mut Vec::new(),
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Refused(refusal) if refusal.reason() == RefusalReason::Unsupported)
        );

        let source = support::Source(String::new());
        let mut session = Session::new("/random", Environment::new(), SessionOptions::default());
        let submit = |session: &mut Session, text: &str| {
            session.submit_with_source_loader(
                "cell",
                text,
                &source,
                &source,
                &NoExecutables,
                &FakePlatform::full(),
                &FakeClock::new(),
                &mut Vec::new(),
            )
        };
        assert_eq!(
            submit(
                &mut session,
                "import std::random as random\nlet retained = 42"
            )
            .unwrap()
            .0,
            SubmitOutcome::Continued
        );
        assert!(matches!(
            submit(&mut session, body).unwrap().0,
            SubmitOutcome::Refused(_)
        ));
        assert_eq!(
            submit(&mut session, "retained").unwrap(),
            (SubmitOutcome::Continued, Value::Int(42))
        );
    }
}

#[test]
fn deny_unknown_unsupported_and_missing_binding_refuse_even_no_draw_calls() {
    for case in 0..4 {
        let mut h = Harness::new(&[], &[]);
        let expected = match case {
            0 => {
                h.context = opaal_runtime::context::OperationalContext::new(
                    AuthorityContext::empty(h.context.authority().evaluation()),
                    opaal_runtime::eval::CancellationToken::never(),
                    h.clock.clone(),
                    None,
                );
                AuthorityVerdict::Denied
            }
            1 => {
                h.effects = EffectSet::default();
                AuthorityVerdict::Unknown
            }
            2 => {
                h.platform = FakePlatform::with_authority_profile(
                    Capabilities::full(),
                    AuthorityProfile::enforced().with(
                        AuthorityEffect::EntropySystem,
                        AuthorityEnforcement::Unsupported,
                    ),
                );
                AuthorityVerdict::Unsupported
            }
            _ => {
                h.state = RandomState::new(None, RandomLimits::default()).unwrap();
                AuthorityVerdict::Unsupported
            }
        };
        for (name, args) in [
            ("bytes", vec![Value::Int(0)]),
            ("int", vec![Value::Int(7), Value::Int(8)]),
        ] {
            assert!(
                matches!(h.invoke(name, &args), Err(ModuleError::Authority { verdict }) if verdict == expected)
            );
        }
        assert!(h.fills().is_empty());
    }
}

#[test]
fn another_evaluations_host_cannot_be_retargeted() {
    let mut h = Harness::new(&[], &[]);
    h.state = RandomState::new(
        Some(Box::new(support::Host {
            script: h.script.clone(),
            evaluation: 72,
        })),
        RandomLimits::default(),
    )
    .unwrap();
    assert_eq!(
        h.invoke("bytes", &[Value::Int(0)]).unwrap_err().code(),
        "EXECUTE_STALE"
    );
    assert!(h.fills().is_empty());
}

#[test]
fn unavailable_or_closed_binding_refuses_empty_and_width_one_without_drawing() {
    let mut h = Harness::new(&[], &[]);
    h.script.lock().unwrap().unavailable = true;
    for (name, args) in [
        ("bytes", vec![Value::Int(0)]),
        ("int", vec![Value::Int(7), Value::Int(8)]),
    ] {
        assert!(matches!(
            h.invoke(name, &args),
            Err(ModuleError::Authority {
                verdict: AuthorityVerdict::Unsupported
            })
        ));
        assert_eq!(h.state.progress().admitted_bytes, 0);
    }
    h.state.close().unwrap();
    assert!(matches!(
        h.invoke("bytes", &[Value::Int(0)]),
        Err(ModuleError::Authority {
            verdict: AuthorityVerdict::Unsupported
        })
    ));
    assert!(h.fills().is_empty());
}
