# OPAAL

OPAAL is the Operational Programming & Automation Language. This repository
contains its standalone syntax, runtime, command-line client, platform
contracts, POSIX adapter, and Language Server Protocol implementation.

> Project status: `1.0.0-alpha.1` is an unreleased development version. The
> current authoring surface is intentionally non-operational: typed actions and
> explicit project tasks can be inspected and checked, but declared effects
> cannot execute. Package and controlled workflow execution remain absent.

## Try OPAAL

OPAAL modules are UTF-8 `.opaal` files containing ordinary program text:

```opaal
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
final value. Running `opaal` without a script in a terminal starts the
interactive client, which presents completed values.

## Current surfaces

- `opaal [SCRIPT [ARG...]]` runs one explicit `.opaal` root.
- `opaal check SOURCE` analyzes a source graph without executing it.
- `opaal check --project opaal.toml ...` validates one explicit task,
  environment, authority document, tool lock, and typed input set without
  executing an action or adapter.
- `opaal task inspect --project opaal.toml TASK` reports the task's shared
  action signature, declared effects, tools, and environments.
- `opaal format --check|--write PATH...` checks or atomically rewrites source.
- `opaal plan SOURCE` returns the structured `PLAN004` unsupported refusal;
  controlled planning authority is not implemented.
- `opaal-language-server` provides stdio diagnostics, completion, hover,
  signature help, definitions, references, and whole-document formatting.

Effectful action declarations are statically analyzed, but invoking an action
with any declared effect returns a structured refusal before host access. Pure
actions retain the existing evaluator route.

## Workspace

| Path | Responsibility |
| --- | --- |
| `crates/opaal-syntax/` | Source, lexer, parser, syntax trees, formatting, and diagnostics |
| `crates/opaal-runtime/` | Pure values, module analysis, operations, outcomes, streams, and bounded evaluation |
| `crates/opaal-platform/` | Platform capability contracts |
| `crates/opaal-platform-posix/` | macOS/Linux host adapter and observation fixtures |
| `crates/opaal-cli/` | Command-line, checker, formatter, planning-refusal, and interactive frontends |
| `crates/opaal-lsp/` | Non-executing Language Server Protocol adapter |
| `fuzz/` | Separate unpublished package with four fuzz targets |

The Cargo workspace has exactly six members. The fuzz package has its own
workspace so nightly instrumentation does not change the normal locked graph.

## Verification

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
python3 benchmarks/run.py --profile smoke
```

The manual release-policy workflow runs the read-only, fail-closed
`python3 ci/check_product.py unpublished` check. No workflow can publish.

Host success establishes only the exercised macOS/Linux surfaces. It does not
claim packaging by another operating system, Redox support, or physical
hardware qualification.

See the [documentation index](docs/README.md), [language foundation](docs/language-foundation.md),
[actions and explicit projects](docs/actions-and-projects.md),
[architecture](docs/architecture.md), [development guide](docs/development.md),
[contribution guide](CONTRIBUTING.md), [security policy](SECURITY.md), and
[changelog](CHANGELOG.md).

## License

Unless a file states otherwise, OPAAL is licensed under the
[Mozilla Public License 2.0](LICENSE).
