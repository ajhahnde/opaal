#![forbid(unsafe_code)]

use opaal_runtime::outcome::{ExecutionOutcome, PrimaryOutcome};
use opaal_runtime::seam::DownstreamCallMetadata;

#[test]
fn pure_source_defaults_remain_empty_and_project_identities_remain_absent() {
    let metadata = DownstreamCallMetadata::default();
    assert!(metadata.is_foundation_only());
    assert_eq!(metadata.evaluation_context(), None);
    assert!(metadata.effects().is_empty());
    assert_eq!(metadata.capability_request(), None);
    assert_eq!(metadata.authority_verdict(), None);
    assert_eq!(metadata.resource_owner(), None);
    assert_eq!(metadata.cancellation_scope(), None);
    assert_eq!(metadata.deadline(), None);
    assert_eq!(metadata.action(), None);
    assert_eq!(metadata.project(), None);
    assert_eq!(metadata.task(), None);

    let outcome = ExecutionOutcome::<_, String>::new(PrimaryOutcome::Completed(7), Vec::new());
    assert_eq!(outcome.primary(), &PrimaryOutcome::Completed(7));
    assert!(outcome.downstream().is_foundation_only());
}
