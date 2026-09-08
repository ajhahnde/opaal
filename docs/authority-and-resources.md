# Authority and resource embedding

OPAAL exposes one platform-independent `OperationalContext` for embedders that
own a future controlled operation. The context combines explicit authority,
adapter enforcement, cancellation, a monotonic deadline, injected-secret
redaction, and adapter-owned resource cleanup. Constructing it does not enable
effects in OPAAL source.

## Authority

An `AuthorityContext` belongs to one nonzero `EvaluationContextId`. It contains
only exact deny or grant rows over these typed requests:

- project-path filesystem read or write;
- maintained-tool process execution;
- endpoint-and-method HTTP access;
- one secret, endpoint, and header sink tuple; and
- wall or monotonic clock observation for the evaluation.

There are no wildcards, prefixes, parent/child path inference, combined grants,
ambient defaults, or source-created authority. More than one row for the same
request refuses construction, including a grant/deny conflict. A missing row
is denied. One evaluation accepts at most 256 authority rows.

The platform adapter reports whether the exact boundary is enforced,
unenforced, unsupported, or unknown. Authority and adapter truth combine into
exactly five public verdicts: `Denied`, `Unknown`, `Unsupported`,
`GrantedEnforced`, and `GrantedUnenforced`. An unenforced boundary is executable
only when its exact grant says `AcknowledgeUnenforced`. An effect outside the
call's declared `EffectSet` is `Unknown`, even if a matching grant exists.
`AcknowledgeUnenforced` is valid only for maintained process execution; direct
filesystem, HTTP, secret-sink, and clock effects must be fully enforced.

`FakePlatform::with_authority_profile` provides a deterministic enforcement
schedule. Ordinary fake and POSIX constructors report every operational effect
as unsupported until a maintained bounded adapter owns and enforces that exact
scope. Existing low-level platform capabilities never imply authority.

## Cancellation and deadlines

Each context owns one `CancellationScope`. It polls caller cancellation before
its monotonic deadline and preserves the caller token's reason. The first
observed reason is sticky. A later phase may narrow a deadline but cannot
extend it; exact equality with the deadline is a timeout. `FakeClock` makes
every boundary deterministic in tests.

## Owned resources

An embedder registers cleanup before exposing an adapter handle. Registrations
receive source-order ordinals beneath one evaluation-owned `ResourceOwnerId`.
Finishing or dropping the context runs every cleanup exactly once in reverse
registration order. Cleanup continues after a failure. `finish` retains the
original primary outcome and attaches every cleanup result in observation
order; secret text in cleanup failures is redacted.

## Secrets and redaction

`Secret` is non-cloneable and has no payload accessor, display implementation,
serialization implementation, equality, or hashing. Secrets enter only through
explicit `OperationalContext::insert_secret`; the runtime never discovers an
environment value, credential file, cache, or account session. Dropping the
owned value overwrites its buffer, and each redaction pass clears its temporary
raw and encoded representations. One evaluation accepts at most eight secrets,
each no larger than 64 KiB.

Debug output exposes only the `SecretId` and `[redacted]`. The context redactor
replaces raw bytes plus lowercase hexadecimal, standard base64, unpadded
base64url, percent-encoded, and JSON-escaped representations before diagnostic
or evidence text leaves the embedding boundary. If a replacement marker would
overlap a registered representation, the complete sink value fails closed to
one marker rather than preserving ambiguous surrounding bytes. Secret
materialization has no public operation in this API; a later maintained typed
sink must add the sole consuming route without widening this contract.

## Pure-source compatibility

Pure function and operation descriptors still receive
`DownstreamCallMetadata::foundation()`. Ordinary `ExecutionOutcome::new`
receives empty downstream metadata. Existing `.opaal` parsing, checking,
planning refusal, evaluation, diagnostics, output bytes, and host-access
refusals are unchanged. Action, project, task, tool, and environment identities
remain absent opaque slots.

[← Documentation index](README.md) · [Architecture](architecture.md)
