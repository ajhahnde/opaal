# OPAAL

OPAAL is the Operational Programming & Automation Language. This repository
contains its standalone syntax, runtime, command-line client, platform
contracts, POSIX adapter, and Language Server Protocol implementation.

> Project status: `1.0.0-alpha.1` is an unreleased development version. The
> explicit project surface can inspect, check, render, explicitly accept,
> execute, journal, and audit one typed task under bound authority. Standalone
> source remains non-operational, and packages remain absent.

## Try OPAAL

OPAAL modules are UTF-8 `.opaal` files containing ordinary program text:

```opaal
import std::value as value

def remaining[T](items: List[T]) -> Int {
    match items {
        [] => { return 0 }
        [first, ...rest] => { return value::length(rest) }
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

OPAAL uses one value language throughout: bare names read bindings, the final
expression is a block's value, `{expression}` interpolates into a command word,
`...{expression}` spreads a list, and only a literal head prefixed with `^`
denotes an external program. Dollar-prefixed forms and implicit external
command fallback are unsupported.

## Current surfaces

- `opaal [SCRIPT [ARG...]]` runs one explicit `.opaal` root.
- `opaal check SOURCE` analyzes a source graph without executing it.
- `opaal check --project opaal.toml ...` validates one explicit task,
  environment, authority document, tool lock, and typed input set without
  executing an action or adapter; `--format json` emits its canonical check
  artifact.
- `opaal task inspect --project opaal.toml TASK` reports the task's shared
  action signature, declared effects, tools, and environments.
- `opaal format --check|--write PATH...` checks or atomically rewrites source.
- `opaal plan SOURCE` returns the structured `PLAN004` unsupported refusal;
  `opaal plan --project opaal.toml ... --out PATH` writes one canonical,
  identity-bound, expiring plan without executing the task or probing tools.
- `opaal execute --plan PATH --accept DIGEST ... --journal PATH` revalidates
  and, on a supported execution host, runs exactly one accepted project plan
  under its explicit authority and writes a synced hash-chained journal.
- `opaal audit --project opaal.toml --journal PATH --out PATH` validates a
  journal without executing or resuming work and publishes a complete or
  incomplete canonical audit.
- `opaal-language-server` provides stdio diagnostics, completion, hover,
  signature help, definitions, references, and whole-document formatting.

Effectful action declarations are statically analyzed everywhere. Ordinary
script and interactive evaluation still refuse them before host access; only
the explicit accepted-plan route can supply the controlled operational host.
Pure actions retain the existing evaluator route. Process-bearing accepted
execution is currently Linux-only: macOS can inspect and check such tasks,
produce a refused non-executable plan, and audit journals, but never falls back
to pathname process execution.

## Workspace

| Path | Responsibility |
| --- | --- |
| `crates/opaal-syntax/` | Source, lexer, parser, syntax trees, formatting, and diagnostics |
| `crates/opaal-runtime/` | Values, module analysis, maintained operational APIs, outcomes, streams, and bounded evaluation |
| `crates/opaal-platform/` | Platform capability and bounded operational adapter contracts plus fakes |
| `crates/opaal-platform-posix/` | macOS/Linux shell and maintained operational adapters plus observation fixtures |
| `crates/opaal-cli/` | Command-line, project workflow, checker, formatter, and interactive frontends |
| `crates/opaal-lsp/` | Non-executing Language Server Protocol adapter |
| `fuzz/` | Separate unpublished package with five fuzz targets |

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
[pre-1.0 language migration](docs/migration.md),
[actions and explicit projects](docs/actions-and-projects.md),
[bounded operational modules](docs/operational-modules.md),
[architecture](docs/architecture.md), [development guide](docs/development.md),
[contribution guide](CONTRIBUTING.md), [security policy](SECURITY.md), and
[changelog](CHANGELOG.md).

## License

Unless a file states otherwise, OPAAL is licensed under the
[Mozilla Public License 2.0](LICENSE).
