# Flash

[FlashOS](../../README.md) › Flash

Flash (`fsh`) is the FlashOS shell and a language for structured system
automation. Code tried at the prompt follows the same non-POSIX syntax and
runtime rules as code in a `.fsh` file. Start with [Flash by
Example](docs/by-example.md), or use the [documentation index](docs/README.md)
for the language reference and implementation guides.

> **Project status:** FlashOS as a complete operating system remains pre-alpha
> software. Flash 1.0.0 is released as the component contract in the current
> source. FlashOS versions and images that carry it are qualified separately;
> execution on a Linux or macOS host is not proof of FlashOS target support.

## On this page

- [Role in FlashOS](#role-in-flashos)
- [Design boundaries](#design-boundaries)
- [Flash v1 contract](#flash-v1-contract)
- [Current implementation](#current-implementation)
- [Using `fsh`](#using-fsh)
- [Using the language server](#using-the-language-server)
- [Development and verification](#development-and-verification)
- [Documentation](#documentation)
- [License](#license)

## Role in FlashOS

The FlashOS x86_64 image configuration includes the `flash` package and assigns `/usr/bin/fsh` as the login shell for the configured root and user accounts.

Running `fsh` without a script starts an interactive session. Passing a script path evaluates the UTF-8 source file as a Flash program; Flash scripts conventionally use the `.fsh` extension.

Flash provides the language and execution environment, but it does not replace the rest of the userspace. External commands remain separate executables supplied by the system image and are launched through the platform integration layer.

FlashOS also uses Flash for project automation wherever `fsh` is already
available. Bootstrap, recovery, third-party tool integration, and independent
validation still need a small number of non-Flash scripts.

Later, Flash and higher-level terminal views are intended to share the same
system actions. Those views and the required stable system API do not exist
yet. [Flash and FlashOS](../../docs/flash.md) describes both the current setup
and that longer-term idea.

## Design boundaries

Flash intentionally does not claim compatibility with POSIX shells such as `sh` or Bash. POSIX shell scripts should not be expected to run as Flash programs, and Flash syntax should not be passed to another shell interpreter.

Several rules shape the implementation:

- Interactive input and script execution use the same syntax and runtime crates, but their front ends are not required to expose identical editing, history, or startup behavior on every target.
- Structured values belong to the Flash runtime. At an external process boundary, commands still use argument vectors, environment variables, working directories, file descriptors, and byte-oriented standard streams.
- External executables are launched through the platform interface. Flash source is never translated into another shell language.
- The `fsh` process preserves representable completed codes and signals while
  keeping program output on stdout, shell diagnostics on stderr, launcher
  misuse distinct from runtime failure, and required report writes checked.
- A successful macOS or Linux run does not prove the same behavior in a FlashOS image. Target compilation and image execution are tested separately.

## Flash v1 contract

Flash 1.0 sets the compatibility baseline for the language, runtime, and tools: values,
commands, pipelines, statuses, jobs, modules, script arguments, typed function
metadata, help, formatting, static checking, language-server behavior, and
platform capabilities. The 30-command core has no current
aliases or reserved names; direct external execution remains available
through `^name` and `command name`.

Pipelines can alternate between external byte stages and internal typed
segments while keeping conversions visible and streaming data in limited
buffers. Post-v1 work may add compatible capabilities,
diagnostics, tooling, and optimizations. An incompatible language redesign
requires a future major-version decision.

A FlashOS release still tests target availability separately. The
[Language Guide](docs/language-guide.md) and [Scripting
Guide](docs/scripting.md) define the public behavior. The [Architecture
Guide](docs/architecture.md) and [Development Guide](docs/development.md)
describe the implementation and maintenance work.

## Current implementation

Flash is an independent Rust workspace inside the FlashOS repository. [`Cargo.toml`](Cargo.toml) lists the current members; this table is a quick map of the main responsibilities.

| Path                           | Responsibility                                                                          |
| ------------------------------ | --------------------------------------------------------------------------------------- |
| `crates/flash-syntax/`         | Source representation, lexical analysis, parsing, syntax trees, and diagnostics         |
| `crates/flash-runtime/`        | Runtime values, shared analysis, built-ins, execution planning, sessions, and jobs      |
| `crates/flash-platform/`       | Platform capability contracts used by the runtime                                       |
| `crates/flash-platform-posix/` | Process, filesystem, descriptor, signal, and terminal integration for supported targets |
| `crates/flash-cli/`            | The `fsh` executable and its interactive and script entry points                        |
| `crates/flash-lsp/`            | The non-executing `flash-language-server` protocol adapter                              |

This split keeps language semantics and non-executing editor services
independent of target-specific process and terminal handling. Flash is
implemented in Rust; unsafe code is prohibited in the CLI and locally justified
where the low-level platform adapter requires it.

## Using `fsh`

On an installed FlashOS image, the executable is available as `/usr/bin/fsh`.

```sh
# Start an interactive session.
fsh

# Run a Flash script.
fsh program.fsh

# Check one file and everything it imports, without execution.
fsh check program.fsh

# Inspect one command pipeline without executing it.
fsh plan command.fsh

# Check formatting or rewrite the files atomically.
fsh format --check program.fsh
fsh format --write program.fsh

# Show the supported command-line options.
fsh --help
```

Language syntax and runtime behavior are documented in the [Language Guide](docs/language-guide.md). Guidance for organizing and executing `.fsh` files belongs in the [Scripting Guide](docs/scripting.md).

## Using the language server

The installed Flash package also provides `/usr/bin/flash-language-server`.
Configure an editor's Language Server Protocol client to start it as a stdio
process for `.fsh` files:

```text
command: ["flash-language-server"]
transport: stdio
```

The stdio server provides diagnostics, completion, hover, signature help,
definition, references, and whole-document formatting without executing open
source. It does not provide TCP transport, workspace configuration, or
incremental edits. See the [Architecture
Guide](docs/architecture.md#language-server-protocol-adapter) for the exact
protocol details and [Development](docs/development.md#language-server-contract) for
editor setup and verification.

## Development and verification

Run the complete host checks from the repository root:

```sh
make flash-bootstrap
make flash-automation-tools
build/flash-bootstrap/134635a5e1282b5d8455a4b2aeb754be5a3a77c1/fsh ci/check_flash_conformance.fsh
build/flash-bootstrap/134635a5e1282b5d8455a4b2aeb754be5a3a77c1/fsh ci/check_flash_release.fsh
cargo test --manifest-path components/flash/Cargo.toml --workspace --locked
cargo clippy --manifest-path components/flash/Cargo.toml --workspace --all-targets -- -D warnings
```

Target compilation is a separate check:

```sh
redoxer build -p flash-cli --bin fsh
redoxer build -p flash-lsp --bin flash-language-server
```

The [Flash v1 exercises](exercises/README.md) cover the released user-facing
features. The [1.0.0 release record](release/v1.toml) ties those exercises and
other required checks to the component release. Host checks, target builds,
image integration, and QEMU execution answer different questions. See [Flash
Development](docs/development.md) for the component workflow and [FlashOS
Verification](../../docs/verification.md) for the evidence model.

## Documentation

Use the [Flash documentation index](docs/README.md) for the complete guide and
technical-reference inventory. New readers should begin with [Flash by
Example](docs/by-example.md); system builders should begin with [FlashOS
Getting Started](../../docs/getting-started.md). Component history lives in the
[Flash changelog](CHANGELOG.md).

## License

The Flash workspace is licensed under the [MIT License](LICENSE).

Other FlashOS components and incorporated third-party materials may be subject to separate terms. See the repository-level [NOTICE](../../NOTICE) and the applicable license files for attribution and licensing details.

---

[← Previous: Upstream References](../../docs/upstream/README.md) · [Product Guide](../../docs/README.md) · [Next: Flash Documentation →](docs/README.md)
