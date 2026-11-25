# Developing OPAAL

## Build and try the CLI

Run commands from the repository root. Install Rust through rustup, which
uses the version pinned in [rust-toolchain.toml](rust-toolchain.toml). A host C
toolchain is needed to build and link the programs. The validators use Python
3.11 or newer and its standard library.

```sh
cargo build --workspace --locked
target/debug/opaal --version
target/debug/opaal format --check examples/language-foundation.opaal
target/debug/opaal check examples/language-foundation.opaal
target/debug/opaal examples/language-foundation.opaal
```

This builds `target/debug/opaal` and `target/debug/opaal-language-server`. The
example finishes silently on success. The [source examples](examples/README.md)
show more features and project workflows.

## Choose focused tests

| Area | Command |
| --- | --- |
| Grammar, lexer and formatter | `cargo test --locked -p opaal-syntax` |
| Types, modules, evaluation and resource limits | `cargo test --locked -p opaal-runtime` |
| Host capabilities and POSIX behavior | `cargo test --locked -p opaal-platform -p opaal-platform-posix` |
| CLI, project commands and interactive behavior | `cargo test --locked -p opaal-cli` |
| LSP framing, editor queries and cancellation | `cargo test --locked -p opaal-lsp` |
| Python validators and packaging | `python3 -m unittest discover -s ci/tests -p 'test_*.py'` |

For changes that cross crates, run the workspace checks.

```sh
cargo fmt --all -- --check
cargo test --workspace --locked --no-fail-fast
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

## Check documentation and fixtures

```sh
python3 ci/check_docs.py --cli target/debug/opaal
python3 ci/check_source_formatting.py --cli target/debug/opaal
git diff --check
```

The [source inventory](tests/golden/source-formatting/inventory.json) tracks
canonical examples and deliberate failure cases. Give `opaal format --check`
an explicit list of sources. Check their classified roles before rewriting
fixtures. Update the example and its checking path together when behavior
changes.

## CI and platform qualification

Maintainer PRs pass the stable `required` and `security-required` checks.
CI runs workspace, repository policy and fuzz checks. Linux x86_64, Linux
ARM64 and macOS ARM64 each run operational-core and archive qualification.
On a PR, the operational-core jobs test the merge commit and the archive
jobs replay the exact PR-head CLI/LSP archives. The security workflow checks
dependencies and repository policy.

See [CI and local qualification](ci/README.md) for policy checks, archive
replays, benchmarks and native tests. See [Releasing](RELEASING.md) for release
archive verification and publication.

## Work on the Rust APIs

The [Rust API notes](crates/README.md) cover syntax visitors, runtime types and
host contracts. Build local API documentation with
`cargo doc --workspace --no-deps --locked`. See [Contributing](CONTRIBUTING.md)
for the maintainer workflow.

[Repository overview](README.md) · [Product documentation](https://opaal-lang.org/docs/)
