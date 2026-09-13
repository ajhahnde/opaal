# Qualifying the operational core

OPAAL carries one permanent, non-publishing release-readiness workflow. It
proves that the language, explicit project model, authority decisions,
maintained host operations, accepted execution, and redacted evidence work as
one coherent path without creating a package, tag, artifact upload, or release.

Run the qualification from the repository root:

```sh
python3 ci/qualify_operational_core.py --profile qualification
```

The command builds `target/release/opaal` when needed, validates the repository
product/public/benchmark contracts, and copies the checked-in
`tests/golden/release-readiness/` project into a temporary isolated repository.
The temporary repository has fresh `HOME`, `TMPDIR`, `CARGO_HOME`, and empty Git
configuration paths, and every OPAAL invocation receives only that closed
environment. The harness fingerprints the exact installed Git and Cargo
executables and resolves the pinned Rust compiler and documentation executables
to generate a strict host tool lock; it does not copy or point at account
credentials, user configuration, or package caches.

## Exercised workflow

The independent harness performs these public operations:

1. Inspect and check a secret-free task, render its plan twice, accept it,
   execute it, audit its journal, and render the audit twice.
2. Remove the terminal journal record and prove the valid prefix becomes an
   inspectable `incomplete` audit rather than a false success or corruption.
3. Check and plan a task with two secret sinks and prove it is sealed as a
   non-executable refusal before stdin or an adapter can be used.
4. Inspect, check, and plan the process-bearing readiness task against exact
   candidate, source, manifest, lock, authority, TLS, and tool identities.
5. On Linux, explicitly accept the plan, run locked Git and Cargo operations,
   contact only the fixed TLS loopback endpoint with a synthetic one-use secret,
   write readiness evidence, and audit the complete journal.
6. On macOS, prove the same process-bearing task becomes a visible refused plan
   before journal creation, secret consumption, a task adapter call, or
   pathname process execution. Secret-free execution and audit remain
   available.

Every displayed or persisted output is scanned for the raw synthetic secret
and its hexadecimal, standard and URL-safe base64, and percent-encoded forms.
The Linux loopback server verifies the exact method, path, and secret header,
observes one request, and is owned and reaped by the harness. The readiness
action runs `git diff --quiet` only against
the temporary workspace manifest and lock, then runs `cargo test --workspace
--locked` for that minimal non-publishable fixture. It has no remote mutation or
publication route.

## Evidence and limits

Success prints one canonical JSON summary containing the candidate digest,
exact host triple, exercised scenarios, process-execution classification, and
`"publication":"not-performed"`. Generated plans, journals, audits, build
products, Git metadata, tool locks, and synthetic inputs remain in the temporary
workspace and are removed after the run.

Pull-request CI runs the full workflow on Linux and the unsupported-process
boundary on Apple-silicon macOS. Linux also exercises the performance smoke;
macOS runs the full retained qualification profile against its checked host
budget. The stable `required` aggregate also requires workspace
build/test/lint/docs, repository policy, fuzz smoke, and both host jobs.
`security-required` independently retains dependency, repository, license,
advisory, ban, and source-policy checks.

Passing these gates makes the exact revision eligible for a separate
operator-controlled release review. It does not change the development version,
publish a crate, package a binary, create a tag or release, qualify another
operating-system image, or establish downstream hardware support.

[← Documentation index](README.md) · [Actions and explicit projects](actions-and-projects.md) · [Development](development.md)
