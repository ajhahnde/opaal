# Developing OPAAL

Use the repository root for all commands. The pinned Rust toolchain is declared
in `rust-toolchain.toml`; Cargo uses the checked-in lockfile. Python benchmark
and transition validators use only the standard library.

## Build and source gates

```sh
cargo fmt --all -- --check
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo doc --workspace --no-deps --locked
cargo deny check
python3 ci/check_public_boundary.py
git diff --check
```

The normal workspace has exactly seven packages and version
`1.0.0-alpha.1`. A new dependency, feature, crate, binary, environment
protocol, source extension, diagnostic namespace, or language identity is a
public contract change and requires corresponding tests and documentation.

## Focused checks

Useful focused commands include:

```sh
cargo test --locked -p opaal-syntax
cargo test --locked -p opaal-runtime
cargo test --locked -p opaal-cli
cargo test --locked -p opaal-lsp
cargo test --locked -p opaal-migrate
python3 ci/check_transition.py verify --claim TR-004
python3 ci/check_transition.py verify --claim TR-005
python3 ci/check_transition.py verify --claim TR-006
python3 ci/check_transition.py verify --claim TR-007
python3 ci/check_transition.py verify --claim TR-008
python3 ci/check_transition.py verify --claim TR-015
```

Syntax tests own directive, grammar, formatter, property, and source-diagnostic
behavior. Runtime tests own modules, types, operations, outcomes, streams,
resource boundaries, cancellation, and refusal. CLI tests exercise actual
binaries, silent success, exact status/channel behavior, no-write checking,
atomic formatting, and fallback negatives. LSP tests exercise framing,
lifecycle, snapshots, queries, cancellation, and stale-result behavior.
Migration tests own schema 2, deterministic ordering, source/import errors,
limits, digests, exit classes, and zero writes.

## Documentation example

The current example must format, check, and execute under the same language
identity:

```sh
target/debug/opaal format --check examples/language-foundation.opaal
target/debug/opaal check examples/language-foundation.opaal
target/debug/opaal examples/language-foundation.opaal
```

Non-interactive success is silent. Do not introduce an example that depends on
filesystem, process, environment, network, terminal, project, action, task, or
package authority while the foundation remains pure.

## Benchmarks

Run structural/adversarial validation and a quick executable smoke:

```sh
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 benchmarks/run.py --profile smoke
```

The candidate macOS-arm64 evidence command is:

```sh
python3 benchmarks/run.py --profile qualification \
  --output benchmarks/evidence/host-darwin-arm64-opaal-v1.json
```

The runner measures exactly seven host cases and invokes the checker before
reporting success. Do not compare an unmatched host to the tracked budget or
rename effectful predecessor measurements into an OPAAL baseline.

## Fuzzing

Install cargo-fuzz and a nightly Rust toolchain, then run:

```sh
fuzz/run-smoke.sh
fuzz/run-smoke.sh 10000
fuzz/run-campaign.sh 600
```

The scripts seed the lexer, parser, expander, migration, and resource targets
from preserved migration corpora and current OPAAL foundation corpora. Writable
corpora and artifacts stay in temporary or ignored directories. Reproduce and
minimize every failure, then retain a focused regression before removing the
artifact.

## Change discipline

Keep current OPAAL docs, source, fixtures, binaries, diagnostics, and protocol
names coherent. Preserve predecessor material only under `history/` or the
explicit migration corpus. Never use a historical host result to claim current
OPAAL support, and never treat a host pass as external image, Redox, or hardware
qualification.

Publishing a package, tag, release, website, or repository state is outside the
development commands above and requires an explicit release decision.

Every workspace package is marked non-publishable. The manual release-policy
workflow validates that boundary and has no write permission or publishing
step. Pull requests must pass the stable `required` and `security-required`
aggregates; all third-party actions are pinned to full commit identifiers.

[← Documentation index](README.md) · [Architecture](architecture.md)
