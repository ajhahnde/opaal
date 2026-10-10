#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityProfile, Capabilities, FakePlatform,
};
use opaal_runtime::Value;
use opaal_runtime::authority::{AuthorityContext, AuthorityVerdict, EffectSet};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::CancellationToken;
use opaal_runtime::operational::ModuleError;
use opaal_runtime::operational::standard::{StandardLimits, StandardState};
use support::{Harness, Host};

#[test]
fn separate_denied_undeclared_unsupported_and_occupied_streams_never_transfer() {
    for (name, effect, stream, value) in [
        ("read_stdin", AuthorityEffect::StdinRead, 0, Value::Int(0)),
        ("print", AuthorityEffect::StdoutWrite, 1, Value::string("")),
        ("eprint", AuthorityEffect::StderrWrite, 2, Value::string("")),
    ] {
        for case in 0..5 {
            let mut h = Harness::new(b"sentinel");
            let expected = match case {
                0 => {
                    h.context = OperationalContext::new(
                        AuthorityContext::empty(h.context.authority().evaluation()),
                        CancellationToken::never(),
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
                        AuthorityProfile::enforced()
                            .with(effect, AuthorityEnforcement::Unsupported),
                    );
                    AuthorityVerdict::Unsupported
                }
                3 => {
                    h.script.lock().unwrap().available[stream] = false;
                    AuthorityVerdict::Unsupported
                }
                _ => {
                    h.state = StandardState::new(None, StandardLimits::default()).unwrap();
                    AuthorityVerdict::Unsupported
                }
            };
            assert!(
                matches!(h.invoke(name, std::slice::from_ref(&value)), Err(ModuleError::Authority { verdict }) if verdict == expected)
            );
            assert!(h.script.lock().unwrap().calls.is_empty());
            assert_eq!(h.state.consumed_bytes(), 0);
        }
    }
}

#[test]
fn a_bound_endpoint_cannot_be_retargeted_to_another_evaluation_or_reused_after_close() {
    let mut h = Harness::new(b"sentinel");
    h.state = StandardState::new(
        Some(Box::new(Host {
            script: h.script.clone(),
            evaluation: 72,
        })),
        StandardLimits::default(),
    )
    .unwrap();
    assert_eq!(
        h.invoke("read_stdin", &[Value::Int(0)]).unwrap_err().code(),
        "EXECUTE_STALE"
    );
    assert!(h.script.lock().unwrap().calls.is_empty());
    h.state.close().unwrap();
    assert!(matches!(
        h.invoke("print", &[Value::string("")]),
        Err(ModuleError::Authority {
            verdict: AuthorityVerdict::Unsupported
        })
    ));
}
