# Flash 2 Language Foundation

[FlashOS](../../../README.md) › [Flash](../README.md) › [Documentation](README.md) › Flash 2 foundation

Flash 2 is implemented in the current development tree as an explicitly
versioned, pure language and operation foundation. It is not a released Flash
or FlashOS version. Flash 1.0.0 remains the stable compatibility baseline, and
Flash 2 source cannot silently fall back to Flash 1 execution.

This page defines the implemented foundation boundary. It covers source
versioning, qualified modules and operations, nominal types, generic callables,
patterns, structured outcomes, typed streams, resource ceilings, editor and CLI
observers, and the read-only Flash 1 source migration analyzer. It does not
claim effects, authority grants, projects, actions, tasks, controlled workflow
planning, packages, or FlashOS image qualification.

## Selecting the language

Every Flash 2 source module starts with `language 2` as its first non-trivia
statement:

```fsh
language 2

let answer = 42
```

The directive is required in the root and every imported source. Missing,
late, duplicate, malformed, unsupported, and mixed-generation directives fail
before semantic analysis. Each file therefore retains its own grammar identity
when formatted or opened independently by an editor.

The Flash 2 interactive session preselects generation 2. Interactive cells do
not repeat the directive, and the session does not discover Flash 1 startup
configuration, history, executable paths, or working-directory candidates.
There is no runtime language switch.

## Modules, types, and patterns

Flash 2 adds explicit qualified module aliases:

```fsh
language 2

import std::value as value
import './model.fsh' as model
export { model }

value::length([1, 2, 3])
```

Local imports use an explicit quoted path and alias. Standard imports use a
compiled `std::name` identity. An alias re-export preserves the original
module identity; it does not clone it. Wildcard imports, ambient preludes,
filesystem module search, and package discovery are absent.

Nominal records and variants are immutable values:

```fsh
language 2

type Box[T] = { value: T }

enum Selection[T] {
    Selected(T),
    Empty,
}
```

Record construction requires every declared field exactly once. Variant
construction and matching use the same qualified nominal identity. Flash 2
does not add untagged structural unions or implicit conversion between nominal
records, structural records, variants, strings, integers, statuses, or errors.

Named functions and closures support invariant generic parameters, explicit
result annotations, and the closed `Equal` and `Ordered` constraint set.
Inference performs exact bounded unification; ambiguous parameters require
explicit bracketed type arguments. A failed inference never inserts `Any`.

Declaration, parameter, and `match` positions share record, variant, list, and
list-rest patterns. A declaration or parameter mismatch is a source-spanned
language error. A `match` mismatch selects the next arm. Closed unguarded
matches are checked for exhaustiveness and unreachable arms.

## Qualified operations and streams

The compiled `std::value::length` operation is the reference operation. Its one
stable identity, overload set, help, type information, and implementation are
shared by expression calls and eligible pipeline lowering:

```fsh
language 2

import std::value as value

value::length(["one", "two"])
```

An eligible pipeline stage may omit exactly the descriptor’s first input.
Lowering does not add a method table, map a scalar operation across a stream,
materialize a stream, or insert the carrier into another argument. A local
`def` remains an ordinary callable rather than becoming a source-defined
operation.

Value streams retain an opaque runtime owner, element type, and cardinality.
They are lazy, single-consumer resources and are not storable or serializable
values. Checked pulls latch the first terminal result. Item and byte ceilings,
delivered prefixes, cancellation, producer failures, contract violations, and
cleanup evidence remain explicit.

## Outcomes and host authority

The structured runtime exposes exactly one primary outcome:

- completed value/carrier with an optional real `Status`;
- catchable language `Error`;
- cooperative cancellation;
- refusal with `denied`, `unsupported`, or `unknown` reason; or
- fatal host/report/session failure.

Completed stages, partial external effects, and cleanup failures are ordered
evidence beside the primary. Cleanup failure never replaces another primary;
when cleanup is the sole failure, its first error becomes the primary resource
error. `Result[T,E]` and `Option[T]` from `std::outcome` remain ordinary nominal
values and never become control outcomes.

Pure Flash 2 execution receives only its explicit source inputs and
deterministic resource budget. Known filesystem, environment, process, network,
terminal, secret, clock, random, substitution, redirection, and background
routes are rejected by analysis. If an operational route is reached only
dynamically, execution returns a structured refusal before executable probing,
platform access, or spawning.

## Downstream metadata seams

The foundation reserves typed metadata attachment points without implementing
their future owners. Named-function signatures and compiled operation
descriptors carry `DownstreamCallMetadata`; structured execution outcomes carry
`DownstreamOutcomeMetadata`. Every current slot is `Absent`. A caller may also
represent `Unknown`, but cannot construct an identity, authority decision,
deadline, or cleanup claim.

The opaque slot types are:

| Execution and resource metadata | Project and action metadata |
| --- | --- |
| `EvaluationContextId` | `ActionId` |
| `EffectSet` | `ProjectId` |
| `CapabilityRequest` | `TaskId` |
| `AuthorityVerdict` | `ToolId` |
| `ResourceOwnerId` | `EnvironmentId` |
| `CancellationScopeId` | `DeclaredInputs` |
| `Deadline` | `DeclaredOutputs` |
| `CleanupOutcome` | |

These records have no serialization contract and provide no discovery,
execution, grant, or mutation API. `OperationPurity::Pure` classifies the
current standard operation. `RequiresAuthorityContract` is reserved for an
operation that must refuse until a separately implemented authority contract
exists.

Future effects and resources must fill these slots without changing callable,
operation, type, carrier, or outcome meaning. Future actions must refine the
same callable contract, and future tasks must identify explicitly exported
project actions rather than introduce command strings or another task
language.

## Tooling workflow

The formatter, checker, help, interactive queries, and language server consume
the same parsed sources, module graph, nominal identities, signatures,
operation descriptors, and resource limits. They do not execute modules to
discover semantic data.

A successful non-interactive pure script retains its final value in the
structured API and writes nothing implicitly to stdout. The interactive shell
may present that value. The complete checked-in workflow demonstrates the
same `Int(2)` through migration, formatting, checking, execution, help, and
editor queries under the
[Flash 2 foundation workflow fixtures](../tests/v2-foundation/workflow/README.md).

### Planning

`fsh plan` remains the exact low-level Flash 1 command-pipeline inspector.
Flash 2 has no authority model or complete controlled-planning contract yet,
so the same command on a `language 2` root returns `PLAN004` and a structured
`unsupported` refusal. Refusal occurs after reading the explicit root only and
before the planner captures the launcher cwd, inherited environment, `PATH`, or
executable metadata. Resolving an explicitly supplied relative root still uses
the launcher's working directory to locate that source input. The refusal does
not publish a partial operation, script, action, or workflow plan.

## Read-only source migration

Use the separate analyzer to classify explicit Flash 1 roots and their static
import closure:

```sh
fsh-migrate-v1-v2 --format human root.fsh
fsh-migrate-v1-v2 --format json -- root.fsh
```

The analyzer never writes or executes source, walks a directory, reads project
state, loads configuration/history, probes executables, or applies edits. It
reports deterministic first-visit source order, stable codes, half-open byte
spans, optional non-overlapping UTF-8 edits, lossless percent-encoded source
URIs, and `sha256:` source digests. A consumer must reanalyze changed source
rather than apply an edit against a mismatched digest.

Status 0 means the report has no required or unresolved work. Status 1 means a
required edit, unresolved migration, read/UTF-8/parse failure, or bounded
incomplete report. Invocation misuse is status 2. Source migration is complete
for this foundation; project, package, lockfile, tool, and environment
migration remain unavailable.

## Resource limits

Analysis deterministically bounds source bytes, module count and depth, syntax
nodes, type depth, generic instantiations, overload candidates, diagnostics,
and work units. Execution bounds steps, nested calls, retained collection
items, and retained collection/string bytes. Migration separately bounds files,
source bytes, findings, edits, output bytes, nesting, and work units.

The exact configured boundary succeeds. The first excess returns a structured
resource result without a partial executable program or a truncated successful
report. Cancellation remains distinct. Wall-clock measurements belong only to
the benchmark harness and are not language deadlines.

## Compatibility and release boundary

Flash 1 source continues to use the frozen Flash 1 parser, runtime, planner,
configuration, history, and host contract. Flash 2 execution does not link a
Flash 1 compatibility mode; only the isolated migration analyzer reads Flash 1
source for classification.

Host tests on macOS or Linux establish source and CLI behavior on that host.
They do not establish that an assembled FlashOS image contains or can execute
the Flash 2 foundation. Before any FlashOS integration claim, the system owner
must complete the checklist in [Architecture: FlashOS integration](architecture.md#flashos-integration).

---

[← Documentation index](README.md) · [Architecture](architecture.md) · [Development](development.md)
