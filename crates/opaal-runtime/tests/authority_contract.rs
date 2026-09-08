#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityProfile, Capabilities, FakePlatform,
};
use opaal_runtime::authority::{
    AuthorityContext, AuthorityContextError, AuthorityRule, AuthorityVerdict, CapabilityRequest,
    EffectSet, EvaluationContextId, MAX_AUTHORITY_RULES, RequiredEnforcement,
};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::FakeClock;

fn evaluation() -> EvaluationContextId {
    EvaluationContextId::new(17).expect("the fixture identity is nonzero")
}

fn read_request(path: &str) -> CapabilityRequest {
    CapabilityRequest::filesystem_read(PathBuf::from(path)).expect("the fixture path is nonempty")
}

#[test]
fn exact_rules_and_adapter_enforcement_produce_all_five_verdicts() {
    let request = read_request("/project/input");
    let denied_request = read_request("/project/denied");
    let authority = AuthorityContext::new(
        evaluation(),
        [
            AuthorityRule::grant(request.clone(), RequiredEnforcement::Enforced),
            AuthorityRule::deny(denied_request.clone()),
        ],
    )
    .expect("the rules are unique");

    let enforced = FakePlatform::with_authority_profile(
        Capabilities::full(),
        AuthorityProfile::unsupported().with(
            AuthorityEffect::FilesystemRead,
            AuthorityEnforcement::Enforced,
        ),
    );
    let unknown = FakePlatform::with_authority_profile(
        Capabilities::full(),
        AuthorityProfile::unsupported().with(
            AuthorityEffect::FilesystemRead,
            AuthorityEnforcement::Unknown,
        ),
    );
    let unsupported =
        FakePlatform::with_authority_profile(Capabilities::full(), AuthorityProfile::unsupported());

    assert_eq!(
        authority.verdict(&request, &enforced),
        AuthorityVerdict::GrantedEnforced
    );
    assert_eq!(
        authority.verdict(&denied_request, &enforced),
        AuthorityVerdict::Denied
    );
    assert_eq!(
        authority.verdict(&read_request("/project/missing"), &enforced),
        AuthorityVerdict::Denied
    );
    assert_eq!(
        authority.verdict(&request, &unsupported),
        AuthorityVerdict::Unsupported
    );
    assert_eq!(
        authority.verdict(&request, &unknown),
        AuthorityVerdict::Unknown
    );

    let process = CapabilityRequest::process_run("cargo").expect("the tool id is valid");
    let acknowledged = AuthorityContext::new(
        evaluation(),
        [AuthorityRule::grant(
            process.clone(),
            RequiredEnforcement::AcknowledgeUnenforced,
        )],
    )
    .expect("the rule is valid");
    let unenforced = FakePlatform::with_authority_profile(
        Capabilities::full(),
        AuthorityProfile::unsupported().with(
            AuthorityEffect::ProcessRun,
            AuthorityEnforcement::Unenforced,
        ),
    );
    assert_eq!(
        acknowledged.verdict(&process, &unenforced),
        AuthorityVerdict::GrantedUnenforced
    );
}

#[test]
fn duplicate_conflicting_and_widened_rules_fail_closed() {
    let request = read_request("/project/exact");
    let error = AuthorityContext::new(
        evaluation(),
        [
            AuthorityRule::grant(request.clone(), RequiredEnforcement::Enforced),
            AuthorityRule::deny(request.clone()),
        ],
    )
    .expect_err("two rows for one exact request must be refused");
    assert_eq!(error, AuthorityContextError::DuplicateRule(request.clone()));

    let authority = AuthorityContext::new(
        evaluation(),
        [AuthorityRule::grant(request, RequiredEnforcement::Enforced)],
    )
    .expect("one exact row is valid");
    assert_eq!(
        authority.verdict(&read_request("/project/exact/child"), &FakePlatform::full()),
        AuthorityVerdict::Denied,
        "a parent or child path is not inferred from an exact path grant"
    );
}

#[test]
fn authority_rule_ceiling_is_enforced_before_use() {
    let rules = (0..=MAX_AUTHORITY_RULES).map(|index| {
        AuthorityRule::deny(
            CapabilityRequest::process_run(format!("tool-{index}"))
                .expect("generated identities are nonempty"),
        )
    });
    assert_eq!(
        AuthorityContext::new(evaluation(), rules),
        Err(AuthorityContextError::TooManyRules {
            count: MAX_AUTHORITY_RULES + 1,
            max: MAX_AUTHORITY_RULES,
        })
    );
}

#[test]
fn unacknowledged_child_opacity_is_not_executable() {
    let request = CapabilityRequest::process_run("cargo").expect("the tool id is valid");
    let authority = AuthorityContext::new(
        evaluation(),
        [AuthorityRule::grant(
            request.clone(),
            RequiredEnforcement::Enforced,
        )],
    )
    .expect("the rule is valid");
    let platform = FakePlatform::with_authority_profile(
        Capabilities::full(),
        AuthorityProfile::unsupported().with(
            AuthorityEffect::ProcessRun,
            AuthorityEnforcement::Unenforced,
        ),
    );

    let verdict = authority.verdict(&request, &platform);
    assert_eq!(verdict, AuthorityVerdict::Unsupported);
    assert!(!verdict.is_executable());
}

#[test]
fn unenforced_acknowledgement_is_process_only() {
    let request = read_request("/project/input");
    assert_eq!(
        AuthorityContext::new(
            evaluation(),
            [AuthorityRule::grant(
                request.clone(),
                RequiredEnforcement::AcknowledgeUnenforced,
            )],
        ),
        Err(AuthorityContextError::InvalidUnenforcedAcknowledgement(
            request,
        ))
    );
}

#[test]
fn one_context_rejects_an_undeclared_request_and_exposes_exact_metadata() {
    let declared = read_request("/project/input");
    let undeclared = read_request("/project/other");
    let authority = AuthorityContext::new(
        evaluation(),
        [AuthorityRule::grant(
            declared.clone(),
            RequiredEnforcement::Enforced,
        )],
    )
    .expect("the rule is valid");
    let context = OperationalContext::new(
        authority,
        opaal_runtime::eval::CancellationToken::never(),
        Arc::new(FakeClock::new()),
        None,
    );
    let effects = EffectSet::new([declared.clone()]);
    let platform = FakePlatform::with_authority_profile(
        Capabilities::full(),
        AuthorityProfile::unsupported().with(
            AuthorityEffect::FilesystemRead,
            AuthorityEnforcement::Enforced,
        ),
    );

    assert_eq!(
        context.verdict(&effects, &undeclared, &platform),
        AuthorityVerdict::Unknown
    );
    let metadata = context.call_metadata(effects, Some(declared.clone()), &platform);
    assert_eq!(metadata.evaluation_context(), Some(evaluation()));
    assert_eq!(metadata.capability_request(), Some(&declared));
    assert_eq!(
        metadata.authority_verdict(),
        Some(AuthorityVerdict::GrantedEnforced)
    );
    assert!(metadata.project().state() == opaal_runtime::seam::OpaqueSlotState::Absent);
}
