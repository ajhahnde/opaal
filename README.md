<div align="center">

<h1>FlashShell</h1>

<h3>A command shell and scripting language, written in Rust</h3>

<p>
  <img src="https://img.shields.io/badge/version-v0.1.0-f59e0b?style=flat-square" alt="Version">
  <img src="https://img.shields.io/badge/license-Apache--2.0-lightgrey?style=flat-square" alt="License">
</p>

<p>
  <b>README</b> ·
  <a href="CHANGELOG.md"><b>Changelog</b></a> ·
  <a href="LICENSE"><b>License</b></a>
</p>

</div>

---

## About

FlashShell is a command shell and scripting language designed from first
principles to make command-line workflows ergonomic, safe, and composable.

The executable is `fsh`, and scripts use the `.fsh` extension. Scripts and the
interactive prompt share the same parser and evaluator, ensuring identical
behavior in both contexts.

FlashShell composes conventional Unix-style programs that communicate through
byte streams. Alongside them, it provides built-in commands that exchange
streams of typed values. Every conversion between structured values and
external byte streams is explicit.

FlashShell deliberately does not aim for POSIX compatibility. Its semantics are
based on typed values, explicit conversions, and predictable expansion rules,
as described in the [Design](#design) section.

The engine is implemented in safe Rust and interacts with the underlying platform
through a narrow, well-defined interface.

## Design

```fsh
let name = "FlashShell"             # immutable binding
mut count = 0                       # mutable binding

echo "hello $name"                  # expansion, never splitting
let args = ["status", "--short"]
^git ...$args                       # ^ forces an external command; ... spreads a list

open users.json
    | from json
    | where {|user| user.active}
    | select name email
    | sort name

^build && echo success || echo failed
```

- **No implicit word splitting**. `$name` expands to exactly one argument.
  A list expands to multiple arguments only through an explicit `...$list`
  spread.
- **No strings as code**. FlashShell has no `eval`. Command substitution
  captures output as a value and never reparses it as source code.
- Direct execution. External commands are launched directly with an
  argument vector. FlashShell never routes command strings through `/bin/sh`.
- **Typed pipelines**. External-to-external pipelines remain ordinary byte
  streams, while built-in commands exchange streams of typed values. Ambiguous
  boundaries are rejected with a suggested explicit converter, such as
  `from json` or `to json`.
- **Statuses, not exceptions**. A nonzero exit code produces a normal Status
  value, and `&&` / `||` branch on that value. The check command explicitly
  converts an unsuccessful status into a catchable error. Process-spawning,
  redirection, and type failures are runtime errors with precise source spans.
- **Explicit globbing**. `glob "src/**/*.rs"` is an expression. A bare `*.rs`
  pattern is never expanded implicitly.
- **Precise diagnostics**. Lossless lexing and byte-accurate source spans power
  every diagnostic, the canonical formatter, and eventually syntax
  highlighting and completion. There are no competing tokenizers.

## Building

Requires the Rust toolchain pinned in `rust-toolchain.toml` (rustup picks it
up automatically).

```sh
cargo build                     # build fsh into target/debug
cargo run -p flashshell-cli -- --version

cargo test --workspace          # unit, integration, golden, and property tests
cargo fmt --check               # formatting gate
cargo clippy --workspace --all-targets --all-features -- -D warnings

./fuzz/run-smoke.sh             # bounded lexer/parser fuzz run (needs nightly + cargo-fuzz)
```

## Layout

```text
crates/flashshell-syntax/           spans, lexer, parser, AST, diagnostics, formatter
crates/flashshell-runtime/          values, scopes, evaluation (in progress)
crates/flashshell-platform/         platform trait and portable process contracts
crates/flashshell-platform-posix/   macOS/Linux process, fd, signal, terminal adapter
crates/flashshell-cli/              the fsh binary
tests/golden/                       golden .fsh sources with expected ASTs and diagnostics
tests/fixtures/                     helper executables for execution tests
tests/e2e/                          black-box and PTY tests
fuzz/                               lexer/parser fuzz targets (separate workspace)
```

Dependency direction is strict: `syntax ← runtime ← cli`, with the platform
adapters behind the platform contract.

---
