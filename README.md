# OPAAL

OPAAL is the Operational Programming & Automation Language. This repository
contains its standalone syntax, runtime, command-line client, platform
contracts, POSIX adapter, and Language Server Protocol implementation.

The [OPAAL website](https://opaal-lang.org/) is the home of the
[product documentation](https://opaal-lang.org/docs/),
[learning guides](https://opaal-lang.org/learn/), and
[downloads](https://opaal-lang.org/download/). This repository contains the
source code, contributor information, and maintainer guides.

> OPAAL's explicit project surface can inspect, check, render, explicitly accept,
> execute, journal, and audit one typed task under bound authority. OPAAL 1.1.0
> adds direct foreground `^program` execution in standalone and interactive
> clients. A non-publishing qualification workflow checks release eligibility.

The [changelog](CHANGELOG.md) records changes by release. Follow the
[download and installation guide](https://opaal-lang.org/download/) for Linux
x86_64 and macOS arm64 archives and checksum instructions.

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

The pure example finishes without printing its final value; a foreground
external program can write to stdout and stderr. The embedding API retains the
final value. Running `opaal` without a script in a terminal starts the
interactive client, which presents completed values. The interactive client is
a language evaluator; project-management commands are not part of its surface.

OPAAL uses one value language throughout: bare names read bindings, the final
expression is a block's value, `{expression}` interpolates into a command word,
`...{expression}` spreads a list, and only a literal head prefixed with `^`
denotes an external program. In 1.1 standalone and interactive sessions, that
head can launch a foreground child. An unknown bare name never launches a host
process.

## Current surfaces

- `opaal [SCRIPT [ARG...]]` runs one explicit `.opaal` root.
- `opaal check SOURCE` analyzes a source graph without executing it.
- `opaal check --project opaal.toml ...` validates one explicit task,
  environment-derived authority/tool lock, explicit lexical or file-snapshot
  inputs, and zero-or-one executable secret requirement without executing an
  action or adapter; `--format json` emits its canonical check artifact.
- `opaal task inspect --project opaal.toml TASK` reports the task's shared
  action signature, declared effects, tools, and environments.
- `opaal format --check|--write PATH...` checks or atomically rewrites source.
- `opaal plan SOURCE` returns the structured `PLAN004` unsupported refusal;
  `opaal plan --project opaal.toml ... --out PATH` writes one canonical,
  identity-bound, expiring plan without executing the task or probing tools;
  `opaal plan inspect PATH` validates and renders its bounded human view.
- `opaal execute --plan PATH --accept DIGEST ... --journal PATH` revalidates
  and, on a supported execution host, runs exactly one accepted project plan
  under its plan-bound authority and writes a synced hash-chained journal; the
  run ID may be supplied or securely generated.
- `opaal audit --project opaal.toml --journal PATH --out PATH` validates a
  journal without executing or resuming work and publishes a complete or
  incomplete canonical audit; `opaal audit inspect PATH` validates and renders
  the recorded prefix identity and redacted operation evidence.
- `opaal-language-server` provides stdio diagnostics, completion, hover,
  signature help, definitions, references, and whole-document formatting. An
  editor may select exactly one project with the absolute manifest `file:` URI
  in `initializationOptions.opaal.projectManifest`; omission is standalone
  mode. See the [editor-services reference](https://opaal-lang.org/docs/developer-services/).

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
python3 ci/qualify_operational_core.py --profile qualification
python3 benchmarks/run.py --profile smoke
```

The release workflow builds both programs on the supported Linux and macOS
hosts and stores checked archives and checksums as workflow artifacts. The
operator attaches those exact files to the GitHub release after both jobs pass.
No workflow publishes crates or creates a GitHub release.

Host success establishes only the exercised macOS/Linux surfaces. It does not
claim packaging by another operating system, Redox support, or physical
hardware qualification.

The website provides the [language reference](https://opaal-lang.org/docs/language/),
[project and authority reference](https://opaal-lang.org/docs/projects/),
[compatibility and platform limits](https://opaal-lang.org/docs/compatibility/),
and [getting started path](https://opaal-lang.org/learn/getting-started/).
Repository guides cover [development](DEVELOPMENT.md),
[release qualification](RELEASING.md), [contributions](CONTRIBUTING.md),
[security](SECURITY.md), and the [changelog](CHANGELOG.md).

## License

Unless a file states otherwise, OPAAL is licensed under the
[Mozilla Public License 2.0](LICENSE).
