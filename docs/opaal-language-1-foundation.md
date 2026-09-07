# OPAAL language 1 foundation

OPAAL language 1 is the current, unreleased foundation implemented by version
`1.0.0-alpha.1`. It defines pure source, module, type, operation, outcome,
stream, tooling, and resource semantics. It does not define effects, authority
grants, projects, actions, tasks, packages, or controlled workflows.

## Source identity

Every file module uses `.opaal` and begins with `language 1` as its first
non-trivia statement. The root and every local import declare their own
identity. Missing, late, duplicate, malformed, mixed, and `language 2`
directives fail before semantic analysis.

Interactive cells preselect OPAAL language 1 and do not include a directive.
There is no language-selection option or legacy startup mode. An unversioned or
`.fsh` source is rejected by execution, checking, formatting, planning, and LSP
entry points.

## Modules, types, and patterns

Local modules use explicit quoted `.opaal` paths and required aliases. Standard
modules use compiled `std::name` identities:

```opaal
language 1

import './model.opaal' as model
import std::value as value
export { model }
```

Aliases and re-exports retain one canonical module identity. Ambient preludes,
wildcard imports, filesystem search paths, and package discovery are absent.

Nominal records and variants are immutable. Construction supplies every field
exactly once and matching uses the same qualified nominal identity. Named
functions and closures support invariant generic parameters, explicit result
annotations, and the closed `Equal` and `Ordered` constraints. Inference is
bounded and exact; ambiguity requires explicit type arguments and never inserts
`Any` silently.

Declaration, parameter, and `match` positions share record, variant, list, and
list-rest patterns. A declaration or parameter mismatch is a language error. A
`match` mismatch selects the next arm, while closed unguarded matches are
checked for exhaustiveness and unreachable arms.

## Operations and streams

`std::value::length` is the reference compiled operation. Expression calls,
eligible pipeline lowering, help, formatting, checking, completion, hover, and
signature help share one descriptor and canonical identity. Pipeline lowering
may supply only the descriptor's omitted first input; it does not create a
method table, map scalars across streams, materialize streams, or turn local
functions into operations.

Value streams retain an opaque owner, element type, and cardinality. They are
lazy, single-consumer resources, not storable or serializable values. Checked
pulls latch the first terminal state. Item and byte ceilings, delivered
prefixes, producer failures, contract violations, cancellation, and cleanup
evidence remain distinct.

## Outcomes and authority refusal

Structured execution has one primary outcome:

- a completed value or carrier with an optional real status;
- a catchable language error;
- cooperative cancellation;
- a refusal classified as denied, unsupported, or unknown; or
- a fatal host, report, or session failure.

Completed stages, partial effects, and cleanup failures are ordered evidence
beside that primary. A cleanup failure never replaces another primary; if it is
the sole failure, its first resource error becomes primary. Language-level
`Result[T,E]` and `Option[T]` values never become control outcomes.

Current evaluation receives explicit source and deterministic resource budgets
only. Known filesystem, process, environment, network, terminal, secret,
clock, random, substitution, redirection, and background routes are rejected
during analysis. A dynamically reached route returns a structured refusal
before executable probing, platform access, or spawning.

`opaal plan` therefore returns the `PLAN004` unsupported refusal after reading
the explicit root but before capturing launcher cwd, inherited environment,
`PATH`, or executable metadata. It publishes no partial operation, action,
script, or workflow plan.

## Tool agreement

The formatter, checker, runtime, help, interactive queries, and language server
consume the same parsed sources, canonical module graph, nominal identities,
function signatures, operation descriptors, and resource limits. Observer
surfaces do not execute modules to discover semantic data.

A successful non-interactive pure script writes nothing implicitly to stdout
and retains its final value in the structured API. Interactive presentation may
display that value. The checked-in workflow under
[`tests/opaal-foundation/workflow/`](../tests/opaal-foundation/workflow/) proves
the same operation identity across formatting, checking, execution, help, and
editor queries.

## Migration boundary

`opaal-migrate-flash-v1` is a separate read-only analyzer. It accepts explicit
Flash 1 `.fsh` roots and their explicit static import closure, and emits only
migration schema 2. It never writes or executes source, walks a directory,
discovers project state, loads configuration/history, probes executables, or
applies edits.

The analyzer retains deterministic traversal, source SHA-256 values, lossless
URIs, half-open byte spans, optional non-overlapping edits, and exact exit
classes. Status 0 is a complete report with no required or unresolved finding;
status 1 is a complete report with required or unresolved work; status 2 is
invocation misuse or a source, limit, or rendering failure that prevented a
complete report.

Flash parsing is behind the default-off `opaal-syntax/flash-v1-migration`
feature. Only `opaal-migrate` enables it in the workspace. It is not an OPAAL
execution personality or compatibility switch.

## Resource limits and evidence boundary

Analysis bounds source bytes, modules, depth, syntax nodes, type depth, generic
instantiations, overload candidates, diagnostics, and work units. Evaluation
bounds steps, calls, retained collection items, and retained bytes. Migration
separately bounds files, source bytes, findings, edit bytes, output bytes,
nesting, and work units. The exact boundary succeeds; the first excess returns
a structured resource result without a partial executable program or truncated
successful report.

Host validation proves the exercised macOS/Linux source and runtime surfaces
only. It does not establish packaging by another operating system, Redox
support, image inclusion, or physical-hardware qualification.

[← Documentation index](README.md) · [Architecture](architecture.md) · [Development](development.md)
