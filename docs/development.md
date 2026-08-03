# FlashShell Development

[FlashOS](../../../README.md) › [FlashShell](../README.md) › [Documentation](README.md) › Development

This guide describes the component-specific workflow for building, testing, documenting, and integrating FlashShell. It is intended for developers changing the language implementation, runtime, platform adapters, interactive front end, or `fsh` executable; repository-wide image development and verification policy remain documented under the main FlashOS documentation.

> **Project status:** FlashOS as a complete operating system remains pre-alpha software. However, this FlashShell Development Guide supports the intended stable FlashShell v1.0 contract. Note that not every v1 feature is automatically available in every current FlashOS image or on every target platform, and successful test execution on a Linux or macOS development host is not automatic proof of FlashOS target support.

## On this page

- [Development scope](#development-scope)
- [Toolchains and prerequisites](#toolchains-and-prerequisites)
- [Workspace layout](#workspace-layout)
- [Local development loop](#local-development-loop)
- [Build and run `fsh`](#build-and-run-fsh)
- [Test layers](#test-layers)
- [Develop syntax and parsing](#develop-syntax-and-parsing)
- [Develop v1 tooling](#develop-v1-tooling)
- [Develop the runtime](#develop-the-runtime)
- [Develop platform integration](#develop-platform-integration)
- [Develop the CLI and interactive session](#develop-the-cli-and-interactive-session)
- [Test fixtures](#test-fixtures)
- [Fuzzing](#fuzzing)
- [Target compilation](#target-compilation)
- [FlashOS image integration](#flashos-image-integration)
- [Dependencies and supply-chain policy](#dependencies-and-supply-chain-policy)
- [Generate API documentation](#generate-api-documentation)
- [Keep documentation synchronized](#keep-documentation-synchronized)
- [Before considering a change complete](#before-considering-a-change-complete)

## Development scope

FlashShell is maintained as an independent Cargo workspace under:

```text
components/flashshell/
```

The workspace has its own:

- `Cargo.toml`;
- `Cargo.lock`;
- pinned Rust toolchain;
- formatting configuration;
- dependency policy;
- build output;
- tests;
- fuzz workspace;
- component documentation.

Run component-level Cargo commands from `components/flashshell/`. Running Cargo from the FlashOS repository root addresses the separate root build-system package and does not exercise the FlashShell workspace.

Development evidence is layered:

| Check                      | What it establishes                                                      |
| -------------------------- | ------------------------------------------------------------------------ |
| Formatting and Clippy      | Source-format and lint compliance for the host build                     |
| Host tests                 | Portable behavior and supported host-platform integration                |
| Fuzzing                    | Resilience of selected syntax entry points against generated inputs      |
| `redoxer` build            | Compilation of the `fsh` binary for the Redox target environment         |
| Package build              | Construction of the pinned FlashShell package through the FlashOS recipe |
| Image build                | Inclusion of that package in an assembled FlashOS image                  |
| QEMU or hardware execution | Runtime behavior in the produced system                                  |

Passing one layer does not imply that later layers pass. In particular, host tests do not prove Redox behavior, and target compilation does not prove login-shell or terminal behavior inside a FlashOS image.

Use [FlashOS Verification](../../../docs/verification.md) for the repository-wide evidence model.

## Toolchains and prerequisites

### Stable component toolchain

The main workspace pins its compiler and required components in [`rust-toolchain.toml`](../rust-toolchain.toml). When `rustup` is installed, entering the component directory and running Cargo causes the pinned toolchain to be selected automatically.

Verify the selected tools with:

```bash
cd components/flashshell

rustc --version
cargo --version
cargo fmt --version
cargo clippy --version
```

Do not silently substitute the FlashOS root toolchain. The root repository and FlashShell are separate Cargo workspaces and may intentionally pin different compiler versions.

The normal component workflow requires:

- Rust through `rustup`;
- Cargo;
- `rustfmt`;
- Clippy;
- a supported macOS or Linux host for host-specific process and pseudoterminal tests.

### Nightly fuzzing toolchain

The fuzz package is a separate Cargo workspace because libFuzzer instrumentation requires a nightly compiler.

Install the additional tools before running fuzz campaigns:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
```

The nightly toolchain is required for fuzzing only. It does not replace the stable toolchain pinned for normal builds, linting, tests, or documentation.

### Redox target tooling

Target compilation requires `redoxer` in addition to the host toolchain.

Verify its availability with:

```bash
redoxer --version
```

A developer working only on portable syntax or runtime code may begin with host checks, but a change that affects target-selected code, process behavior, terminal behavior, or the shipped executable requires the target build as an additional check.

## Workspace layout

The current workspace membership is defined by [`components/flashshell/Cargo.toml`](../Cargo.toml). Do not treat a fixed crate count as a permanent project contract.

The principal implementation responsibilities are:

| Concern | Current owner |
| --- | --- |
| Source files, spans, lexer, parser, syntax trees, canonical formatting, and diagnostics | `flashshell-syntax` |
| Values, scopes, evaluation, functions, command metadata, planning, pipelines, modules, sessions, and jobs | `flashshell-runtime` and shared analysis interfaces |
| Portable operating-system capability contracts and deterministic test adapters | `flashshell-platform` |
| Unix-like process, descriptor, filesystem, signal, and terminal operations | `flashshell-platform-posix` |
| CLI modes, interactive editing, configuration, history, tooling entry points, and executable assembly | `flashshell-cli` |

Supporting directories hold component documentation, fuzz targets, end-to-end tests, fixture executables, and golden corpora. Inspect the workspace manifest and the relevant directory before documenting an exact package or test inventory.

Read [FlashShell Architecture](architecture.md) before moving behavior across responsibility boundaries or introducing a dependency in the opposite direction.

## Local development loop

Use the narrowest relevant tests while iterating, then run the complete component gate before considering the change complete.

A normal loop is:

```bash
cd components/flashshell

cargo fmt --all
cargo test -p affected-package --locked
cargo clippy -p affected-package --all-targets -- -D warnings
```

Replace `affected-package` with the owning package, for example:

```bash
cargo test -p flashshell-syntax --locked
cargo test -p flashshell-runtime --locked
cargo test -p flashshell-cli --locked
```

Before completing the change, run the full host gate:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

The repository helper exposes the same complete host gate from the FlashOS root:

```bash
source ./flashos.sh
flashos shell all
```

The helper changes into the component workspace before running formatting, Clippy, and tests. Direct Cargo commands remain the clearest interface when debugging an individual crate or test target.

### Keep checks reproducible

Use `--locked` for test and build operations that are intended to verify the committed dependency graph:

```bash
cargo test --workspace --locked
```

Do not regenerate `Cargo.lock` as an incidental side effect of an unrelated change. A lockfile update should be deliberate and reviewed together with its manifest changes.

## Build and run `fsh`

Build the executable on the host:

```bash
cd components/flashshell

cargo build -p flashshell-cli --bin fsh --locked
```

The development binary is written below the component workspace:

```text
target/debug/fsh
```

Run an interactive host session through Cargo:

```bash
cargo run -p flashshell-cli --bin fsh
```

Run a script:

```bash
cargo run -p flashshell-cli --bin fsh -- path/to/program.fsh
```

Inspect the command-line interface:

```bash
cargo run -p flashshell-cli --bin fsh -- --help
cargo run -p flashshell-cli --bin fsh -- --version
```

Host execution is useful for language and frontend iteration. It does not establish that the same terminal facilities, executables, filesystem layout, or process capabilities are available inside FlashOS.

## Test layers

The workspace combines unit tests, crate integration tests, declarative golden corpora, deterministic child fixtures, black-box executable tests, and pseudoterminal tests.

| Area               | Representative coverage                                                                                                                                |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Syntax             | Source spans, lexical losslessness, input classification, AST construction, parser behavior, formatting, and property invariants                       |
| Runtime            | Values, scopes, functions, expansion, command resolution, planning, pipelines, structured commands, status propagation, cancellation, limits, and jobs |
| Platform contracts | Capability reporting, fake and recording adapters, native values, resources, and cleanup                                                               |
| Concrete adapter   | Process creation, descriptors, waits, signals, process groups, and terminal guards                                                                     |
| CLI                | Argument handling, startup configuration, editor services, interactive recovery, history, completion, and highlighting                                 |
| Black-box and PTY  | Executable behavior, prompts, control characters, terminal ownership, signals, and job control                                                         |
| Fuzzing            | Public lexer and parser entry points with arbitrary bytes                                                                                              |

Run a named integration test with Cargo's `--test` selector:

```bash
cargo test -p flashshell-syntax --test parser
cargo test -p flashshell-runtime --test pipeline
cargo test -p flashshell-cli --test e2e
```

Pass a test-name filter after the target when narrowing a failure:

```bash
cargo test -p flashshell-runtime --test pipeline pipeline_name_fragment
```

Use `--nocapture` when a test intentionally writes useful diagnostic output:

```bash
cargo test -p flashshell-cli --test pty -- --nocapture
```

Do not treat a single filtered test as the final gate. Tests in adjacent layers often enforce the same public contract from different boundaries.

## Develop syntax and parsing

Changes to tokens, grammar, precedence, input completeness, syntax trees, source formatting, or diagnostics belong primarily in `flashshell-syntax`.

The syntax tests include focused targets for:

- source and diagnostic behavior;
- the lossless lexer;
- lexical classification;
- parser productions;
- AST construction;
- formatter behavior;
- parser and formatter invariants;
- lexical and grammar golden corpora.

### Update the lexical corpus

The lexical corpus is under:

```text
tests/golden/lexical/
```

Its `manifest.tsv` is the inventory consumed by the tests. Each source belongs to one classification:

| Classification | Meaning                                                       |
| -------------- | ------------------------------------------------------------- |
| `complete`     | The lexer has enough source to finish                         |
| `incomplete`   | The source may become valid when more input is supplied       |
| `invalid`      | The lexer can reject the source without requesting more input |

When changing a lexical rule:

1. add or modify a focused `.fsh` fixture in the appropriate classification directory;
2. update `manifest.tsv`;
3. give the row a specific reason that describes the contract being tested;
4. run the lexical golden test;
5. run the complete syntax package tests.

Do not copy the fixture source into a second Rust test table. The manifest and fixture file are the declarative source of the golden case.

### Update the grammar corpus

The grammar corpus is under:

```text
tests/golden/grammar/
```

Its classifications are:

| Classification | Meaning                                                           |
| -------------- | ----------------------------------------------------------------- |
| `complete`     | The source must parse as a complete script                        |
| `incomplete`   | End of input must request additional source                       |
| `invalid`      | The construct is closed but does not match a supported production |

A grammar change should normally include:

- a positive complete case;
- an incomplete case when the construct can remain open;
- an invalid case for an important rejected form;
- focused parser or AST assertions when tree shape matters;
- formatter coverage when canonical output changes.

Run the focused corpus tests with:

```bash
cargo test -p flashshell-syntax --test lexical_golden
cargo test -p flashshell-syntax --test grammar_golden
```

Then run all syntax tests:

```bash
cargo test -p flashshell-syntax --locked
```

### Preserve diagnostics and formatting

A parser change is not complete merely because valid source parses.

Also verify:

- byte spans still identify the intended source;
- incomplete input is not misreported as invalid;
- invalid input does not become an interactive continuation;
- formatter output parses again;
- formatting is stable when reapplied;
- diagnostics remain specific enough to identify the failed construct.

Changes to public syntax must also update the [Language Guide](language-guide.md) and, where execution behavior changes, the [Scripting Guide](scripting.md).

## Develop v1 tooling

The v1 formatter, static checker, help system, and language server are different frontends over shared language and analysis services. They must not duplicate the grammar, syntax tree, module resolver, function metadata, pipeline validation, or diagnostic model.

### Formatter contract

Formatter changes must preserve all of the following:

- parsing succeeds before formatting;
- the formatted result parses to the same supported program structure;
- formatting is idempotent;
- check mode detects noncanonical source without rewriting it;
- write mode produces canonical source;
- comments and documentation metadata remain attached to the intended constructs;
- golden fixtures cover representative valid, incomplete, and invalid boundaries where formatting interacts with parsing.

Formatter commands and options must remain synchronized with the implemented CLI contract and its tests.

### Static checker contract

`fsh check` performs non-executing analysis. Checker development must cover:

- parsing and incomplete-input handling;
- canonical module resolution;
- import-cycle diagnostics;
- local, imported, exported, private, and missing names;
- function and command signatures;
- pipeline carrier compatibility;
- stable source spans and deterministic diagnostic ordering;
- success and failure behavior suitable for CI.

Checker tests must prove that analysis does not start external processes, apply redirections, mutate the caller's environment, change the working directory, or require interactive terminal state.

### Help and documentation metadata

Built-in and user-function help must use the same names, signatures, and documentation metadata consumed by static analysis and editor tooling. Tests should prevent help text, checker signatures, and language-server information from drifting into separate incompatible definitions.

Help lookup is inspection-only and must not execute the documented callable.

### Language-server contract

The FlashShell language server is required for the v1 tooling surface. It must reuse the shared parser, syntax tree, module graph, name resolution, signatures, and diagnostics rather than implementing another version of the language.

Language-server development should include:

- document open, change, and close behavior;
- deterministic diagnostics;
- cross-file module and name resolution;
- signature and help information;
- completion and navigation based on shared analysis;
- protocol request and response tests;
- cancellation and stale-document handling;
- tests proving that requests do not execute user programs.

Existing editor-local highlighting or completion does not replace the language-server contract.

## Develop the runtime

Runtime changes belong in `flashshell-runtime` when they affect:

- values or conversions;
- scopes and bindings;
- expressions or control flow;
- functions and closures;
- command expansion;
- internal command registration;
- external-command resolution;
- execution planning or preflight;
- pipeline carrier compatibility;
- structured streams;
- statuses and errors;
- sessions or background jobs.

The runtime tests are organized by behavior rather than by one monolithic integration target. Use the test file nearest to the changed contract, then run the full runtime package.

Examples:

```bash
cargo test -p flashshell-runtime --test values
cargo test -p flashshell-runtime --test expansion
cargo test -p flashshell-runtime --test plan
cargo test -p flashshell-runtime --test preflight
cargo test -p flashshell-runtime --test structured
cargo test -p flashshell-runtime --test jobs
```

### Prefer platform test doubles

Portable runtime tests should use the fake or recording implementations from `flashshell-platform` when the contract can be expressed through platform requests and responses.

This makes tests:

- deterministic;
- independent from the developer's process table;
- independent from installed host utilities;
- able to exercise unsupported capabilities and injected failures;
- able to inspect spawn, signal, descriptor, and terminal requests directly.

Use the concrete adapter only when the behavior under test is the operating-system integration itself.

### Test success and failure paths

For execution changes, cover more than the successful result. Relevant cases may include:

- expansion failure before execution;
- unsupported or unavailable capability;
- executable-resolution failure;
- redirection setup failure;
- partial pipeline startup;
- nonzero exit;
- signal termination;
- cancellation;
- output or collection limit;
- cleanup after a primary failure;
- stopped and continued process observations;
- status aggregation with and without `pipefail`.

Do not replace a runtime error with a synthetic command status merely to simplify a test. Statuses, evaluation errors, cancellation, and platform failures are separate public outcomes.

### Keep planning side-effect free

Changes to command planning or preflight should preserve the boundary between inspection and execution.

Tests should be able to construct and validate a plan without:

- starting a child process;
- creating a target file;
- changing a descriptor;
- transferring terminal ownership;
- modifying the host working directory.

Side effects belong to the executor after successful planning and preflight.

## Develop platform integration

Changes to capability contracts, owned process handles, descriptors, clocks, directory streams, or test adapters belong in `flashshell-platform`.

Changes that call concrete operating-system interfaces belong in `flashshell-platform-posix`.

Run the platform-contract tests with:

```bash
cargo test -p flashshell-platform --locked
```

Run the concrete-adapter tests with:

```bash
cargo test -p flashshell-platform-posix --locked
```

The concrete adapter tests exercise behavior such as:

- native argument and environment preservation;
- working-directory selection;
- pipe and descriptor ownership;
- process completion;
- signal handling;
- process groups;
- terminal-mode restoration;
- foreground-terminal restoration.

### Keep unsafe code contained

Low-level operating-system calls may require explicitly scoped unsafe code in the concrete adapter. Do not move such code into the syntax, runtime, abstract platform, or CLI crates to avoid an adapter boundary.

For every new unsafe block:

- keep the block as small as practical;
- document the safety conditions;
- establish ownership of every raw descriptor or process resource;
- define cleanup behavior for all early returns;
- test both normal completion and failure;
- preserve the crate's lint policy.

Rust's type system reduces classes of memory and ownership defects, but it does not replace validation of process, descriptor, signal, and terminal semantics.

### Preserve capability reporting

Do not silently approximate an unavailable process-group or terminal operation with weaker behavior.

A platform change should retain the distinction between:

- an unsupported capability;
- a capability that exists but is unavailable in the current session;
- a failed attempt to perform an available operation.

This distinction is required for useful diagnostics and target-specific degradation.

## Develop the CLI and interactive session

Changes to command-line parsing, startup modes, configuration, prompts, completion, highlighting, history, line editing, interactive recovery, or top-level session control belong in `flashshell-cli`.

The CLI tests include focused coverage for:

- startup configuration and policy;
- dependency direction;
- editor events;
- context-aware completion;
- syntax highlighting;
- history storage and suggestions;
- interactive session recovery;
- the FlashOS terminal editor;
- black-box executable behavior;
- pseudoterminal interaction.

Run focused tests with commands such as:

```bash
cargo test -p flashshell-cli --test config_startup
cargo test -p flashshell-cli --test context_aware_completion
cargo test -p flashshell-cli --test interactive_session
cargo test -p flashshell-cli --test terminal_editor
cargo test -p flashshell-cli --test e2e
cargo test -p flashshell-cli --test pty
```

Some interactive and PTY tests are host-specific. Cargo's target configuration controls whether host-only dependencies and tests are compiled.

### Keep the editor behind its interface

Interactive control flow should depend on the CLI's line-editor boundary rather than on details of one editing library.

Parser-driven services should continue to use the public syntax APIs for:

- complete, incomplete, and invalid input classification;
- highlighting spans;
- completion context;
- source diagnostics.

Do not introduce a second grammar in terminal-editor code.

### Test terminal cleanup

A terminal test should verify the state after the operation, not only the output produced while it ran.

Relevant postconditions include:

- canonical or raw mode is restored;
- the shell regains foreground terminal ownership;
- signal arrangements are restored;
- no child remains unreaped;
- prompt output begins only after pending notices are handled;
- cancellation returns to a usable session.

PTY tests should use bounded waits and explicit cleanup so that a failing child cannot leave the test suite blocked indefinitely.

## Test fixtures

Shared child programs live under:

```text
tests/fixtures/
```

They are small Rust executables used by runtime, adapter, CLI, and end-to-end tests. They avoid dependence on a host shell or on the output conventions of unrelated system utilities.

The shared fixture responsibilities include:

| Fixture               | Purpose                                                                                            |
| --------------------- | -------------------------------------------------------------------------------------------------- |
| `process_observer.rs` | Record native arguments, selected environment values, working directory, and descriptor visibility |
| `status.rs`           | Produce a requested exit code or terminate through a real signal                                   |
| `stream.rs`           | Generate, relay, consume, merge, or close deterministic byte streams                               |
| `terminal_editor.rs`  | Exercise terminal-editor input and output behavior                                                 |

Individual crates also contain narrower fixtures for behaviors such as signal-guard restoration or hang-up observation.

When adding a fixture:

- keep its interface minimal and deterministic;
- use machine-readable output where exact values matter;
- avoid invoking another shell;
- avoid relying on external utilities;
- avoid sleeps as the primary synchronization mechanism;
- register the binary only in the crates whose tests need it;
- document a shared fixture in [`tests/fixtures/README.md`](../tests/fixtures/README.md).

A fixture is test infrastructure, not part of the installed FlashShell package.

## Fuzzing

The separate [`fuzz/`](../fuzz/) workspace contains libFuzzer targets for the public lexer and parser entry points.

Both targets accept arbitrary bytes:

- valid UTF-8 is passed through the syntax APIs;
- invalid UTF-8 exercises source loading and normal rejection;
- golden `.fsh` files seed the mutation corpus.

From the component workspace, run the bounded smoke campaign:

```bash
cd components/flashshell
./fuzz/run-smoke.sh
```

The default campaign runs a bounded number of executions for each target. Supply another nonnegative count when needed:

```bash
./fuzz/run-smoke.sh 10000
```

The runner:

- invokes the `lexer` and `parser` targets;
- uses the grammar and lexical golden files as read-only seeds;
- creates writable corpora in a temporary directory;
- limits generated input length;
- removes the temporary corpus when the run ends.

The smoke runner is a fast regression check. It is not equivalent to a long-running fuzz campaign.

When a campaign finds a failure:

1. preserve the generated input;
2. reproduce it against the affected target;
3. minimize it where practical;
4. determine whether the outcome is a panic, nontermination, resource problem, or incorrect classification;
5. add the minimal input to a durable regression test or appropriate golden corpus;
6. fix the implementation;
7. rerun the focused test, smoke campaign, and complete host gate.

Do not claim that fuzzing proves the absence of parser, memory, or denial-of-service defects. It provides evidence for the exercised targets and inputs.

Generated fuzz artifacts, coverage files, and fuzz build output are ignored by Git and should not be committed accidentally.

## Target compilation

Build the `fsh` executable for the Redox target with:

```bash
cd components/flashshell
redoxer build -p flashshell-cli --bin fsh
```

From the repository root, the helper equivalent is:

```bash
source ./flashos.sh
flashos shell target
```

The target build verifies that the selected crate graph compiles for the Redox environment. It can detect issues such as:

- unsupported dependencies;
- incorrect conditional compilation;
- host-only APIs leaking into target code;
- missing target implementations;
- incompatible feature selection.

It does not execute the binary or qualify:

- interactive line editing;
- process launching;
- redirections;
- pipelines;
- job control;
- terminal ownership;
- login-shell integration.

Those properties require an assembled image and target-side runtime evidence.

## FlashOS image integration

FlashShell is packaged through:

```text
recipes/terminal/flashshell/recipe.toml
```

The recipe fetches the FlashOS repository at a pinned Git revision and builds the `fsh` binary from `flashshell-cli`.

This has an important consequence:

> Uncommitted component changes in the current checkout are not automatically consumed by a normal FlashOS recipe build.

To integrate a new FlashShell revision:

1. complete the component host checks;
2. complete the Redox target build where required;
3. commit the intended component state;
4. update the recipe's pinned revision deliberately;
5. rebuild the FlashShell package;
6. rebuild the image from its declared profile;
7. run the required target-side verification.

Using the repository helper:

```bash
source ./flashos.sh

flashos recipe rebuild flashshell
flashos build disk
flashos smoke disk
```

A package push into an existing development image can shorten an iteration cycle, but it is not evidence that a clean image build resolves and installs the same package correctly.

For the repository-level package and image workflow, see [FlashOS Development](../../../docs/development.md). For the meaning of package, image, QEMU, and hardware evidence, see [FlashOS Verification](../../../docs/verification.md).

Do not replace the recipe revision with a floating branch. Image construction must resolve the shell source to an immutable revision.

## Dependencies and supply-chain policy

The component dependency graph is controlled by:

```text
Cargo.toml
Cargo.lock
deny.toml
```

The workspace policy checks:

- known security advisories;
- accepted licenses;
- duplicate and wildcard dependency policy;
- permitted registries and source types.

With `cargo-deny` installed, run the component policy locally:

```bash
cd components/flashshell
cargo deny check advisories bans licenses sources
```

When adding or updating a dependency:

1. confirm that the owning crate needs it;
2. prefer keeping dependencies out of lower-level crates when the behavior belongs at a higher boundary;
3. inspect default features and disable unnecessary ones;
4. determine whether the dependency reaches the shipped target binary;
5. update `Cargo.toml` and `Cargo.lock` together;
6. review newly introduced transitive packages;
7. run the dependency policy;
8. run formatting, Clippy, and tests;
9. run the Redox target build when the dependency reaches target-selected code.

A dependency that works on a Linux development host may still be unsuitable for the Redox target.

The hosted security workflow also evaluates the FlashShell manifest independently from the root Cargo workspace. The detailed hosted contract belongs in [CI/CD Contracts](../../../ci/README.md).

## Generate API documentation

Generate local Rust API documentation for all FlashShell crates with:

```bash
cd components/flashshell
cargo doc --workspace --no-deps
```

The generated documentation is written below:

```text
components/flashshell/target/doc/
```

Open the CLI crate index at:

```text
target/doc/flashshell_cli/index.html
```

Use the generated crate documentation when working with public Rust types, traits, and module boundaries. The Markdown guides remain responsible for cross-crate concepts, public language behavior, and development workflow.

Build output under `target/` is ignored by Git and must not be committed.

## Keep documentation synchronized

A behavior change may require updates in more than one document.

| Change | Documentation to inspect |
| --- | --- |
| Tokens, grammar, values, expressions, functions, signatures, modules, imports, exports, or name resolution | [Language Guide](language-guide.md) |
| Script execution, script arguments, checking, formatting, redirections, statuses, or jobs | [Scripting](scripting.md) |
| Dependency direction, analysis services, platform interfaces, adapters, or process lifecycle | [Architecture](architecture.md) |
| Formatter, checker, help, language-server, test, fixture, fuzzing, or integration workflow | This guide |
| Component purpose, v1 boundary, or public availability wording | [FlashShell overview](../README.md) |
| Image package, target evidence, or system integration | Main [FlashOS documentation](../../../docs/README.md) |

Examples in public documentation must use supported FlashShell syntax. Verify them against the parser, runtime, tests, or executable behavior rather than adapting POSIX shell examples by appearance.

Keep exact versions, test counts, and other frequently changing values in their authoritative manifests or inventories unless a reader needs the value to perform the documented procedure.

## Before considering a change complete

Select the checks required by the affected layer rather than treating every command as interchangeable.

### Every Rust change

```bash
cd components/flashshell

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

### Syntax or parser change

Also verify:

- focused lexer or parser tests;
- relevant lexical and grammar golden entries;
- input completeness classification;
- formatter and property invariants;
- the bounded fuzz smoke campaign;
- language documentation.

### Formatter change

Also verify:

- focused formatter tests;
- parse-format-parse structural stability;
- formatter idempotence;
- check mode without writes;
- write mode on representative fixtures;
- preservation of comments and documentation metadata;
- relevant Language and Scripting documentation.

### Static checker or module-analysis change

Also verify:

- checking performs no execution;
- module path canonicalization;
- import-cycle diagnostics;
- local, imported, exported, private, duplicate, and missing-name cases;
- function and command signatures;
- pipeline compatibility;
- deterministic diagnostics and CI-facing status behavior;
- relevant Language, Scripting, and Architecture documentation.

### Language-server or help change

Also verify:

- reuse of shared parser and analysis APIs;
- protocol request and response tests;
- multi-file module and name resolution;
- signature and documentation information;
- deterministic diagnostics;
- cancellation or stale-document behavior where applicable;
- no execution of user source during inspection;
- relevant Language and Architecture documentation.

### Runtime or command change

Also verify:

- focused runtime tests;
- error and cancellation paths;
- execution-plan and preflight behavior;
- carrier compatibility;
- status aggregation;
- scripting or language documentation.

### Platform, process, descriptor, signal, or terminal change

Also verify:

- abstract platform tests;
- concrete-adapter tests;
- deterministic fixture coverage;
- CLI or PTY coverage where applicable;
- resource cleanup and restoration;
- the Redox target build;
- target-side execution when the changed capability is used in FlashOS.

### Dependency change

Also verify:

```bash
cargo deny check advisories bans licenses sources
redoxer build -p flashshell-cli --bin fsh
```

Review `Cargo.lock` and the target-visible transitive graph.

### Image-facing change

Also verify:

- the pinned recipe revision;
- a clean package build;
- a clean image build;
- the applicable QEMU runtime contract.

Before finalizing the change, review the working tree for:

- generated `target/` content;
- fuzz artifacts or corpora;
- editor swap files;
- accidental lockfile changes;
- debugging output;
- host-specific paths;
- private project material;
- documentation that still describes the old behavior.

## Related documentation

- [Language Guide](language-guide.md) — Language syntax, values, expressions, commands, and structured pipelines.
- [Scripting](scripting.md) — Script execution, external processes, redirections, statuses, and jobs.
- [Architecture](architecture.md) — Crate boundaries, execution planning, platform capabilities, and process lifecycle.
- [FlashShell overview](../README.md) — Component purpose, public boundaries, and documentation entry point.
- [FlashOS Development](../../../docs/development.md) — Repository, package, profile, and image development.
- [FlashOS Verification](../../../docs/verification.md) — Evidence layers and qualification boundaries.
- [CI/CD Contracts](../../../ci/README.md) — Local CI scripts and hosted workflow contracts.

---

[← Previous: Architecture](architecture.md) · [FlashShell documentation](README.md) · [Next: CI/CD Contracts →](../../../ci/README.md)
