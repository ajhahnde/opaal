# Flash Development

[FlashOS](../../../README.md) › [Flash](../README.md) › [Documentation](README.md) › Development

This guide describes the component-specific workflow for building, testing,
documenting, and integrating Flash. It is intended for developers changing the
language implementation, runtime, platform adapters, interactive front end,
`fsh`, or `flash-language-server`; repository-wide image development and
verification policy remain documented under the main FlashOS documentation.

> **Project status:** FlashOS as a complete operating system remains pre-alpha
> software. Flash 1.0.0 is released as the component contract in the current
> source. This guide supports its implementation and verification. Availability
> in a particular FlashOS image or on another target is qualified separately;
> tests on a Linux or macOS host are not proof of FlashOS target support.

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
- [Scheduling stress](#scheduling-stress)
- [Performance benchmarks](#performance-benchmarks)
- [Fuzzing](#fuzzing)
- [Target compilation](#target-compilation)
- [FlashOS image integration](#flashos-image-integration)
- [Dependencies and supply-chain policy](#dependencies-and-supply-chain-policy)
- [Generate API documentation](#generate-api-documentation)
- [Keep documentation synchronized](#keep-documentation-synchronized)
- [Before considering a change complete](#before-considering-a-change-complete)

## Development scope

Flash is maintained as an independent Cargo workspace under:

```text
components/flash/
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

Run component-level Cargo commands from `components/flash/`. Running Cargo from the FlashOS repository root addresses the separate root build-system package and does not exercise the Flash workspace.

Development evidence is layered:

| Check                      | What it establishes                                                      |
| -------------------------- | ------------------------------------------------------------------------ |
| Formatting and Clippy      | Source-format and lint compliance for the host build                     |
| Host tests                 | Portable behavior and supported host-platform integration                |
| Scheduling stress          | Replayable host process, pipeline-cancellation, and job-control schedules   |
| Performance benchmarks     | Retained host and exact-image target samples evaluated against evidence-derived, environment-specific budgets |
| Fuzzing                    | Resilience of syntax and ordinary-word expansion against generated inputs  |
| `redoxer` builds           | Compilation of the shipped Flash executables for the Redox target environment |
| Package build              | Construction of the checkout-bound Flash workspace through the FlashOS recipe |
| Image build                | Inclusion of that package in an assembled FlashOS image                  |
| QEMU or hardware execution | Runtime behavior in the produced system                                  |

Passing one layer does not imply that later layers pass. In particular, host tests do not prove Redox behavior, and target compilation does not prove login-shell or terminal behavior inside a FlashOS image.

Use [FlashOS Verification](../../../docs/verification.md) for the repository-wide evidence model.

## Toolchains and prerequisites

### Stable component toolchain

The main workspace pins its compiler and required components in [`rust-toolchain.toml`](../rust-toolchain.toml). When `rustup` is installed, entering the component directory and running Cargo causes the pinned toolchain to be selected automatically.

Verify the selected tools with:

```bash
cd components/flash

rustc --version
cargo --version
cargo fmt --version
cargo clippy --version
```

Do not silently substitute the FlashOS root toolchain. The root repository and Flash are separate Cargo workspaces and may intentionally pin different compiler versions.

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

### FlashOS target baseline

The tracked
[`platforms/flashos-x86_64.toml`](../platforms/flashos-x86_64.toml) file records
the Rust target, target-compiler identity, C runtime, dynamic linker, and ELF
contract used by FlashOS integration. Validate its source-owned fields from the
repository root:

```bash
python3 ci/check_flashos_platform.py
```

After an image build has populated the compiler fingerprint and staged package
trees, validate the observed toolchain, package metadata, and ELF outputs:

```bash
python3 ci/check_flashos_platform.py --artifacts
```

The artifact mode verifies the target that produced the staged `fsh`; it is not
a replacement for target execution, QEMU qualification, or a capability
report. Update the baseline only from an intentional toolchain, ABI, or
source-recipe transition, not merely to accept unexplained build drift.

The adjacent
[`platforms/flashos-x86_64-capability-evidence.toml`](../platforms/flashos-x86_64-capability-evidence.toml)
file compares the complete portable capability enum with the current Redox
executable path, selected adapter source, and existing FlashOS QEMU
observations. Validate its source markers and evidence references from the
repository root:

```bash
python3 ci/check_flashos_capabilities.py
```

This inventory is deliberately not a support matrix. A source method or a
capability declaration does not establish target behavior, and a missing
runtime observation does not establish that the target is incapable of the
operation.

The adjacent
[`platforms/flashos-x86_64-operation-map.toml`](../platforms/flashos-x86_64-operation-map.toml)
file maps every requirement from that inventory to the current Flash-internal,
Rust standard-library, direct `relibc`, or unrouted boundary. Validate its
ordered coverage and source identities from the repository root:

```bash
python3 ci/check_flashos_operation_map.py
```

The map deliberately stops Rust standard-library routes at public APIs because
the target compiler source commit is unknown. Direct C ABI routes cite the
configured `relibc` source revision, which candidate image qualification also
requires in the staged package. Mapping is not support classification or
runtime qualification; keep those later claims in their own reviewed changes
with their required evidence.

The separate
[`platforms/flashos-x86_64-capability-classification.toml`](../platforms/flashos-x86_64-capability-classification.toml)
file consumes that complete map and gives every operation and capability an
architectural route verdict. Validate its ordered coverage, aggregation, and
qualification boundary from the repository root:

```bash
python3 ci/check_flashos_capability_classification.py
```

The current classification records 41 native operations and one
three-operation FlashOS policy shim for standard-directory selection. No
operation is deliberately unsupported or requires kernel work. These verdicts
select implementation routes; the classification artifact does not itself make
a runtime-support claim. Redox-target executables select the
`flash-platform-flashos` crate. Its image-qualified declaration enables every
capability group except signals, whose complete stop/continue/termination
transition vocabulary remains unqualified and therefore unavailable.

## Workspace layout

The current workspace membership is defined by [`components/flash/Cargo.toml`](../Cargo.toml). Do not treat a fixed crate count as a permanent project contract.

The principal implementation responsibilities are:

| Concern | Current owner |
| --- | --- |
| Source files, spans, lexer, parser, syntax trees, canonical formatting, and diagnostics | `flash-syntax` |
| Values, scopes, evaluation, functions, command metadata, planning, pipelines, modules, sessions, and jobs | `flash-runtime` and shared analysis interfaces |
| Portable operating-system capability contracts and deterministic test adapters | `flash-platform` |
| Unix-like process, descriptor, filesystem, signal, and terminal operations | `flash-platform-posix` |
| FlashOS target policy, classified route composition, and qualification-gated adapter | `flash-platform-flashos` |
| CLI modes, interactive editing, configuration, history, tooling entry points, and executable assembly | `flash-cli` |
| Versioned overlays, JSON-RPC/LSP projection, stdio framing, and language-server assembly | `flash-lsp` |

Supporting directories hold component documentation, fuzz targets, end-to-end tests, fixture executables, and golden corpora. Inspect the workspace manifest and the relevant directory before documenting an exact package or test inventory.

Read [Flash Architecture](architecture.md) before moving behavior across responsibility boundaries or introducing a dependency in the opposite direction.

## Local development loop

Use the narrowest relevant tests while iterating, then run the complete component gate before considering the change complete.

A normal loop is:

```bash
cd components/flash

cargo fmt --all
cargo test -p affected-package --locked
cargo clippy -p affected-package --all-targets -- -D warnings
```

Replace `affected-package` with the owning package, for example:

```bash
cargo test -p flash-syntax --locked
cargo test -p flash-runtime --locked
cargo test -p flash-cli --locked
cargo test -p flash-lsp --locked
```

Before completing the change, run the full host gate:

```bash
python3 ../../ci/check_flash_conformance.py
python3 ../../ci/check_flash_release.py
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

The machine-readable
[`conformance/v1.toml`](../conformance/v1.toml) inventory maps each frozen
host-v1 semantic family to enabled workspace tests. The checker also verifies
CI wiring, the complete platform-contract list, and the classification of
intentional runtime refusals and executor invariants. The locked workspace test
run executes every listed owner; neither command substitutes for target
compilation or FlashOS runtime qualification.

The machine-readable [`release/v1.toml`](../release/v1.toml) record binds the
Flash 1.0.0 package version to that frozen contract, the exhaustive user-path
inventory, retained host evidence, FlashOS target matrix, current public
claims, and candidate CI commands. The ready candidate workflow executes those
owners and boots the exact in-tree package source before the release tree can
reach protected `main`.

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
cd components/flash

cargo build -p flash-cli --bin fsh --locked
```

The development binary is written below the component workspace:

```text
target/debug/fsh
```

Run an interactive host session through Cargo:

```bash
cargo run -p flash-cli --bin fsh
```

Run a script:

```bash
cargo run -p flash-cli --bin fsh -- path/to/program.fsh
```

Inspect the command-line interface:

```bash
cargo run -p flash-cli --bin fsh -- --help
cargo run -p flash-cli --bin fsh -- --version
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
| CLI                | Argument handling, host-status mapping, report streams, startup configuration, editor services, interactive recovery, history, completion, and highlighting |
| Black-box and PTY  | Executable statuses and channels, prompts, control characters, terminal ownership, signals, recovery, and job control                                      |
| Fuzzing            | Public lexer, parser, and ordinary-word expansion entry points with arbitrary bytes                                                                    |

Run a named integration test with Cargo's `--test` selector:

```bash
cargo test -p flash-syntax --test parser
cargo test -p flash-runtime --test pipeline
cargo test -p flash-cli --test e2e
```

Pass a test-name filter after the target when narrowing a failure:

```bash
cargo test -p flash-runtime --test pipeline pipeline_name_fragment
```

Use `--nocapture` when a test intentionally writes useful diagnostic output:

```bash
cargo test -p flash-cli --test pty -- --nocapture
```

Do not treat a single filtered test as the final gate. Tests in adjacent layers often enforce the same public contract from different boundaries.

## Develop syntax and parsing

Changes to tokens, grammar, precedence, input completeness, syntax trees, source formatting, or diagnostics belong primarily in `flash-syntax`.

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
cargo test -p flash-syntax --test lexical_golden
cargo test -p flash-syntax --test grammar_golden
```

Then run all syntax tests:

```bash
cargo test -p flash-syntax --locked
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

The shipped launcher surface is:

```text
fsh format --check [--] PATH...
fsh format --write [--] PATH...
fsh format --help
```

Formatter changes must preserve all of the following:

- parsing succeeds before formatting;
- the formatted result parses to the same supported program structure;
- formatting is idempotent;
- check mode detects noncanonical source without rewriting it;
- write mode produces canonical source;
- comments and documentation metadata remain attached to the intended constructs;
- every write batch completes source and filesystem preflight before mutation;
- changed files use permission-preserving same-directory atomic replacement;
- final symlinks, nonregular files, stale sources, and duplicate canonical targets are refused;
- the launcher frontend does not initialize or execute a Flash runtime session;
- golden fixtures cover representative valid, incomplete, and invalid boundaries where formatting interacts with parsing.

Formatter commands and options must remain synchronized with the implemented CLI contract and its tests.

The focused frontend and real-filesystem suites are:

```bash
cargo test -p flash-cli cli::tests --lib
cargo test -p flash-cli --test format_frontend
cargo test -p flash-cli --test formatter_e2e
```

The injected `FormatFilesystem` suite owns deterministic orchestration, while
the host suite owns native-path inspection, permission preservation, stale
source detection, temporary cleanup, and atomic rename behavior. Neither
replaces the existing `flash-syntax` formatter grammar, idempotence, structural,
significant-token, or documentation-comment tests.

### Static checker contract

The shipped launcher surface is:

```text
fsh check [--] SOURCE
fsh check --help
```

`flash-runtime` owns a report-oriented analysis path over the canonical root
and recursively reachable imports. Discovery retains every successfully decoded
source and accumulates independent branch failures in first-visit depth-first
order. A complete graph enables accumulating name analysis; clean names enable
accumulating signature analysis. Poisoned invalid owners suppress dependent
cascades, and no partial `ModuleProgram` escapes after any error. Static
pipeline analysis walks every retained parsed source even when an earlier phase
fails.

Runtime planning, static analysis, help, completion, and editor queries share
source-independent carrier and argument contracts from `CommandSignature`.
The checker maps exact built-in argument faults to `CMD003`-`CMD006` and carrier
faults to `PIP001`-`PIP004` without word expansion. Forced and assumed
externals use byte-stream carrier contracts, while dynamic heads and
interpolation-dependent argument facts remain unknown. The checker must not
probe `PATH` or infer facts that the shared schema and static source do not
establish.

Successful module analysis also owns source-spanned direct and transitive
initializer-effect summaries. The shared vocabulary distinguishes working
directory, child environment, status, output, filesystem read/write, process,
job, program exit, and opaque external behavior. Named dependencies fold once
in runtime initialization order; load-only edges remain absent from transitive
runtime summaries; known callable bodies fold into their call sites; and
indirect or external behavior stays conservative. This model is descriptive:
valid effectful modules remain silent checker successes, and no frontend may
add an effect warning or probe the host to make a summary more precise.

The `flash-cli` checker frontend receives only injected canonicalization and
finite source-loading capabilities. Its host adapter resolves canonical aliases
and reads regular files without constructing a runtime session, environment,
executable probe, platform, terminal, configuration loader, or history store.
Checker development must cover:

- parsing and incomplete-input handling;
- canonical module resolution;
- import-cycle diagnostics;
- local, imported, exported, private, and missing names;
- mutable, immutable, imported-snapshot, captured, and unknown assignment
  targets, plus statically known assignment-type mismatches;
- exact built-in positional and option schemas with conservative dynamic-word
  and spread handling;
- function and command signatures;
- pipeline carrier compatibility;
- direct and named-dependency-folded initializer effects;
- stable source spans and deterministic diagnostic ordering;
- success and failure behavior suitable for CI.

The focused frontend and executable suites are:

```bash
cargo test -p flash-cli cli::tests --lib
cargo test -p flash-cli --test check_frontend
cargo test -p flash-cli --test checker_e2e
```

Checker tests must prove silent status-0 success, stderr-only status-1 analysis
failure, status-2 invocation misuse, deterministic multi-source rendering, and
that analysis does not initialize modules, start external processes, apply
redirections, mutate the caller's environment, change the working directory,
or require interactive terminal state.

### Execution-plan inspection contract

The shipped planner surface is:

```text
fsh plan [--] SOURCE
fsh plan --help
```

`SOURCE` must contain exactly one top-level foreground command pipeline. The
planner frontend shares canonical loading, parsing, static command analysis,
ordinary expansion/resolution, structural preflight, and
`ExecutionPlan::render`. It may receive an inherited cwd/environment snapshot
and a read-only executable probe, but it must not gain a runtime platform,
session, config/history provider, writable filesystem, editor, terminal, or
process capability. Tests must prove that substitutions are rejected and that
redirection targets and external fixtures remain untouched.

Focused planner coverage is:

```bash
cargo test -p flash-runtime --test plan
cargo test -p flash-cli --test plan_frontend --test planner_e2e
```

### Help and documentation metadata

Built-in and user-function help must use the same names, signatures, and documentation metadata consumed by static analysis and editor tooling. Tests should prevent help text, checker signatures, and language-server information from drifting into separate incompatible definitions.

The lossless lexer distinguishes complete-line `##` tokens, while the parser
retains only exact attached spans on named functions. Normalization belongs to
shared runtime analysis, not the formatter or a frontend. `FunctionSignature`
owns resolved callable metadata; `CommandSignature` owns carriers, flags,
invocation, and prose. The host-free help catalog derives entries from those
owners and `ScopeStack::visible_bindings`.

The planner recognizes only static `help [NAME]`, snapshots its selected
entries, and stores them on the planned stage. The internal executor renders
that snapshot as bytes without receiving the live scope or callable body. Help
tests must cover attachment and normalization, registry documentation
completeness, exact ordering, lexical shadowing, imported defining ownership,
same-name built-in/function entries, unknown and dynamic queries, byte routing,
and panic-on-use process/platform adapters.

Help lookup is inspection-only and must not execute the documented callable,
probe an executable, or mutate the session merely to discover metadata.

### Built-in namespace changes

The standard manifest in
[`builtin.rs`](../crates/flash-runtime/src/builtin.rs) is the sole inventory for
core commands, aliases, reservations, and lifecycle metadata. A namespace
change must not add a checker-owned, completion-owned, help-owned, or
executor-owned copy of the inventory. The public Scripting and Language Guide
inventories are checked against the standard manifest by the focused registry
suite.

Before changing the namespace, classify the source compatibility effect. A new
core command, alias, or reservation under a previously unknown name requires a
language-major decision because it changes bare-name external resolution.
Activation of a name already reserved for the current major may be compatible.
Removing or renaming a core command, removing or retargeting an alias, releasing
a reservation, changing entry class outside reserved activation, or changing a
successful command contract also requires explicit semantic review and normally
the next language major.

Every namespace change must cover the affected boundaries:

- manifest validation, exact class inventories, deterministic order, lifecycle
  rules, alias canonicalization, and public inventory alignment;
- resolution, planning, canonical executor dispatch, background
  classification, forced-external bypass, and native non-UTF-8 behavior;
- host-free `CMD001` deprecation warnings and `CMD002` reserved errors,
  including ordering, status, source spans, bypasses, and carrier-cascade
  suppression;
- help kinds, exact queries, lifecycle and target rendering, canonical alias
  metadata reuse, and unchanged output for an unaffected core-only manifest;
- completion inclusion of core and alias names, canonical alias flags, and
  reserved-name exclusion; and
- all five `which` result kinds, ordered `name`/`kind`/`target`/`path` fields,
  path and target population, and final status.

Run the focused registry, planner, built-in, module-analysis, help/session,
completion, and interactive-driver suites before the complete locked workspace
tests and strict all-feature/all-target Clippy gate. Namespace policy changes
also require the public-boundary scan and review of Scripting, the Language
Guide, Architecture, the Flash overview, and the changelog.

### Language-server contract

The Flash language server is required for the v1 tooling surface. It reuses the
shared parser, syntax tree, module graph, name resolution, signatures,
diagnostics, command metadata, completion context, and formatter rather than
implementing another version of the language.

For an installed package, configure an editor's LSP client to start:

```text
/usr/bin/flash-language-server
```

For a component-workspace development session, the exact equivalent is:

```bash
cd components/flash
cargo run --locked -p flash-lsp --bin flash-language-server
```

Both invocations are stdio servers. Do not pass a source path or shell option,
and do not treat stdout as a terminal stream: it contains only
`Content-Length`-framed JSON-RPC messages. A generic editor registration should
associate `.fsh` files with a Flash language identifier and use this launch
record:

```text
command: ["flash-language-server"]
transport: stdio
document selector: .fsh files
```

Flash does not bundle an editor extension; use the editor's generic LSP client.
The client must send absolute `file:` document URIs and full document text for
every accepted change. The server
advertises open/close plus full synchronization, UTF-8 positions when offered
and UTF-16 otherwise, and these methods:

```text
textDocument/completion
textDocument/hover
textDocument/signatureHelp
textDocument/definition
textDocument/references
textDocument/formatting
```

Diagnostics arrive through `textDocument/publishDiagnostics`. The lifecycle and
sync surface also uses `initialize`, `initialized`, `shutdown`, `exit`,
`$/cancelRequest`, `textDocument/didOpen`, `textDocument/didChange`, and
`textDocument/didClose`. Incremental edits, non-file URIs, TCP or socket
transport, project configuration, workspace file discovery, dynamic
registration, and unadvertised methods are not supported.

Language-server development must preserve immutable generation-scoped
snapshots, deterministic diagnostics and queries, exact cancellation/stale
result responses, and one response for every request. It must also prove that
effectful source cannot initialize a module, execute a command, probe an
executable or `PATH`, apply a redirection, mutate cwd or the environment, access
a terminal or session, or load shell configuration/history.

Run the focused language-tooling gate with:

```bash
cd components/flash
cargo test -p flash-syntax --locked
cargo test -p flash-runtime --test modules --locked
cargo test -p flash-cli --test check_frontend --locked
cargo test -p flash-cli --test checker_e2e --locked
cargo test -p flash-cli --test context_aware_completion --locked
cargo test -p flash-cli --test dependency_direction --locked
cargo test -p flash-lsp --locked
```

Then run the complete locked workspace test, formatting, and strict lint gates.
When the packaged executable or target-visible dependencies change, also build
`flash-language-server` with `redoxer` and validate the package/profile contract
before claiming target availability. Existing editor-local highlighting or
completion does not replace this language-server contract.

## Develop the runtime

Runtime changes belong in `flash-runtime` when they affect:

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
- sessions or background jobs;
- module initialization or effect analysis.

The runtime tests are organized by behavior rather than by one monolithic integration target. Use the test file nearest to the changed contract, then run the full runtime package.

Examples:

```bash
cargo test -p flash-runtime --test values
cargo test -p flash-runtime --test expansion
cargo test -p flash-runtime --test plan
cargo test -p flash-runtime --test preflight
cargo test -p flash-runtime --test structured
cargo test -p flash-runtime --test jobs
```

### Prefer platform test doubles

Portable runtime tests should use the fake or recording implementations from `flash-platform` when the contract can be expressed through platform requests and responses.

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
- caller-environment commit versus external no-rollback boundaries;
- whole-program initializer exit and background-job precedence;
- fatal output failure after a written prefix;
- stopped and continued process observations;
- status aggregation with and without `pipefail`.

For a mixed-pipeline executor change, also cover more than one internal segment:

- alternating start/end topologies and adjacent external runs;
- large bounded streaming and downstream early close;
- repeated carrier and `|&` preflight at every boundary;
- source-ordered status leaves, default selection, `pipefail`, and deferred
  external-predecessor `check`;
- failure, cancellation, child reaping, endpoint release, and terminal
  restoration from first, middle, and last segments;
- pending-state commit and rollback, source-ordered closure deltas, and explicit
  `exit`; and
- local descriptor override/EOF plus interactive, script, and supervised
  background-chain parity.

Do not make structured streams or their pull closures cross threads merely to
schedule another segment. The concurrent boundary is an owned byte descriptor.

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

Changes to capability contracts, owned process handles, descriptors, clocks, directory streams, or test adapters belong in `flash-platform`.

Changes that call shared Unix-like operating-system interfaces belong in `flash-platform-posix`. FlashOS-specific policy, route composition, and qualification state belong in `flash-platform-flashos`; target details must not move into `flash-runtime`.

Run the platform-contract tests with:

```bash
cargo test -p flash-platform --locked
```

Run the concrete-adapter tests with:

```bash
cargo test -p flash-platform-posix --locked
```

Run the FlashOS adapter and policy tests with:

```bash
cargo test -p flash-platform-flashos --locked
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

Low-level operating-system calls may require explicitly scoped unsafe code in
the concrete adapter. Do not move such code into the syntax, runtime, abstract
platform, or CLI crates to avoid an adapter boundary. Workspace lints reject
undocumented unsafe blocks, undocumented public unsafe functions, and implicit
unsafe operations inside unsafe functions across every target, including tests
and process fixtures.

For every new unsafe block:

- keep the block as small as practical;
- place a local `SAFETY:` comment at the block and document its pointer,
  initialization, lifetime, ownership, concurrency, signal, and FFI conditions
  as applicable;
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

Validate the checked FlashOS route classification, reusable runtime fixtures,
versioned advertised-capability report, and exhaustive target matrix from the
repository root:

```bash
python3 ci/check_flashos_capability_classification.py
python3 ci/check_flashos_capability_report.py
python3 ci/check_flashos_target_matrix.py
```

Render the ordered smoke inputs and exhaustive matrix observations for a
manually observed target with:

```bash
python3 ci/flashos_runtime_fixtures.py
python3 ci/flashos_target_matrix.py
```

The report and reusable smoke fixtures remain bounded evidence. The separate
target matrix covers every advertised operation and the required target
surfaces through exact ordered cases. Neither rendered checklist is evidence
that an operator ran it, and neither contract establishes physical-hardware or
release qualification. The withheld `Signals` group remains outside both.

The [Flash v1 exercise contract](../exercises/README.md) adds the exhaustive
user-path inventory above these focused layers. Its retained host report and
assembled-image matrix record exact actions and observations while keeping host,
QEMU, withheld-capability, and approval-gated physical evidence distinct.

## Develop the CLI and interactive session

Changes to command-line parsing, startup modes, configuration, prompts, completion, highlighting, history, line editing, interactive recovery, or top-level session control belong in `flash-cli`.

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
cargo test -p flash-cli --test config_startup
cargo test -p flash-cli --test context_aware_completion
cargo test -p flash-cli --test interactive_session
cargo test -p flash-cli --test terminal_editor
cargo test -p flash-cli --test e2e
cargo test -p flash-cli --test pty
```

Some interactive and PTY tests are host-specific. Cargo's target configuration controls whether host-only dependencies and tests are compiled.

### Test status and channel reporting

Keep host process policy in `flash-cli`. Runtime tests should prove structured
completion, failure, cleanup, and ordered background reports without observing
process-global stdout, stderr, or exit codes. CLI report tests should inject
writers and exercise exact status mapping, checked writes and flushes, output
failure, diagnostic failure, and the rule that stderr is never reused
recursively after it fails.

Black-box script tests must assert all three observable fields together:

- the exact process status;
- exact stdout bytes; and
- exact stderr bytes or a deliberately stable diagnostic prefix and newline.

Cover ordinary completed codes separately from shell-owned failure and launcher
misuse, including codes 1 and 2. Keep impossible or platform-unrepresentable
status shapes at the pure mapper seam rather than trying to fabricate them with
a real child process. Do not classify failures by matching rendered prose.

Interactive driver tests use injected program and diagnostic writers to prove
checked flushing, recoverable diagnostics, non-recursive diagnostic failure,
and fatal cleanup before status 1. PTY tests remain responsible for edit Ctrl-C,
foreground child signals, EOF, exact explicit exit, live-job refusal and
hang-up, prompt-safe notices, and terminal restoration.

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

A fixture is test infrastructure, not part of the installed Flash package.

## Scheduling stress

The [`scheduling/`](../scheduling/) runner exercises the real `fsh` executable
over a host pseudoterminal. Fixed seeds remain part of the ordinary workspace
test gate. A separate bounded campaign derives more exact seeds from one
recorded campaign seed and retains a manifest plus complete output:

```bash
cd components/flash
./scheduling/run-campaign.sh
```

Pass a case count, a new result directory, and an optional nonzero campaign
seed when longer or independently replayable evidence is needed:

```bash
./scheduling/run-campaign.sh 256 /path/to/new-results 0x4f3c2b1a098765ef
```

The randomized choices cover multi-member pipeline size, stop/resume cycles,
job-table observations, concurrent completion release order, exit cleanup, and
foreground, background, or stopped cancellation placement. Every child must be
reaped, terminal ownership must return to `fsh`, notices must remain prompt
safe, and the session must accept another command.

This is Linux/macOS host evidence. The same seed preserves the generated
actions and assertions, but the host kernel still controls exact process and
thread timing. Redox target compilation and FlashOS image/QEMU qualification
remain separate evidence layers and do not inherit host signal or job-control
claims from this campaign.

See the [scheduling stress README](../scheduling/README.md) for bounds, retained
files, exact-seed replay, and the failure-to-regression workflow.

## Performance benchmarks

The versioned [`benchmarks/`](../benchmarks/) suite measures optimized `fsh`
startup, first-prompt latency, simple command overhead, pipeline throughput,
structured-stream peak memory, and completion latency. Run its bounded smoke
profile during ordinary development:

```sh
python3 benchmarks/run.py --profile smoke
```

Use the qualification profile only when collecting or comparing retained
evidence. Absolute budgets are keyed to their environment; do not compare a
Linux CI runner with the macOS reference or treat TCG/serial observations as
physical-hardware performance. The exact-image QEMU consumer collects the
applicable target cases and evaluates them separately.

See the [performance benchmark README](../benchmarks/README.md) for the cold and
warm definitions, fixtures, repeats, noise controls, raw JSON schema, budget
derivation, regression policy, target exclusions, and exact commands.

## Fuzzing

The separate [`fuzz/`](../fuzz/) workspace contains libFuzzer targets for the public lexer, parser, and ordinary-word expansion entry points.

All targets accept arbitrary bytes:

- valid UTF-8 is passed through the syntax APIs;
- invalid UTF-8 exercises source loading and normal rejection;
- golden `.fsh` files seed the mutation corpus.

The expander target evaluates parsed command words against a fixed in-memory
scope. Its public pure-evaluation boundary does not launch processes or perform
platform I/O.

From the component workspace, run the bounded smoke campaign:

```bash
cd components/flash
./fuzz/run-smoke.sh
```

The default campaign runs a bounded number of executions for each target. Supply another nonnegative count when needed:

```bash
./fuzz/run-smoke.sh 10000
```

The runner:

- invokes the `lexer`, `parser`, and `expander` targets;
- uses the grammar and lexical golden files as read-only seeds;
- creates writable corpora in a temporary directory;
- limits generated input length, per-input execution time, and resident memory;
- removes the temporary corpus when the run ends.

The smoke runner is a fast regression check. It is not equivalent to a long-running fuzz campaign.

For a sustained campaign, use the time-bounded runner. Its default is ten minutes
per target:

```bash
./fuzz/run-campaign.sh
```

Supply a positive duration in seconds and, optionally, a new result directory:

```bash
./fuzz/run-campaign.sh 3600 /path/to/results
```

The selected result directory must not already exist, preventing one campaign
from silently mixing its evidence with an earlier run.

The default result path is a unique ignored directory under
`fuzz/campaigns/`. Unlike the smoke runner, the sustained runner preserves each
target's writable corpus and failure artifacts for review. Both runners limit
inputs to 4,096 bytes, ten seconds of execution, and 2,048 MiB of resident
memory.

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

Build both shipped Flash executables for the Redox target with:

```bash
cd components/flash
redoxer build -p flash-cli --bin fsh
redoxer build -p flash-lsp --bin flash-language-server
```

From the repository root, the helper currently covers the `fsh` half of this
gate:

```bash
source ./flashos.sh
flashos shell target
```

Use the direct `redoxer` command above for `flash-language-server` until the
helper exposes a combined target build.

The target builds verify that both selected crate graphs compile for the Redox environment. They can detect issues such as:

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

Flash is packaged through:

```text
recipes/terminal/flash/recipe.toml
```

The recipe snapshots tracked and non-ignored files from the current
`components/flash/` workspace and builds both shipped executables: `fsh` from
`flash-cli` and `flash-language-server` from `flash-lsp`.

This has an important consequence:

> Intentional uncommitted component changes can be consumed by a local recipe
> build, while ignored generated outputs are excluded. CI and release builds
> use a clean exact checkout.

To integrate a Flash change:

1. complete the component host checks;
2. complete the Redox target build where required;
3. inspect the component workspace inputs with `git status`;
4. rebuild the Flash package;
5. rebuild the image from its declared profile;
6. run the required target-side verification;
7. commit the component and integration changes together.

Using the repository helper:

```bash
source ./flashos.sh

flashos recipe rebuild flash
flashos build disk
flashos smoke disk
```

A package push into an existing development image can shorten an iteration cycle, but it is not evidence that a clean image build resolves and installs the same package correctly.

For the repository-level package and image workflow, see [FlashOS Development](../../../docs/development.md). For the meaning of package, image, QEMU, and hardware evidence, see [FlashOS Verification](../../../docs/verification.md).

Do not replace the workspace source with a floating branch. Image construction
must resolve Flash from the same exact checkout that defines the image.

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
cd components/flash
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

The hosted security workflow also evaluates the Flash manifest independently from the root Cargo workspace. The detailed hosted contract belongs in [CI/CD Contracts](../../../ci/README.md).

## Generate API documentation

Generate local Rust API documentation for all Flash crates with:

```bash
cd components/flash
cargo doc --workspace --no-deps
```

The generated documentation is written below:

```text
components/flash/target/doc/
```

Open the CLI crate index at:

```text
target/doc/flash_cli/index.html
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
| Component purpose, v1 boundary, or public availability wording | [Flash overview](../README.md) |
| Image package, target evidence, or system integration | Main [FlashOS documentation](../../../docs/README.md) |

Examples in public documentation must use supported Flash syntax. Verify them against the parser, runtime, tests, or executable behavior rather than adapting POSIX shell examples by appearance.

Keep exact versions, test counts, and other frequently changing values in their authoritative manifests or inventories unless a reader needs the value to perform the documented procedure.

## Before considering a change complete

Select the checks required by the affected layer rather than treating every command as interchangeable.

### Every Rust change

```bash
cd components/flash

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
- full-sync versioning, absolute file-URI handling, and negotiated positions;
- framing, lifecycle, stdout purity, and exact one-response completion;
- dependency direction and package/profile installation of the separate binary;
- no execution of user source during inspection;
- relevant Architecture, Development, overview, and changelog documentation.

### Runtime or command change

Also verify:

- focused runtime tests;
- error and cancellation paths;
- execution-plan and preflight behavior;
- carrier compatibility;
- status aggregation;
- scripting or language documentation.

### CLI status, reporting, or interactive-exit change

Also verify:

- pure status-mapping and injected-writer report tests;
- complete executable status/stdout/stderr cases;
- interactive driver recovery and fatal-cleanup cases;
- the relevant PTY subset, followed by the complete PTY suite;
- no process-stream or host-exit ownership has moved into `flash-runtime`;
- Scripting and Architecture documentation.

### Platform, process, descriptor, signal, or terminal change

Also verify:

- abstract platform tests;
- concrete-adapter tests;
- FlashOS adapter and standard-directory policy tests when the target boundary changes;
- deterministic fixture coverage;
- CLI or PTY coverage where applicable;
- resource cleanup and restoration;
- the Redox target build;
- target-side execution when the changed capability is used in FlashOS.

### Dependency change

Also verify:

```bash
cargo deny check advisories bans licenses sources
redoxer build -p flash-cli --bin fsh
redoxer build -p flash-lsp --bin flash-language-server
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
- [Flash overview](../README.md) — Component purpose, public boundaries, and documentation entry point.
- [FlashOS Development](../../../docs/development.md) — Repository, package, profile, and image development.
- [FlashOS Verification](../../../docs/verification.md) — Evidence layers and qualification boundaries.
- [CI/CD Contracts](../../../ci/README.md) — Local CI scripts and hosted workflow contracts.

---

[← Previous: Architecture](architecture.md) · [Flash documentation](README.md) · [Next: CI/CD Contracts →](../../../ci/README.md)
