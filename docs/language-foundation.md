# OPAAL language foundation

OPAAL `1.0.0-alpha.1` defines source, module, type, action, project-authoring,
outcome, stream, tooling, and resource semantics. A separate embedding contract
defines explicit authority and adapter-owned lifetimes. Project checks validate
declarations and grants without enabling adapters or controlled execution.

## Source and modules

Every file module is a UTF-8 `.opaal` file. Source begins directly with its
documentation, imports, declarations, or expressions; there is no version
directive or source-selection option.

Local modules use explicit quoted `.opaal` paths and required aliases. Standard
modules use compiled `std::name` identities:

```opaal
import './model.opaal' as model
import std::value as value
export { model }
```

Aliases and re-exports retain one canonical module identity. Ambient preludes,
wildcard imports, filesystem search paths, and package discovery are absent.

An explicitly selected `opaal.toml` additionally permits imports from the
closed `project::context`, `project::tools`, `project::endpoints`, and
`project::secrets` namespaces. These imports are declarative identities, not
ordinary values or a route to ambient configuration. See
[Actions and explicit projects](actions-and-projects.md).

## Types, patterns, operations, and streams

Nominal records and variants are immutable. Construction supplies every field
exactly once, and matching uses the same qualified nominal identity. Functions
and closures support invariant generic parameters, explicit result annotations,
and the closed `Equal` and `Ordered` constraints.

Declaration, parameter, and `match` positions share record, variant, list, and
list-rest patterns. Closed unguarded matches are checked for exhaustiveness and
unreachable arms.

Compiled operations have one descriptor shared by expression calls, eligible
pipeline lowering, help, checking, completion, hover, and signature help.
Value streams are lazy, typed, single-consumer resources with deterministic
item, byte, terminal-state, cancellation, and cleanup behavior.

## Outcomes and authority refusal

Structured execution distinguishes completed values or carriers, catchable
language errors, cooperative cancellation, classified refusal, and fatal host
or reporting failure. Completed stages, partial effects, and cleanup failures
remain ordered secondary evidence.

Actions may declare the closed static effect taxonomy. Analysis rejects
undeclared callee effects, cycles, action values, function-to-action calls, and
invalid project scopes. Invoking an action with a declared effect is refused
before platform access. Other known filesystem, process, environment, network,
terminal, random, substitution, redirection, and background routes retain
their existing refusal boundary.
`opaal plan` returns `PLAN004` after reading the explicit root but before
capturing ambient launcher or executable state.

Embedders may construct the documented
[authority and resource context](authority-and-resources.md). Its presence does
not alter parsing, analysis, pure evaluation, planning refusal, or any source
host-access boundary.

## Tool and resource agreement

The formatter, checker, runtime, interactive client, embedding API, and language
server share parsed sources, module graphs, type identities, operation
descriptors, semantic queries, and resource limits. Observer surfaces do not
execute modules to discover semantic data.

Analysis bounds source bytes, modules, depth, syntax nodes, type depth, generic
instantiations, overload candidates, diagnostics, and work units. Evaluation
bounds steps, calls, retained collection items, and retained bytes. Exact limits
succeed; the first excess returns structured failure without a partial program.

[← Documentation index](README.md) · [Actions and explicit projects](actions-and-projects.md) · [Architecture](architecture.md) · [Development](development.md)
