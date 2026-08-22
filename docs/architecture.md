# Flash Architecture

[FlashOS](../../../README.md) › [Flash](../README.md) › [Documentation](README.md) › Architecture

This document describes the internal architecture of Flash: crate boundaries, source processing, runtime state, command planning, pipeline execution, platform capabilities, interactive front ends, and process lifecycle management. It is intended for maintainers and developers extending the implementation; language usage belongs in the [Language Guide](language-guide.md), while build and test procedures belong in [Development](development.md).

> **Project status:** FlashOS as a complete operating system remains pre-alpha software. However, this Flash Architecture Guide describes the intended stable Flash v1.0 architecture and component contracts. Note that not every v1 feature or platform capability is automatically available in every current FlashOS image or on every target platform, and successful execution on a Linux or macOS development host is not automatic proof of FlashOS target support.

## On this page

- [Architectural scope](#architectural-scope)
- [Design principles](#design-principles)
- [Workspace and dependency direction](#workspace-and-dependency-direction)
- [Source and syntax front end](#source-and-syntax-front-end)
- [Formatter launcher frontend](#formatter-launcher-frontend)
- [Execution-plan inspection frontend](#execution-plan-inspection-frontend)
- [Modules and static analysis](#modules-and-static-analysis)
- [Shared tooling services](#shared-tooling-services)
- [Language-server protocol adapter](#language-server-protocol-adapter)
- [Runtime and session state](#runtime-and-session-state)
- [Command resolution and execution planning](#command-resolution-and-execution-planning)
- [Pipeline execution](#pipeline-execution)
- [Platform capability boundary](#platform-capability-boundary)
- [Interactive front end](#interactive-front-end)
- [Jobs and process lifecycle](#jobs-and-process-lifecycle)
- [Diagnostics and failure containment](#diagnostics-and-failure-containment)
- [Safety and portability boundaries](#safety-and-portability-boundaries)
- [FlashOS integration](#flashos-integration)
- [Sources of truth](#sources-of-truth)

## Architectural scope

Flash is a userspace command-language implementation. It owns parsing, evaluation, structured values, internal commands, process orchestration, interactive session behavior, and the `fsh` executable.

It does not own:

- the operating-system kernel or ABI;
- authentication and login services;
- the filesystem implementation;
- external userspace commands;
- image construction or package management;
- terminal and process primitives supplied by the operating system.

The architecture therefore separates language semantics from operating-system access. Most of the implementation can be exercised without starting a process or opening a terminal, while target-specific behavior remains behind explicit interfaces.

The main execution path is:

```text
source text
    ↓
source model, lexer, parser, and diagnostics
    ↓
syntax tree
    ↓
session and evaluator
    ↓
command expansion and resolution
    ↓
inspectable execution plan
    ↓
preflight validation
    ↓
internal, external, or mixed pipeline executor
    ↓
platform capability interface
    ↓
operating-system processes, files, pipes, signals, and terminal
```

## Design principles

### One language front end

Scripts, interactive submissions, formatting, input classification, and editor services use the same syntax crate. Flash does not maintain a separate interactive grammar or translate source into another shell language.

### Planning before execution

Command words, spreads, closures, redirection targets, command resolution, pipeline carriers, and session options are captured in an execution plan before process execution begins.

Planning does not spawn a process or apply a redirection. A separate preflight step rejects invalid carrier edges, descriptor conflicts, unrepresentable native values, and other plan-level errors before platform resources are created.

### Explicit data boundaries

Internal stages exchange one of four pipeline carriers:

- no payload;
- a byte stream;
- one structured value;
- a stream of structured values.

The runtime does not implicitly display, serialize, decode, encode, collect, wrap, or flatten one carrier into another. External processes remain byte-oriented, and explicit converter commands cross the structured-data boundary.

### Direct external execution

External commands receive an executable path, native argument vector, environment snapshot, working directory, descriptor map, and process-group placement. Flash does not render the command back into source text or route it through `/bin/sh`.

### Capability-based platform access

The runtime requests named capabilities rather than assuming that every target provides POSIX process groups, signals, terminal ownership, directory enumeration, or configuration directories.

A missing capability produces a specific diagnostic. It is not silently emulated with weaker behavior.

### Bounded materialization

Streams remain lazy wherever the operation permits it. Operations that must retain command output, collect a stream, parse a complete document, or format buffered data enforce explicit resource limits rather than treating available memory as an unbounded contract.

## Workspace and dependency direction

Flash is a nested Cargo workspace rooted at [`components/flash/`](../Cargo.toml). The workspace manifest is authoritative for current package membership. The architecture is defined by responsibilities and dependency direction rather than by a permanent number of crates.

| Responsibility | Current owner |
| --- | --- |
| Source representation, spans, lexical analysis, parsing, syntax trees, formatting, and source diagnostics | [`flash-syntax`](../crates/flash-syntax/) |
| Values, scopes, evaluation, functions, command metadata, planning, pipelines, sessions, jobs, module analysis, and shared semantic services | [`flash-runtime`](../crates/flash-runtime/) and syntax-owned analysis interfaces |
| Portable operating-system capability contracts and deterministic test adapters | [`flash-platform`](../crates/flash-platform/) |
| Unix-like process, descriptor, filesystem, signal, and terminal integration | [`flash-platform-posix`](../crates/flash-platform-posix/) |
| FlashOS target policy, route composition, and qualification-gated adapter | [`flash-platform-flashos`](../crates/flash-platform-flashos/) |
| Command-line modes, interactive front ends, configuration, history, tooling entry points, and `fsh` assembly | [`flash-cli`](../crates/flash-cli/) |
| Versioned document overlays, JSON-RPC/LSP projection, stdio framing, and `flash-language-server` assembly | [`flash-lsp`](../crates/flash-lsp/) |

Portable language semantics depend on syntax and abstract platform contracts,
not on a concrete operating-system adapter. Concrete adapters depend on the
abstract capability interface. `flash-cli` selects the appropriate runtime
adapter and user-facing services. `flash-lsp` instead depends directly on
`flash-syntax` and `flash-runtime`; it has no dependency on `flash-cli`,
`flash-platform-posix`, or terminal integration and declares no direct
dependency on `flash-platform`.

New crates may be introduced as implementation responsibilities grow, but they must preserve this dependency direction and must not create an alternative grammar, evaluator, name resolver, or platform contract.

## Source and syntax front end

The [`flash-syntax`](../crates/flash-syntax/src/lib.rs) crate is the sole owner of Flash source structure.

Its public model includes:

- `SourceFile` and stable source identities;
- byte-based `Span` values and line-column conversion;
- lexical tokens and invalid-token classifications;
- parser output and incomplete-input classifications;
- syntax-tree nodes;
- structured diagnostics;
- canonical source formatting.

### Source ownership and spans

A source file owns its name and text. Syntax nodes retain spans into that source rather than copying arbitrary fragments of source text.

The same spans flow into later runtime stages. Expansion, command planning, redirection handling, carrier validation, process failures, and structured-data errors can therefore attach diagnostics to the source construct that caused the failure.

Paths and external arguments may contain native non-UTF-8 units, but Flash source itself remains UTF-8. The source model and the platform-native data model are deliberately separate.

### Parsing outcomes

Parsing distinguishes three outcomes:

```text
Complete
Incomplete
Invalid
```

A complete result contains a syntax tree. An incomplete result identifies source that may become valid with more input, allowing an interactive editor to request continuation without inventing its own delimiter logic. An invalid result contains diagnostics for source that should be reported immediately.

This distinction is reused by interactive validation rather than approximated with terminal-editor heuristics.

## Formatter launcher frontend

`fsh format --check` and `fsh format --write` are non-executing launcher modes
owned by `flash-cli`. Their pure argument classifier retains ordered native
paths, and their formatter orchestration receives only an injected filesystem
capability. It does not construct a module program, runtime session, platform
adapter, executable probe, terminal, configuration loader, or history store.

The orchestration assigns stable source identities in operand order, decodes
each explicit file as UTF-8, and delegates canonical text exclusively to
`flash_syntax::format_source`. Check mode turns the first scalar divergence into
an anchored `FMT001`; incomplete and invalid input retain shared syntax
diagnostics. Imports are ordinary retained syntax and are never traversed by
the formatter frontend.

Both operations inspect and prepare the complete ordered batch. Write mode
performs no replacement after any preflight failure. Its host adapter rejects
final symlinks and nonregular or canonically duplicate targets, rechecks source
bytes and permission bits, and replaces each changed file through a unique
same-directory temporary file using complete write, file synchronization, and
atomic rename. The adapter preserves permission bits only and deliberately
does not present ordered per-file replacement as a multi-file transaction.

## Execution-plan inspection frontend

`fsh plan [--] SOURCE` exposes the runtime's concrete planning boundary for one
top-level foreground command pipeline. The CLI frontend first uses the shared
module parser and static command analysis, then supplies a fresh lexical scope,
the inherited cwd and environment, default session options, the standard
command registry, and a read-only executable probe to the ordinary planner. It
runs structural preflight before rendering the plan.

The frontend's capability surface contains source canonicalization/loading and
executable metadata probing, but no runtime platform, session, editor,
configuration, history, writable filesystem, terminal, or process interface.
Command substitution and broader script shapes are rejected rather than
approximated. `ExecutionPlan::render` is the single deterministic rendering
owner and preserves source order, retained spans, and escaped native bytes; its
output is inspection text rather than source, serialization, or a launcher
protocol.

### Shared editor services

Syntax highlighting, completion context, formatter behavior, and multiline validation are built from Flash tokens, spans, parse outcomes, and command metadata.

These services may present different user interfaces on different targets, but they do not define alternative language semantics.

## Modules and static analysis

Multi-file programs are represented as a graph of canonically identified source modules. Module resolution records the importing source, the requested path, the canonical module identity, and the source spans required for diagnostics.

The shared syntax tree represents a static dependency as a top-level `import '<path>'` declaration. The path is a nonempty exact literal rather than an expression, so module discovery never depends on evaluation, interpolation, environment state, or globbing.

Canonicalization and source-byte loading are separate injected capabilities. The
recursive program analyzer assigns stable source identities in first-visit
depth-first order, decodes and parses each canonical module once, retains alias
imports as distinct graph edges, and registers both canonical module and source
identities for later diagnostics. It registers a module before traversing its
imports, allowing the graph to reject a back edge without unbounded recursion.
Report-oriented discovery retains decoded sources and continues through
independent sibling failures. Name analysis begins only with a complete graph,
and signature analysis begins only with clean names; poisoned invalid owners
suppress dependent cascades. A complete `ModuleProgram` is produced only when
every error-classified phase succeeds, so execution receives no partial graph
or registry.

Loading is analysis, not execution. The module program contains the canonical graph, source files, parsed syntax, deterministic local/export/import/reference tables, a resolved type registry, and direct/transitive initializer-effect summaries. Explicit `export { name }` lists can expose top-level lexical declarations and functions; `import { name } from '<path>'` resolves only names explicitly exported by the canonical target. Every loaded source is then resolved in evaluator-matched source order, including sources reached only through load-only imports. Each reference retains its complete source span and local declaration target; an imported reference also retains the import identifier and the target declaration/export provenance. The type registry retains source-spanned annotations and named-function signatures, including normalized attached documentation, for exact-span lookup. Unknown, private, duplicate, colliding, or invalid type names fail analysis with source-anchored diagnostics, and there is no wildcard import path.

The root module begins above an implicit immutable `args: List[String]` input
containing the ordered UTF-8 operands supplied after the script path. This
synthetic parent scope permits ordinary root source-order shadowing without
inventing a declaration span. Dependency modules do not receive the input.

Non-interactive `fsh <script>` execution injects the host filesystem adapter at
this boundary and reports grouped excerpts for diagnostics that span multiple
registered sources. The runtime derives deterministic source-edge depth-first
postorder from the analyzed named-import tables, initializes each canonical
dependency once, and executes the root last. A source reached only through
load-only imports remains dormant. The legacy execution loader selects the
first deterministic analysis error, while the checker frontend retains all
independent issues allowed by the phase barriers; both consume the same
successful program representation.

Each activated module executes through the existing session driver with an isolated lexical root. Completed exports are cloned into importer roots as immutable snapshots; private bindings never become ambient names. Working directory, child-process environment, status, output, process activity, and background jobs remain shared across the whole program. Runtime binding cells and callables receive the resolved annotations owned by the program. Named callables also retain their signature-derived inspection metadata and defining source; imported callables therefore keep both correct help ownership and correct cross-file body diagnostics. Initializers, assignments, callable arguments, and named-function results use exact value-family checks. Assignment-mutability analysis remains separate.

The session driver commits successful statement-local cwd, environment, and
status changes before the next statement. Normal completion and explicit
whole-program `exit` copy the final child environment to the caller; runtime or
required-output failure does not. Output, filesystem operations, and process
activity cross their boundaries immediately and are never rolled back. One
program-owned job coordinator joins background work on normal completion,
explicit exit, runtime failure, and output failure, retaining ordered
background evidence and its existing completion precedence.

Host-free effect analysis walks the same syntax and command manifest without
expansion, executable probing, or platform access. It records source-spanned
working-directory, child-environment, status, output, filesystem-read,
filesystem-write, process, job, program-exit, and opaque-external occurrences.
Known callable bodies fold into the caller summary; indirect calls and external
execution remain conservative. A transitive summary visits named dependencies
once in runtime initialization order and excludes load-only edges. The model is
descriptive and does not add a checker warning or a second execution policy.

The analysis layer is responsible for:

- resolving explicit imports and exports;
- constructing the module graph;
- rejecting import cycles with source-anchored diagnostics;
- resolving local, imported, exported, and private names;
- resolving source annotations and named-function signatures;
- checking known local and imported call arity and conservatively known types;
- retaining runtime contracts for dynamic binding and callable checks;
- classifying direct and named-dependency-folded initializer effects;
- collecting command metadata;
- validating typed pipeline connections;
- exposing the same results to the checker, help system, and language server.

Analysis must not depend on executing user code to discover names or signatures. It produces inspectable program information that execution can consume after validation.

## Shared tooling services

The formatter, `fsh check`, help output, interactive editor features, and language server use the same source model, parser, syntax tree, module graph, name resolution, function metadata, initializer-effect summaries, and diagnostic types.

No tooling frontend may maintain a second Flash grammar or a competing name resolver. A language change is implemented in the shared language services first and then exposed through the relevant CLI, editor, and protocol adapters.

Static pipeline checking also shares its source-independent carrier contracts and
fault classification with runtime preflight. It walks retained syntax without
expanding words: exact built-ins use the standard command registry, forced and
assumed externals are byte-stream stages, and dynamic heads remain unknown.
Executable availability and `PATH` contents are not analysis inputs.

`flash-cli` exposes checking through a read-only adapter whose entire capability
surface is canonicalization and finite regular-file source loading. It does not
construct a runtime session, environment, executable probe, platform, terminal,
configuration loader, or history store. Execution remains a separate stage.
Formatting, checking, help lookup, completion, navigation, and language-server
requests must not start external commands or mutate the active shell session
merely to obtain analysis results.

## Language-server protocol adapter

`flash-language-server` is a separate executable from `fsh`. It speaks JSON-RPC
2.0 over stdin and stdout using LSP `Content-Length` framing. Stdout is reserved
exclusively for framed protocol messages; the process does not inherit the
shell launcher, prompt, configuration, history, terminal, or process-reporting
paths. The first protocol surface is the stable LSP 3.17 core and remains
compatible with 3.18 clients through capability negotiation.

The implemented surface is:

| Area | Methods and notifications |
| --- | --- |
| Lifecycle | `initialize`, `initialized`, `shutdown`, `exit`, and `$/cancelRequest` |
| Synchronization | `textDocument/didOpen`, `textDocument/didChange`, and `textDocument/didClose` |
| Diagnostics | `textDocument/publishDiagnostics` server notifications |
| Discovery | `textDocument/completion`, `textDocument/hover`, and `textDocument/signatureHelp` |
| Navigation | `textDocument/definition` and `textDocument/references` |
| Formatting | `textDocument/formatting` |

Synchronization is full-text with open and close notifications. Incremental
edits are not advertised. The server negotiates UTF-8 positions when the client
offers them and otherwise uses the protocol-default UTF-16 encoding; checked
conversion remains owned by `flash-syntax`. Only absolute `file:` document URIs
are accepted. TCP, sockets, batches, dynamic registration, workspace or watched
file operations, custom methods, progress, partial results, rename, code
actions, semantic tokens, and execute-command requests are outside this
surface.

Each open document owns an exact client version and an overlay over the
read-only host loader. Unsaved roots and imported documents therefore enter the
same canonical module graph as disk sources. One canonical module has at most
one active overlay owner. Accepted open, full-change, close, or identity
transitions advance the workspace generation and conservatively invalidate all
open roots. Analysis jobs use immutable snapshots, merge diagnostics in
deterministic canonical-root order, attach current open-document versions, and
publish empty replacements for sources that leave the result set.

A reader, receive-order coordinator, sole writer, and one bounded worker keep
document changes and cancellation responsive while preserving deterministic
request order. Explicitly cancelled requests return `RequestCancelled`;
completed requests from an older generation return `ContentModified`; stale
diagnostics are discarded. Every request receives exactly one response.

Completion, hover, signature help, definition, references, and formatting are
projections of shared syntax, command metadata, and complete current module
programs. The adapter never initializes modules, evaluates expressions, expands
words, probes `PATH` or executables, opens redirections, mutates cwd or the
environment, accesses a terminal or runtime session, loads shell configuration
or history, or starts a process. Whole-document formatting delegates to the
shared canonical formatter and returns at most one edit.

## Runtime and session state

The [`flash-runtime`](../crates/flash-runtime/src/lib.rs) crate owns language evaluation and execution coordination.

A long-lived [`Session`](../crates/flash-runtime/src/session.rs) retains:

- the lexical `ScopeStack`;
- the logical working directory;
- the child-process environment;
- session options;
- the standard command registry;
- the most recent normally completed status;
- background-job state when job control is enabled.

Interactive submissions and complete script files both use this session driver. Script execution creates a session, submits the source, joins the jobs started by that script, and returns its structured completion or failure to the CLI.

### Runtime and CLI reporting boundary

`flash-runtime` owns language execution, cleanup, and outcome precedence. Its
script, module-program, isolated-chain, and interactive submission paths accept
an injected program-output sink. Script completion returns the optional final
`Status` plus ordered background-job reports; script failure returns the primary
structured error plus those reports. The runtime does not select host process
exit codes or write directly to process stdout or stderr.

`flash-cli` owns the executable boundary. Its host-free report layer maps a
classified completed `Status` to an exact eight-bit process code, writes program
bytes to stdout, and writes diagnostics and shell reports to stderr. Required
writes and flushes are checked. A failed program-output operation becomes
failure 1 and is reported on stderr when possible; a failed diagnostic stream
is not used recursively to report itself. Classification uses structured
outcomes and never parses rendered diagnostic text.

Normally completed codes `0..=255` remain exact. Numeric signals `1..=127` map
to `128 + signal`; an unrepresentable status becomes diagnosed failure 1 rather
than being wrapped or truncated. Launcher misuse remains status 2, and other
shell-owned failures use status 1. Codes 1 and 2 therefore remain distinguishable
ordinary program results when they arrive through a completed `Status`.

One shared runtime diagnostic builder retains the primary source and ordered
call frames for ordinary and multi-source module failures. The CLI renders that
structured diagnostic once; it does not add a second summary or infer the
failure kind from the stable `RUN001` heading.

### Lexical state and process state

Lexical bindings and child-process environment entries remain separate:

```text
ScopeStack
    └── Flash bindings, functions, and closures

Environment
    └── native values inherited by external processes
```

The runtime may copy an environment entry into a lexical seed for an isolated child-shell execution path, but ordinary lexical declarations are not automatically exported.

The logical working directory also belongs to the Flash session. External stages and platform-backed internal commands receive the session's directory explicitly rather than depending on process-global directory mutation throughout the runtime.

### Pure evaluation and command execution

Statements are divided into two broad paths:

- expressions, declarations, assignments, functions, and pure control flow use the evaluator;
- command jobs use resolution, planning, and one of the pipeline executors.

This separation keeps value operations and lexical scope behavior independent from descriptor and process mechanics.

Statements commit in source order. A failing statement reports its diagnostic without manufacturing a command status, while already completed earlier statements retain their established session effects.

### Runtime values and carriers

Structured values and pipeline carriers are related but distinct:

```text
Runtime value
    Null, Bool, Int, Float, String, Bytes, Path, Duration,
    ByteSize, List, Record, Table, Range, Status, Error, Function, Closure

Pipeline carrier
    Empty, ByteStream, Value, ValueStream
```

A `Bytes` value is finite data held as one value. A `ByteStream` is a lazy, single-consumer execution resource. Similarly, a `List` is not automatically treated as a `ValueStream`.

The concrete lazy stream implementations live in [`stream.rs`](../crates/flash-runtime/src/stream.rs). Pulling a stream yields an item or chunk, normal exhaustion, a source-spanned failure, or cancellation.

## Command resolution and execution planning

The command path is divided into registry lookup, external resolution, planning, and preflight.

```text
command syntax
    ↓
word and spread expansion
    ↓
internal-command registry lookup
    ↓
external executable resolution when required
    ↓
ExecutionPlan
    ↓
preflight
```

### Command registry

The [`CommandRegistry`](../crates/flash-runtime/src/command.rs) owns one
validated, deterministically ordered namespace manifest. Each spelling is a
canonical core command, an alias targeting exactly one core command, or a
reserved name protected from implicit external fallback. Core entries own
signatures; aliases borrow the canonical signature rather than copying it; and
reserved entries own no executable signature.

A signature declares:

- which carriers the command accepts;
- which carrier it returns, or whether it passes the input carrier through;
- command flags exposed to editor services;
- a stable invocation spelling and normalized user-facing documentation.

Invocable entries also carry their language-major introduction and optional
deprecation/replacement lifecycle metadata. Construction rejects duplicate or
empty names, invalid lifecycle data, missing or non-core alias targets, alias
chains, and invalid replacements. Runtime command execution does not use
manifest order as an override mechanism.

One classification operation returns unknown, core, alias, or reserved state.
Runtime resolution, planning, static analysis, background classification,
help, completion, and `which` consume that operation rather than maintaining
copied command lists or inferring namespace class from signature lookup. An
alias classification retains both the source spelling and canonical executor
identity, so every executor dispatches through the core implementation while
diagnostics and inspection preserve the spelling the program used.

A non-forced UTF-8 bare name is classified before external lookup. Core and
alias entries resolve internally; a reserved entry fails before `PATH`; and an
unknown name continues to external resolution. Forced-external and native
non-UTF-8 names retain the external path. Static checking uses the same
classification without probing host executables and gives a reserved stage an
unknown carrier contract to suppress dependent pipeline cascades.

### External resolution

External resolution produces a native executable path. It observes the session environment and working directory while preserving native path units.

The resolver does not return rendered shell source. Its result is either an internal command name or an external executable path.

### Execution plans

An [`ExecutionPlan`](../crates/flash-runtime/src/plan.rs) records:

- the working directory;
- the complete child environment;
- ordered stages;
- resolved internal or external stage identities;
- native external argument vectors;
- typed internal-command arguments;
- accepted and produced carriers;
- pipeline operators and their source spans;
- source-ordered redirections;
- snapshotted execution options;
- an optional immutable help selection for an inspection stage;
- process-group policy;
- the complete pipeline span.

Closures accepted by internal structured commands remain typed callable values in the plan. They are not converted into command-line strings.

Because plans are inspectable independently of execution, plan construction and plan validation can be tested without launching the planned processes.

### Preflight

Preflight validates the complete plan before an executor creates its first pipe or child process.

Validation includes relevant invariants such as:

- compatible carrier edges;
- representable external arguments and environment values;
- valid descriptor ownership;
- supported redirection combinations;
- stage forms accepted by the selected executor.

Platform capabilities are then checked by the operation that requires them. A plan can therefore be structurally valid while a particular target reports that the required operating-system capability is unavailable.

## Pipeline execution

The runtime selects among three execution shapes.

### All-internal pipelines

The [`internal`](../crates/flash-runtime/src/internal.rs) executor moves owned carriers directly between internal commands:

```text
InternalPayload::Empty
InternalPayload::ByteStream
InternalPayload::Value
InternalPayload::ValueStream
```

There is no intermediate terminal rendering or serialization.

Lazy commands such as filtering, mapping, line decoding, directory enumeration, and format conversion return pull-driven streams. A downstream stage that stops early can therefore stop requesting upstream items instead of forcing full materialization.

Commands that need session state, such as directory changes or explicit exit handling, receive a controlled session-state interface rather than unrestricted ownership of the complete session.

### All-external pipelines

The [`execute`](../crates/flash-runtime/src/execute.rs) module creates operating-system pipes, builds each child's final descriptor map, applies source-ordered redirections, and starts external stages directly through the platform interface.

All required stages are started before the executor waits for completion. This prevents a producer from filling a pipe while its consumer has not yet started.

Parent-owned pipe endpoints are released as soon as their ownership has been transferred or is no longer needed. Waiting produces source-ordered process results, which the runtime converts into leaf and aggregate `Status` values.

Command substitution and other capture paths drain output concurrently with child execution. When a capture limit is exceeded, the executor stops retaining further bytes but continues draining and reaping the pipeline before returning the error.

### Mixed pipelines

The mixed executor partitions a plan into maximal internal segments and
individual external stages. There is no topology limit on how many times they
alternate:

```text
external === byte pipe === [internal segment] === byte pipe === external
   === byte pipe === [internal segment] === byte pipe === external
```

Complete preflight validates every carrier edge and descriptor route before the
executor creates a pipe, opens a target, or starts a child. External-to-external
edges remain ordinary operating-system pipes. Every edge touching an internal
segment retains exactly the parent-owned endpoint that segment needs.

All external children start before internal draining begins. Nonterminal
internal segments drain on scoped workers; a final internal segment remains on
the session thread so its output sink does not acquire a cross-thread
requirement. Each segment creates, transforms, and destroys its own structured
payload. Only owned byte-descriptor endpoints and immutable plan data cross a
worker boundary, so lazy `ByteStream` and `ValueStream` pull closures remain
single-threaded.

A source-order sequencer admits internal stage preparation deterministically
while already prepared segments drain concurrently through bounded pipes.
Session built-ins update one pending state under that sequencer. Lazy closure
environments remain segment-local and merge into the pending environment in
segment source order only after the complete pipeline succeeds.

The coordinator owns one status slot per source stage. Worker completion order
cannot reorder the aggregate. A `check` immediately after an external stage
forwards bytes without waiting, then validates that exact predecessor after
child completion; an unsuccessful deferred check is the ordinary structured
runtime error and prevents state commit.

Success commits pending cwd, environment, closure deltas, and aggregate status
once, after every segment drain, final output, child wait, deferred assertion,
and status slot succeeds. Runtime, cancellation, output, or wait failure
discards that in-memory transaction. Bytes already displayed, files already
changed, and process effects remain observable because they are not
transactional.

A language `try` adds an enclosing checkpoint across lexical scope and the
host-owned in-memory session state. When a runtime error reaches it, the catch
block starts from that pre-try checkpoint with one fresh immutable `Error`
binding. Successful try or catch work follows the ordinary commit rules.
Cancellation, explicit exit, stopped-job control, and fatal host failures bypass
this boundary. The same external-effect limit applies: a catch cannot undo
bytes, files, or process effects that have already occurred.

One shared failure controller owns every external child until all segment
workers quiesce. A genuine segment or output failure closes endpoints,
cooperatively cancels peers, terminates and waits children, and selects the
earliest source-stage failure rather than a scheduler-race winner. `BrokenPipe`
at an internal-to-external boundary remains ordinary downstream early
completion. Explicit `exit` stops later preparation, cleans up children, and
commits only source-prior admitted state before returning its requested code.

Pipeline assignment still precedes local redirection. A byte-producing segment
tail either drains to its external successor or to its final resolved local
file route; an overridden pipe writer is dropped so the successor observes EOF.
Unsupported internal descriptor shapes fail preflight. A structured carrier
cannot cross an external boundary directly and still requires an explicit codec
or format command.

This architecture preserves:

- ordinary streaming for external programs;
- structured lazy evaluation between internal stages;
- bounded bridging at every repeated representation boundary;
- deterministic source-ordered preparation and aggregate status;
- transactional in-memory session state without false rollback claims for
  external effects; and
- exact child, endpoint, and terminal cleanup on every exit route.

### Terminal presentation

Human-readable presentation is selected only for a final structured carrier in an interactive session whose output is an unredirected terminal.

Terminal tables and other display forms are presentation, not serialization. Redirected output and external consumers require an explicit byte-producing stage.

## Platform capability boundary

The [`flash-platform`](../crates/flash-platform/src/lib.rs) crate defines the interface between portable runtime logic and operating-system operations.

The `Platform` trait is synchronous and blocking. Concurrency is arranged by the runtime with threads where required; Flash does not impose an asynchronous runtime on the CLI or target adapter.

### Capability groups

The interface separates capabilities such as:

- environment and working-directory access;
- file actions and directory enumeration;
- anonymous pipes;
- direct process spawning and waiting;
- process groups;
- signal delivery;
- foreground terminal ownership;
- terminal information and raw mode;
- monotonic time;
- standard directory discovery;
- discovery of the running shell executable;
- process hang-up disposition.

A platform exposes the set it supports. Each capability method verifies the relevant capability before touching the host.

### Error taxonomy

Platform failures remain distinguishable:

| Failure                | Meaning                                                              |
| ---------------------- | -------------------------------------------------------------------- |
| Unsupported capability | The adapter cannot provide the requested operation                   |
| Unavailable capability | The operation exists but cannot be used in the current session       |
| Operation failure      | A specific spawn, open, wait, signal, or descriptor operation failed |

This distinction allows the runtime to report a missing feature separately from a failed use of an available feature.

### Byte-preserving types

Executable paths, arguments, environment values, and filesystem paths cross the boundary through native `OsStr`, `OsString`, `Path`, and `PathBuf` representations.

Portable language code therefore does not need to convert all operating-system data through UTF-8. A conversion to text occurs only where a language operation explicitly requires text.

### Owned resources and guards

Platform resources use ownership rather than shared raw identifiers wherever possible:

- child-process handles own wait state;
- descriptor endpoints own pipes or files;
- directory streams own enumeration state;
- terminal-mode guards restore previous attributes;
- foreground-terminal guards return ownership to the shell;
- signal-arrangement guards restore prior dispositions.

Restoration is also performed when a guard is dropped, providing a cleanup boundary for early returns and propagated errors.

### Adapter roles

A concrete adapter implements the abstract platform capability contract for one operating-system environment.

[`flash-platform-posix`](../crates/flash-platform-posix/src/lib.rs) provides the Unix-like process, descriptor, filesystem, signal, and terminal routes used by the current executable. Its behavior on Linux or macOS is host evidence, not automatic FlashOS qualification.

[`flash-platform-flashos`](../crates/flash-platform-flashos/src/lib.rs) is the dedicated FlashOS adapter. It composes the 38 classified existing Rust and `relibc` routes behind the portable contract and owns the shimmed standard-directory policy: absolute native `HOME` and XDG roots are preserved, while missing or relative values receive deterministic FlashOS home, configuration, cache, and state fallbacks. These target details do not enter `flash-runtime`.

The Redox-target `fsh` dependency graph compiles the FlashOS adapter, but the executable does not select it yet. Its public capability set remains empty until later target-runtime qualification enables individual groups. This keeps implementation, selection, and behavioral support claims as separate reviewable steps.

The runtime depends only on the abstract capability contract. It must not silently emulate a missing target capability with weaker POSIX behavior. Release and target evidence determine which adapter capabilities may be claimed publicly.

### FlashOS target baseline

The machine-readable
[`flashos-x86_64.toml`](../platforms/flashos-x86_64.toml) record identifies the
platform boundary that a FlashOS adapter targets. The current baseline records:

| Boundary | Current identity |
| --- | --- |
| Rust target | `x86_64-unknown-redox` |
| Rust target configuration | `target_os="redox"`, `target_env="relibc"`, 64-bit little-endian ELF |
| Target compiler | Redox Rust branch selector `redox-2026-05-24`, reporting `rustc 1.98.0-dev`, unknown source commit, and LLVM 21.1.2 |
| C runtime | `relibc`, `libc.so.6`, with `/lib/ld64.so.1` as the executable interpreter |
| Flash executable | x86_64 position-independent ELF requiring `libc.so.6` and `libgcc_s.so.1` |

Candidate and release images cook their selected packages from tracked recipes.
The record binds the staged `relibc` package's source identity to the revision
selected by its recipe, preventing a moving binary feed from silently replacing
the configured userland input.
The compiler likewise reports its release and LLVM identity but no source
commit; the record preserves that limitation instead of turning the dated
branch selector into a revision claim.

Source validation keeps the record aligned with the image profile, source
package rule, build toolchain, Rust recipe, and `relibc` recipe. Image
qualification additionally checks Cargo's compiler fingerprint, the staged
`relibc` source identity, and the ELF headers of the staged `fsh` and C runtime.
These facts establish target identity only. They do not classify a platform
capability as supported or prove that a capability works at runtime.

The separate
[`flashos-x86_64-capability-evidence.toml`](../platforms/flashos-x86_64-capability-evidence.toml)
inventory compares every current `Capability` variant with the source path
selected by the Redox executable and with observations already made by the
FlashOS QEMU contract. It records requirements, source observations, runtime
observations, and explicit evidence gaps without turning them into support
classifications. In particular, the selected adapter's full-capability
declaration remains a claim under comparison; it is not accepted as target
qualification merely because the same code passes on Linux or macOS.

The paired
[`flashos-x86_64-operation-map.toml`](../platforms/flashos-x86_64-operation-map.toml)
then maps every requirement in that inventory to its current boundary. Some
operations remain entirely inside Flash, direct job-control and terminal calls
reach configured `relibc` ABI and Redox userland paths, and higher-level
environment, process, filesystem, directory, executable, and time calls stop at
public Rust standard-library APIs because the target compiler source commit is
unknown. Configuration-directory operations are recorded as currently
unrouted. These are mapping facts, not native, adapted, unsupported, or
kernel-work classifications, and they do not replace target behavior evidence.

The separate
[`flashos-x86_64-capability-classification.toml`](../platforms/flashos-x86_64-capability-classification.toml)
consumes the complete map and classifies the implementation route for every
operation and capability. Existing Flash-internal, target standard-library,
and configured-`relibc` routes are native. Standard-directory discovery,
native-path preservation, and fallback policy are shimmed because FlashOS must
define and wire an explicit target convention over existing filesystem
primitives. No current operation is deliberately unsupported or requires
kernel work. All target-runtime qualification remains pending; an architectural
route verdict is not a behavioral support claim. The dedicated adapter now
implements those routes without enabling any capability or changing the
executable's selected adapter.

### Test adapters

The platform crate also provides deterministic fake and recording adapters.

They allow tests to:

- select supported and unsupported capabilities;
- script process results;
- inspect spawn requests;
- observe process-group placement;
- record signal delivery and terminal handovers;
- exercise cleanup behavior without using the host's real process table or terminal.

This keeps most runtime verification independent from the POSIX adapter and reserves host or pseudoterminal tests for behavior that genuinely crosses the platform boundary.

## Interactive front end

The [`flash-cli`](../crates/flash-cli/src/lib.rs) crate combines the runtime and selected platform adapter into the `fsh` executable.

Its top-level modes are:

```text
help
version
static checking
canonical formatting
script execution
interactive session
reserved internal child-shell execution
```

The reserved child-shell path supports background conditional chains and is not part of the public command-line interface.

### Editor boundary

The interactive loop depends on the synchronous [`LineEditor`](../crates/flash-cli/src/editor.rs) boundary rather than on a specific terminal-editing library.

An editor produces events such as:

- submitted source;
- cancellation of the current edit;
- end of input.

The interactive loop owns the sequencing of prompts, notices, diagnostics,
evaluation, and exit decisions. Evaluation receives an injected program-output
sink, while the driver owns checked output and diagnostic flushes plus the final
host result. Recoverable diagnostics return to the same session. Fatal editor,
output, diagnostic, or platform failure performs unconditional session cleanup
before returning status 1; if the diagnostic stream itself failed, the driver
does not attempt another write through it. The editor owns terminal writes that
must appear safely before the next prompt.

### Host and target editors

macOS and Linux builds use the Reedline-backed adapter for parser-driven multiline validation, highlighting, completion, history, and hints.

The Redox path uses the Flash terminal editor when both input and output are terminals. A canonical line reader remains available as a fallback when raw terminal editing is unavailable or output is redirected.

These adapters share the same session evaluator, syntax implementation, and
editor-neutral completion, highlighting, hint, history, external-print, and
resize contracts. The portable editor implements Tab completion, parser-owned
styles, history hints, persistent recall, grapheme-aware editing, display-cell
rendering, whole-submission multiline movement, in-flight resize, and safe
redraw around background notices. This source parity is host-tested; FlashOS
runtime qualification remains required before the behavior is claimed for an
assembled target image. Differences in editor facilities do not create
different script semantics.

### Configuration and history

Configuration and history selection occur before the interactive
`Session` is created. A successfully initialized configuration seeds the
session scope and environment plus typed `pipefail` and capture-limit options
and completion/history policy. Six config-only mutable bindings carry those
settings plus primary and continuation prompt strings through the isolated
transaction and are removed before the live lexical scope is installed. Safe
mode restores clean settings and its fixed `[SAFE] >> ` prompt; config bypass
retains ordinary defaults. The CLI `--no-history` policy remains an
unconditional override.

At each prompt boundary the interactive evaluator snapshots the live registry
and lexical scope, then collects executable names from the child `PATH` and
recursive path candidates from the logical cwd. The cancellable collector is
outside the keypress callback and is bounded to 256 directories and 4,096
entries per host candidate family. It does not follow directory symlinks and
omits native paths that have no exact UTF-8 source representation. Crossing a
ceiling discards that family for the prompt, preserving deterministic results
instead of retaining a host-ordered prefix. Each completed snapshot has a
monotonic generation, and the editor rejects a stale generation before it can
replace the current immutable engine.

Path replacement remains syntax-owned. The ordinary lossless token stream
provides quote mode, interpolation boundary, grammar role, and exact UTF-8 byte
span. Semantic matching consumes the same wildcard component rules as the
explicit `glob(...)` runtime, while rendering produces reversible bare or
quoted Flash source. Completion therefore observes filesystem candidates
without expanding an argument or evaluating the edit buffer.

The host and Redox executables select their native configuration and state-path
conventions at compile time. The Redox source path now wires transactional
configuration, completion policy, portable persistent history, prompts, and
both opt-out flags into the portable editor. Development-host and forced-editor
tests establish source parity, not availability inside a FlashOS image; target
runtime qualification remains separate.

### Prompt-safe notices

Background-job notices are transferred from the runtime as structured records
with stable identities. An active editor clears its presentation, writes the
complete notice, and redraws the same prompt, buffer, cursor, and completion
state. Notices already pending at a prompt boundary use the same editor-owned
write path. The runtime acknowledges a notice only after successful
presentation.

This prevents asynchronous output from corrupting an active edit or losing a
completion observed while the editor owns the terminal.

## Jobs and process lifecycle

Job management is divided between execution mechanics and a session-owned coordinator.

### Foreground pipelines

External members of one foreground pipeline are placed in one process group when the platform supports that capability. An interactive shell may hand the terminal to that group for the duration of the foreground wait.

Terminal ownership is held by a guard and restored before the next prompt. A failed restoration is reported rather than silently continuing with a shell that may no longer receive terminal input.

### Background jobs

A background job receives a stable Flash job identity that is separate from operating-system process and process-group identifiers.

The coordinator owns:

- job identities;
- member process handles;
- process-group metadata;
- running, stopped, continued, and completed observations;
- aggregate statuses;
- addressable job state;
- prompt-safe notices;
- cleanup and reaping state.

Observer tasks report process transitions to the coordinator. The session remains the sole owner of the job table and applies those observations in a defined order.

### Direct and supervised background execution

A background chain that consists of one all-external pipeline can use the direct pipeline launcher.

More general background conditional chains use a re-executed `fsh` child as their supervisor:

```text
interactive or script session
    ↓ starts one child shell
background supervisor process
    ↓ parses and executes the conditional chain
internal and external child work
```

The supervisor is the addressable job member. It plans the chain in the child process, reports the chain's aggregate status, and remains alive long enough to reap external descendants during shutdown.

The internal invocation is reserved so that this mechanism does not create a public command-string execution mode.

### Lifetime policy

A script joins the background jobs it started before returning to its caller. Background failures can therefore affect the final script result rather than being silently orphaned.

An interactive session warns before leaving live jobs. A confirmed exit resumes stopped jobs where necessary, sends the session's hang-up request, and waits for completion.

Destructive termination is an explicit job operation rather than an automatic timeout escalation.

## Diagnostics and failure containment

Flash keeps several outcome classes separate:

```text
parse outcome
runtime value or control completion
normal command Status
runtime error
cancellation
platform or output failure
```

A nonzero external exit is a normal `Status`, not a runtime error. Operators such as `&&` and `||` branch on Boolean or status success, while `check` explicitly converts an unsuccessful status into an evaluation error.

### Source anchoring

Errors retain the most relevant source span available:

- invalid tokens and grammar failures point into the source front end;
- expansion failures point to the affected word or spread;
- command-resolution failures point to the command head;
- incompatible carrier errors point to the pipeline boundary;
- redirection failures point to the relevant operator or target;
- process failures point to the planned command;
- lazy structured errors point to the stage that owns the producer.

Runtime stack frames can add callable context without discarding the original span.

### Failure precedence and cleanup

When execution has already acquired resources, cleanup still occurs before the error is returned.

Depending on the execution path, cleanup may include:

- closing parent descriptor owners;
- terminating and reaping already-started children;
- draining capture pipes;
- waiting for remaining pipeline stages;
- restoring foreground terminal ownership;
- restoring terminal attributes or signal dispositions;
- joining or hanging up session-owned background jobs.

Cleanup failures do not automatically replace the more informative originating failure. The relevant executor defines which outcome remains primary.

### No false transactional claim

Preflight prevents many failures before execution begins, but process and filesystem execution is not a transaction.

For example, a source-ordered redirection may successfully create or truncate one file before a later file action fails. Flash cleans up owned resources, but it does not claim to reverse completed operating-system side effects.

Mixed execution does use one pending in-memory session transaction. Cwd,
environment, closure deltas, and aggregate status become caller-visible only
after final output, child waits, deferred checks, and status construction all
succeed. That narrower transaction does not include bytes already written,
filesystem changes, or process activity.

## Safety and portability boundaries

Flash is implemented in Rust, but it is not accurate to describe the entire component as containing no unsafe code.

The crates containing language semantics and frontend orchestration prohibit unsafe code:

- `flash-syntax`;
- `flash-runtime`;
- `flash-platform`;
- `flash-cli`.

The POSIX adapter denies unsafe code by default and permits it only in explicitly scoped implementation areas that require low-level system interfaces, including descriptor installation, process-group operations, terminal control, signal disposition, and child-status observation.

This arrangement creates a review boundary; it does not by itself establish the absence of defects.

Additional portability boundaries include:

- source text is UTF-8, while native paths and arguments may not be;
- the runtime does not assume every platform capability exists;
- terminal behavior is enabled only when the relevant streams are terminals;
- process-group and signal behavior remains adapter-owned;
- human display is never treated as a wire protocol;
- successful host execution does not qualify the FlashOS target;
- target compilation does not replace image-level execution evidence.

## FlashOS integration

Flash is integrated into the FlashOS system through the package recipe at [`recipes/terminal/flash/recipe.toml`](../../../recipes/terminal/flash/recipe.toml).

The recipe selects the `flash-cli` package, builds the `fsh` binary for the active target, and installs it into the image package. The active x86_64 product profiles include the package and configure `/usr/bin/fsh` as the login shell.

```text
Flash workspace
    ↓
flash-cli package
    ↓
fsh target binary
    ↓
Flash package recipe
    ↓
FlashOS image
    ↓
login starts /usr/bin/fsh
```

The package recipe snapshots tracked and non-ignored files from the current
`components/flash/` workspace. A clean image build therefore uses the Flash
tree from the exact outer FlashOS checkout without a self-referential recipe
SHA. Local uncommitted component files are included only when they are not
ignored, allowing pre-commit image testing while excluding generated targets.

System-level package selection, image assembly, boot flow, and login configuration remain documented in [FlashOS Architecture](../../../docs/architecture.md). This document owns the internal architecture of the Flash component after its executable starts.

## Sources of truth

Use the following files when evaluating or changing an architectural contract:

| Concern                                               | Primary source                                                                           |
| ----------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Workspace membership and shared metadata              | [`Cargo.toml`](../Cargo.toml)                                                            |
| Source, syntax trees, classification, and diagnostics | [`flash-syntax/src/lib.rs`](../crates/flash-syntax/src/lib.rs)                 |
| Runtime module boundaries                             | [`flash-runtime/src/lib.rs`](../crates/flash-runtime/src/lib.rs)               |
| Session ownership and submitted-source flow           | [`session.rs`](../crates/flash-runtime/src/session.rs)                              |
| Script execution and background joining               | [`script.rs`](../crates/flash-runtime/src/script.rs)                                |
| Command signatures and registry                       | [`command.rs`](../crates/flash-runtime/src/command.rs)                              |
| Command planning and preflight                        | [`plan.rs`](../crates/flash-runtime/src/plan.rs)                                    |
| External and mixed execution                          | [`execute.rs`](../crates/flash-runtime/src/execute.rs)                              |
| Internal structured execution                         | [`internal.rs`](../crates/flash-runtime/src/internal.rs)                            |
| Lazy byte and value streams                           | [`stream.rs`](../crates/flash-runtime/src/stream.rs)                                |
| Background-job coordination                           | [`background.rs`](../crates/flash-runtime/src/background.rs)                        |
| Job identities and states                             | [`job.rs`](../crates/flash-runtime/src/job.rs)                                      |
| Platform capabilities and test adapters               | [`flash-platform/src/lib.rs`](../crates/flash-platform/src/lib.rs)                     |
| Concrete Unix-like platform operations                | [`flash-platform-posix/src/lib.rs`](../crates/flash-platform-posix/src/lib.rs)         |
| FlashOS adapter and standard-directory policy         | [`flash-platform-flashos/src/lib.rs`](../crates/flash-platform-flashos/src/lib.rs)     |
| CLI assembly and target selection                     | [`flash-cli/src/main.rs`](../crates/flash-cli/src/main.rs)                             |
| Interactive editor contract                           | [`editor.rs`](../crates/flash-cli/src/editor.rs)                                       |
| Interactive control loop                              | [`interactive.rs`](../crates/flash-cli/src/interactive.rs)                             |
| FlashOS package construction                          | [`recipe.toml`](../../../recipes/terminal/flash/recipe.toml)                           |
| FlashOS product integration                           | [FlashOS Architecture](../../../docs/architecture.md)                                    |

When descriptive documentation and executable behavior disagree, inspect the current source, manifests, tests, package recipe, and target evidence before changing the public architectural claim.

## Related documentation

- [Language Guide](language-guide.md) — Language syntax, values, bindings, expressions, commands, and structured-data semantics.
- [Scripting](scripting.md) — Script execution, external processes, redirections, statuses, and job control.
- [Development](development.md) — Workspace builds, tests, linting, fuzzing, fixtures, and local API documentation.
- [Flash overview](../README.md) — Component purpose, public boundaries, and documentation entry point.
- [FlashOS Architecture](../../../docs/architecture.md) — System layers, image construction, package integration, and boot-to-shell flow.
- [FlashOS Verification](../../../docs/verification.md) — Evidence boundaries between host checks, target builds, images, QEMU, and hardware.

---

[← Previous: Scripting](scripting.md) · [Flash documentation](README.md) · [Next: Development →](development.md)
