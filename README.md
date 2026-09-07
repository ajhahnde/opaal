# OPAAL

OPAAL is the Operational Programming & Automation Language. This repository
contains the standalone OPAAL language 1 foundation, its command-line and
editor tooling, and a read-only analyzer for migrating Flash 1 source.

> Project status: `1.0.0-alpha.1` is a development version, not a published
> release. The current foundation is intentionally pure. It does not yet grant
> filesystem, process, environment, network, terminal, project, action, task,
> package, or workflow authority.

## Try the language foundation

Every source module is UTF-8, uses the `.opaal` extension, and begins with
`language 1` as its first non-trivia statement:

```opaal
language 1

import std::value as value

def remaining[T](items: List[T]) -> Int {
    match $items {
        [] => { return 0 }
        [first, ...rest] => { return value::length($rest) }
    }
}

remaining(["build", "test", "review"])
```

Build and exercise the checked-in example from the repository root:

```sh
cargo build --workspace --locked
cargo run --locked -p opaal-cli --bin opaal -- examples/language-foundation.opaal
cargo run --locked -p opaal-cli --bin opaal -- check examples/language-foundation.opaal
cargo run --locked -p opaal-cli --bin opaal -- format --check examples/language-foundation.opaal
```

Successful non-interactive execution is silent; the embedding API retains the
final value. Run `cargo run --locked -p opaal-cli --bin opaal` at a terminal to
start the interactive client, which presents completed values.

## Current surfaces

- `opaal [SCRIPT [ARG...]]` runs one explicitly versioned `.opaal` root.
- `opaal check SOURCE` analyzes a source graph without executing it.
- `opaal format --check|--write PATH...` checks or atomically rewrites source.
- `opaal plan SOURCE` returns the structured `PLAN004` unsupported refusal;
  OPAAL language 1 has no controlled-planning authority yet.
- `opaal-language-server` provides stdio diagnostics, completion, hover,
  signature help, definitions, references, and whole-document formatting.
- `opaal-migrate-flash-v1` analyzes explicit `.fsh` roots without applying
  edits, executing source, discovering projects, or probing tools.

Missing, late, duplicate, malformed, mixed, or `language 2` directives are
rejected before semantic analysis. `.fsh` roots are accepted only by the
migration analyzer. Known effectful syntax is rejected during analysis; a
dynamically reached effect returns a structured refusal before host access.

## Workspace

| Path | Responsibility |
| --- | --- |
| `crates/opaal-syntax/` | OPAAL source, lexer, parser, syntax trees, diagnostics, and feature-gated migration syntax |
| `crates/opaal-migrate/` | Deterministic read-only Flash 1 to OPAAL 1 analysis and schema-2 reports |
| `crates/opaal-runtime/` | Pure values, module analysis, operations, outcomes, streams, and bounded evaluation |
| `crates/opaal-platform/` | Platform capability contracts |
| `crates/opaal-platform-posix/` | macOS/Linux host adapter and observation fixtures |
| `crates/opaal-cli/` | Command-line, formatting, checking, planning-refusal, and interactive frontends |
| `crates/opaal-lsp/` | Non-executing Language Server Protocol adapter |
| `fuzz/` | Separate unpublished `opaal-fuzz` package and five fuzz targets |

The workspace has exactly seven members. The fuzz package is intentionally
separate because cargo-fuzz uses nightly instrumentation.

## Migration and lineage

Flash 1 is a predecessor, not an OPAAL execution mode. Analyze an explicit
source graph with:

```sh
cargo run --locked -p opaal-migrate --bin opaal-migrate-flash-v1 -- \
  --format human legacy.fsh
cargo run --locked -p opaal-migrate --bin opaal-migrate-flash-v1 -- \
  --format json -- legacy.fsh
```

The analyzer emits migration schema 2, preserves source digests and lossless
URIs, applies deterministic resource ceilings, and never writes. Historical
Flash source, tests, documentation, and evidence retained by the extraction
live under [`history/flash-v1/`](history/flash-v1/) and remain historical.

## Verification

The host source gates are:

```sh
cargo fmt --all -- --check
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo doc --workspace --no-deps --locked
cargo deny check
python3 ci/check_transition.py verify --claim TR-004
python3 ci/check_transition.py verify --claim TR-005
python3 ci/check_transition.py verify --claim TR-006
python3 ci/check_transition.py verify --claim TR-007
python3 ci/check_transition.py verify --claim TR-008
python3 ci/check_transition.py verify --claim TR-015
python3 ci/check_public_boundary.py
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 benchmarks/run.py --profile smoke
```

Host success establishes only the exercised macOS/Linux surfaces. This
repository makes no claim that OPAAL is packaged by another operating system,
available on Redox, or qualified on physical hardware.

See the [documentation index](docs/README.md), [language foundation](docs/opaal-language-1-foundation.md),
[architecture](docs/architecture.md), [development guide](docs/development.md),
[contribution guide](CONTRIBUTING.md), [security policy](SECURITY.md), and
[changelog](CHANGELOG.md).

## License

Unless a file states otherwise, OPAAL is licensed under the
[Mozilla Public License 2.0](LICENSE). Preserved historical files retain the
licensing statements that accompanied them.
