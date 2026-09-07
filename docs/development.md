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

The workspace has exactly six packages at `1.0.0-alpha.1`. A new dependency,
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
cancellation, and stale-result behavior.

## Documentation example

```sh
target/debug/opaal format --check examples/language-foundation.opaal
target/debug/opaal check examples/language-foundation.opaal
target/debug/opaal examples/language-foundation.opaal
```

Non-interactive success is silent. Examples must not depend on authority that
the pure foundation does not grant.

## Benchmarks

```sh
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 benchmarks/run.py --profile smoke
python3 benchmarks/run.py --profile qualification \
  --output benchmarks/evidence/host-darwin-arm64-candidate.json
```

The runner measures exactly seven host cases and invokes the checker before
reporting success. Evidence is bound to the candidate binary and matching host.

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
only read-only validation. Pull requests must pass the stable `required` and
`security-required` aggregates; third-party actions are pinned to full commit
identifiers.

[← Documentation index](README.md) · [Architecture](architecture.md)
