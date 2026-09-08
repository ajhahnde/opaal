#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use opaal_runtime::authority::{AuthorityContext, EvaluationContextId};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancelReason, CancellationToken, FakeClock, Instant};
use opaal_runtime::lifetime::{CleanupStatus, Deadline};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::security::{REDACTED, Secret, SecretId};

fn evaluation() -> EvaluationContextId {
    EvaluationContextId::new(23).expect("the fixture identity is nonzero")
}

fn context(
    token: CancellationToken,
    clock: FakeClock,
    deadline: Option<Deadline>,
) -> OperationalContext {
    OperationalContext::new(
        AuthorityContext::empty(evaluation()),
        token,
        Arc::new(clock),
        deadline,
    )
}

#[test]
fn cancellation_and_deadline_are_sticky_with_caller_precedence() {
    let requested = Arc::new(AtomicBool::new(false));
    let clock = FakeClock::new();
    let scope = context(
        CancellationToken::from_fn({
            let requested = Arc::clone(&requested);
            move || requested.load(Ordering::SeqCst)
        }),
        clock.clone(),
        Some(Deadline::at(Instant::from_nanos(10))),
    );

    clock.advance(9);
    assert_eq!(scope.poll_cancellation(), None);
    clock.advance(1);
    assert_eq!(scope.poll_cancellation(), Some(CancelReason::Timeout));
    requested.store(true, Ordering::SeqCst);
    assert_eq!(
        scope.poll_cancellation(),
        Some(CancelReason::Timeout),
        "the first observed reason remains sticky"
    );

    let requested = Arc::new(AtomicBool::new(true));
    let simultaneous = context(
        CancellationToken::from_fn({
            let requested = Arc::clone(&requested);
            move || requested.load(Ordering::SeqCst)
        }),
        FakeClock::at(10),
        Some(Deadline::at(Instant::from_nanos(10))),
    );
    assert_eq!(
        simultaneous.poll_cancellation(),
        Some(CancelReason::Requested),
        "an already requested caller cancellation wins the first poll"
    );
}

#[test]
fn caller_token_reason_is_preserved() {
    let clock = FakeClock::at(10);
    let scope = context(
        CancellationToken::deadline(clock.clone(), Instant::from_nanos(10)),
        clock,
        None,
    );
    assert_eq!(
        scope.poll_cancellation(),
        Some(CancelReason::Timeout),
        "a caller-supplied timeout token must not be relabeled as requested"
    );
}

#[test]
fn a_deadline_can_only_be_narrowed() {
    let mut context = context(
        CancellationToken::never(),
        FakeClock::new(),
        Some(Deadline::at(Instant::from_nanos(100))),
    );
    context.narrow_deadline(Deadline::at(Instant::from_nanos(200)));
    assert_eq!(
        context.cancellation().deadline(),
        Some(Deadline::at(Instant::from_nanos(100)))
    );
    context.narrow_deadline(Deadline::at(Instant::from_nanos(50)));
    assert_eq!(
        context.cancellation().deadline(),
        Some(Deadline::at(Instant::from_nanos(50)))
    );
}

#[test]
fn finish_cleans_every_resource_once_in_lifo_order_and_retains_the_primary() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut context = context(CancellationToken::never(), FakeClock::new(), None);
    context
        .insert_secret(
            Secret::new(
                SecretId::new("token").expect("the identity is valid"),
                b"canary".to_vec(),
            )
            .expect("the payload is nonempty"),
        )
        .expect("the identity is unique");

    for (name, fails) in [("first", false), ("second", true), ("third", false)] {
        let events = Arc::clone(&events);
        context
            .register_resource(move || {
                events
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(name);
                if fails {
                    Err("cleanup exposed canary".to_owned())
                } else {
                    Ok(())
                }
            })
            .expect("the scope is open");
    }

    let outcome = context.finish(PrimaryOutcome::<_, String>::Completed(41), Vec::new());
    assert_eq!(outcome.primary(), &PrimaryOutcome::Completed(41));
    assert_eq!(
        events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_slice(),
        ["third", "second", "first"]
    );

    let cleanup = outcome.downstream().cleanup();
    assert_eq!(cleanup.len(), 3);
    assert_eq!(cleanup[0].resource().ordinal(), 2);
    assert_eq!(cleanup[1].resource().ordinal(), 1);
    assert_eq!(cleanup[2].resource().ordinal(), 0);
    assert_eq!(cleanup[0].status(), &CleanupStatus::Succeeded);
    assert_eq!(
        cleanup[1].status(),
        &CleanupStatus::Failed(format!("cleanup exposed {REDACTED}"))
    );
    assert_eq!(cleanup[2].status(), &CleanupStatus::Succeeded);
    assert_eq!(
        outcome.downstream().evaluation_context(),
        Some(evaluation())
    );
}

#[test]
fn dropping_an_unfinished_context_still_cleans_once() {
    let cleanups = Arc::new(AtomicUsize::new(0));
    {
        let mut context = context(CancellationToken::never(), FakeClock::new(), None);
        let cleanups = Arc::clone(&cleanups);
        context
            .register_resource(move || {
                cleanups.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .expect("the scope is open");
    }
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
}
