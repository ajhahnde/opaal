# Flash

[FlashOS](../../README.md) › Flash

Flash (`fsh`) is the primary interactive shell and scripting interface of FlashOS. It is a non-POSIX command language built around structured runtime values, explicit process invocation, and a shared syntax and execution core for interactive input and `.fsh` scripts. This page provides a component overview; detailed language, scripting, architecture, and development documentation is available under [`docs/`](docs/README.md).

> **Project status:** FlashOS as a complete operating system remains pre-alpha software. However, Flash component documentation defines the intended stable Flash v1.0 contract. Note that not every v1 feature is automatically available in every current FlashOS image or on every target platform, and execution on a Linux or macOS development host is not automatic proof of FlashOS target support.

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

## Design boundaries

Flash intentionally does not claim compatibility with POSIX shells such as `sh` or Bash. POSIX shell scripts should not be expected to run as Flash programs, and Flash syntax should not be passed to another shell interpreter.

The component follows several implementation boundaries:

- Interactive input and script execution use the same syntax and runtime crates, but their front ends are not required to expose identical editing, history, or startup behavior on every target.
- Structured values belong to the Flash runtime. At an external process boundary, commands still use argument vectors, environment variables, working directories, file descriptors, and byte-oriented standard streams.
- External executables are launched directly through the platform interface rather than by translating Flash source into another shell language.
- The `fsh` process preserves representable completed codes and signals while
  keeping program output on stdout, shell diagnostics on stderr, launcher
  misuse distinct from runtime failure, and required report writes checked.
- Successful behavior on a macOS or Linux development host does not by itself establish equivalent behavior in a FlashOS image. Target compilation and image-level execution require separate verification.

## Flash v1 contract

The public Flash documentation defines the intended v1 language, runtime, and tooling contract. That contract covers the existing value, command, pipeline, status, and job model together with maintainable multi-file scripts, explicit module boundaries and initializer effects, a stable built-in namespace compatibility policy, script arguments, typed function metadata, discoverable help, canonical formatting, non-executing static checks, language-server integration, and explicit platform capabilities.

Flash v1 is the language-completion baseline, not a checkpoint that knowingly leaves foundational semantic or executor-topology restrictions for a later release. Post-v1 development may add compatible capabilities, diagnostics, tooling, and optimizations, while incompatible language redesign belongs to an explicit future major-version decision.

A particular FlashOS release may expose only the parts of that contract that are implemented and qualified for its target environment. Unsupported or unqualified capabilities must remain visible rather than being silently replaced with weaker host-specific behavior.

The Flash v1 built-in namespace is now ratified as an exact 30-command core
with no current aliases or reserved names. Its validated manifest drives
resolution, planning, static diagnostics, help, completion, `which`, background
classification, and canonical execution identity. Capturing or releasing an
ordinary external name requires a language-major decision; explicit external
execution remains available through `^name` and `command name`.

The detailed responsibilities are divided between the [Language Guide](docs/language-guide.md), [Scripting Guide](docs/scripting.md), [Architecture Guide](docs/architecture.md), and [Development Guide](docs/development.md).

## Current implementation

Flash is maintained as an independent Rust workspace inside the FlashOS repository. The workspace manifest at [`Cargo.toml`](Cargo.toml) is authoritative for current membership; the table below describes the principal implementation responsibilities rather than a permanent crate count.

| Path                           | Responsibility                                                                          |
| ------------------------------ | --------------------------------------------------------------------------------------- |
| `crates/flash-syntax/`         | Source representation, lexical analysis, parsing, syntax trees, and diagnostics         |
| `crates/flash-runtime/`        | Runtime values, shared analysis, built-ins, execution planning, sessions, and jobs      |
| `crates/flash-platform/`       | Platform capability contracts used by the runtime                                       |
| `crates/flash-platform-posix/` | Process, filesystem, descriptor, signal, and terminal integration for supported targets |
| `crates/flash-cli/`            | The `fsh` executable and its interactive and script entry points                        |
| `crates/flash-lsp/`            | The non-executing `flash-language-server` protocol adapter                              |

The separation between syntax, runtime, protocol, platform contracts,
operating-system integration, and the command-line interface keeps language
semantics independent from target-specific terminal and process handling. The
language server depends only on shared syntax and analysis services; it does not
depend on the CLI or a platform adapter.

Flash is implemented in Rust. The CLI prohibits unsafe code, while the low-level platform adapter permits explicitly scoped unsafe sections for operations such as process-group, signal, and file-descriptor setup.

## Using `fsh`

On an installed FlashOS image, the executable is available as `/usr/bin/fsh`.

```sh
# Start an interactive session.
fsh

# Run a Flash script.
fsh program.fsh

# Check one root and its canonical import closure without execution.
fsh check program.fsh

# Check or atomically rewrite canonical source formatting.
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

The executable accepts protocol input on stdin and reserves stdout for framed
JSON-RPC messages. It uses full-document synchronization for absolute `file:`
URIs and provides diagnostics, completion, hover, signature help, definition,
references, and whole-document formatting without executing the open source.
It does not provide a source-taking command-line mode, TCP transport, workspace
configuration, or incremental edits. See the [Architecture
Guide](docs/architecture.md#language-server-protocol-adapter) for the exact
protocol surface and [Development](docs/development.md#language-server-contract)
for editor setup and verification.

## Development and verification

Run workspace checks from the Flash component directory:

```sh
cd components/flash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Target compilation is a separate check:

```sh
redoxer build -p flash-cli --bin fsh
```

These checks establish different properties:

- Host tests exercise portable syntax, runtime, and CLI behavior on the development system.
- A `redoxer` build verifies that the selected binary compiles for the Redox target environment.
- FlashOS image construction and QEMU execution verify package integration, installation, login-shell configuration, and behavior inside the assembled system.

For the component-specific workflow, test layout, and maintenance guidance, see [Flash Development](docs/development.md). For the repository-wide distinction between host checks, target checks, image validation, and runtime evidence, see [FlashOS Verification](../../docs/verification.md).

## Documentation

The Flash documentation is organized as follows:

- [Flash documentation index](docs/README.md) — entry point for the component documentation
- [Language Guide](docs/language-guide.md) — language concepts, modules, name resolution, function metadata, commands, and typed pipelines
- [Scripting Guide](docs/scripting.md) — `.fsh` execution, script arguments, static checking, formatting, external processes, statuses, and jobs
- [Architecture](docs/architecture.md) — internal responsibilities, analysis services, platform capabilities, adapters, and lifecycle boundaries
- [Development](docs/development.md) — workspace setup, tests, formatter,
  checker, and language-server integration and gates, target builds, and
  maintenance workflow

For building and booting FlashOS as a complete system, begin with the [FlashOS Getting Started Guide](../../docs/getting-started.md).

## License

The Flash workspace is licensed under the [MIT License](LICENSE).

Other FlashOS components and incorporated third-party materials may be subject to separate terms. See the repository-level [NOTICE](../../NOTICE) and the applicable license files for attribution and licensing details.

---

[← Previous: Upstream References](../../docs/upstream/README.md) · [FlashOS README](../../README.md) · [Next: Flash Documentation →](docs/README.md)
