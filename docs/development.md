# Developing OPAAL

Use the repository root for all commands. The pinned Rust toolchain is declared
in `rust-toolchain.toml`; Cargo uses the checked-in lockfile. Python validators
use only the standard library.

## Build and source gates

```sh
cargo fmt --all -- --check
cargo build --workspace --locked
cargo test --workspace --locked --no-fail-fast
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo doc --workspace --no-deps --locked
cargo deny check
python3 -m unittest discover -s ci/tests -p 'test_*.py'
python3 ci/check_product.py source
python3 ci/check_public_boundary.py
git diff --check
```

The workspace has exactly six packages at `1.0.0`. A new dependency,
feature, crate, binary, environment protocol, source extension, diagnostic
namespace, or source identity is a public contract change.

## Focused checks

```sh
cargo test --locked -p opaal-syntax
cargo test --locked -p opaal-runtime
cargo test --locked -p opaal-cli
cargo test --locked -p opaal-lsp
```

Syntax tests own grammar, lexer, formatter, property, and source-diagnostic
behavior. Runtime tests own modules, types, operations, outcomes, streams,
resources, cancellation, and refusal. CLI tests exercise binaries, silent
success, status channels, no-write checking, atomic formatting, and input
negatives. LSP tests exercise framing, lifecycle, snapshots, queries,
cancellation, stale-result behavior, explicit project selection, disk-backed
module loading, editor-local overlays, and standalone fallbacks.

## Documentation example

```sh
target/debug/opaal format --check examples/language-foundation.opaal
target/debug/opaal check examples/language-foundation.opaal
target/debug/opaal examples/language-foundation.opaal
```

Pure non-interactive evaluation does not print its final value. Examples must not depend on authority that
the pure foundation does not grant.

## Benchmarks

```sh
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 benchmarks/run.py --profile smoke
python3 benchmarks/run.py --profile qualification \
  --budget-environment host-darwin-arm64 \
  --output benchmarks/evidence/host-darwin-arm64-candidate.json
```

The runner measures exactly eleven host cases, including the four operational
artifact cases, and invokes the checker before reporting success. Evidence is
bound to the candidate binary and matching host. Budget comparison is explicit
because a different host, including a shared CI runner, may validate the full
profile without claiming equivalence to the retained evidence host.

## Operational-core qualification

```sh
python3 ci/qualify_operational_core.py --profile qualification
```

This creates an isolated temporary project and proves the complete
non-publishing check, plan, accepted-execution, journal, and audit path on Linux.
On macOS arm64 it proves secret-free execution/audit plus the explicit refused
process boundary. The harness uses only synthetic inputs and a TLS loopback
server. See [Qualifying the operational core](release-readiness.md) for the
scenario and evidence boundary.

## Fuzzing

Install cargo-fuzz and a nightly Rust toolchain, then run:

```sh
fuzz/run-smoke.sh
fuzz/run-smoke.sh 10000
fuzz/run-campaign.sh 600
```

The scripts seed lexer, parser, expander, and resource targets from current
OPAAL corpora. Writable corpora and artifacts stay in temporary or ignored
directories. Reproduce and minimize failures, then retain a focused regression.

## Change discipline

Keep source, fixtures, binaries, diagnostics, docs, and protocol names coherent.
Do not use a host pass to claim external image, target, Redox, or hardware
qualification.

Every package is non-publishable. The manual release-policy workflow performs
only read-only validation. Maintainer pull requests must pass the stable `required` and
`security-required` aggregates; third-party actions are pinned to full commit
identifiers.

[← Documentation index](README.md) · [Source and project references](reference/README.md)
