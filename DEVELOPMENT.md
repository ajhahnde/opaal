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
python3 ci/check_docs.py --cli target/debug/opaal
python3 ci/check_source_formatting.py --cli target/debug/opaal
git diff --check
```

The workspace has exactly six packages sharing the workspace version. A new
dependency, feature, crate, binary, environment protocol, source extension,
diagnostic namespace, or source identity is a public contract change.

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

## Source formatting

Use one canonical style for named functions and actions: the signature precedes
the body brace on its own line, and actions place `effects {` on a separate line
with semicolon-terminated requests. Control blocks, records and closures retain
their existing layout. Old same-line declarations remain executable; formatting
is an explicit check or rewrite, rather than an execution requirement.

Give the formatter an explicit list of owned project sources:

```sh
target/debug/opaal format --check -- examples/numeric-report/report.opaal examples/numeric-report/tasks.opaal
target/debug/opaal format --write -- examples/numeric-report/report.opaal examples/numeric-report/tasks.opaal
```

Check is read-only and silent on success; noncanonical source reports FMT001.
Write preflights all operands and replaces each changed file atomically,
preserving mode bits. A later replacement failure can leave earlier files
rewritten: inspect and recheck the list. A rewrite changes content digests, so
regenerate project checks and plans before accepting and executing them.

The [source inventory](tests/golden/source-formatting/inventory.json) classifies
every public source, Markdown fence, embedded source owner and fuzz seed.
Positive sources are canonical. Invalid, incomplete and deliberately legacy
fixtures retain their exact bytes and diagnostic assertions; semantic and
runtime negatives retain their owning corpus expectations. Do not run a blanket
write across fixtures. The inventory validator fails on unclassified or missing
sources and requires renewed classification when embedded owners change.

Editors can enable their own format-on-save using LSP document formatting. The
server emits the same bytes as CLI write, independently of tab/space options;
invalid and incomplete buffers receive no edit. Terminal and piped interactive
input continue incomplete signatures/bodies without rewriting the entered text.
Published 1.1 binaries cannot parse the new function layout; upgrade before
migrating source and keep source backups when downgrading.

Replay the formatter, framed language server and fresh/stale project workflows
outside the checkout using built working programs (select the current host):

```sh
python3 ci/qualify_source_formatting.py --binary-directory target/debug \
  --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 \
  --report dist/source-formatting-working.json
```

This records program, fixture and working-source digests without claiming
committed archive qualification. On a clean committed checkout, both host CI
jobs package the separate fixture bundle with `ci/package_source_formatting.py`
and replay it with the exact binary archive, retaining reports in `dist/`.
The fixture project uses the development version range; release preparation
must raise its minimum to 1.2 before qualifying the published candidate.

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
server. See [Qualifying the operational core](RELEASING.md) for the
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

[← Repository overview](README.md) · [Product documentation](https://opaal-lang.org/docs/)
