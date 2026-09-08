# OPAAL architecture

OPAAL separates source semantics, pure analysis and evaluation, host contracts,
frontends, and observation. Dependencies point inward toward semantic owners;
no frontend defines a second source personality.

## Crate graph

```text
opaal-syntax ── opaal-runtime ── opaal-cli
                      └───────── opaal-lsp

opaal-platform ── opaal-runtime
       └────────── opaal-platform-posix ── opaal-cli
```

- `opaal-syntax` owns source bytes, spans, tokens, syntax trees, parsing,
  formatting primitives, completion context, and diagnostics.
- `opaal-runtime` owns module analysis, types, operations, values, streams,
  outcomes, semantic queries, typed actions and projects, deterministic
  budgets, and pure evaluation.
- `opaal-cli` owns invocation parsing, file inspection, reporting, interactive
  presentation, explicit project inspection/checking, and the current planning
  refusal.
- `opaal-lsp` owns stdio framing, JSON-RPC lifecycle, document snapshots,
  cancellation, and projection of shared semantic queries.
- `opaal-platform` defines host capability traits and byte-preserving native
  contracts. `opaal-platform-posix` implements the macOS/Linux adapter and test
  observers.

The Cargo workspace contains exactly these six crates. The unpublished
`opaal-fuzz` package is separate so nightly instrumentation cannot change the
normal locked graph.

## Source and module flow

File frontends enforce `.opaal`, UTF-8, regular-file, and resource boundaries.
Parsing retains source-local spans. Module loading canonicalizes only explicit
local imports, applies source/module/depth/work ceilings, and builds one graph.
Standard modules are compiled identities rather than filesystem search results.

Static analysis resolves aliases, exports, nominal types, generics, patterns,
operations, effects, and semantic-query indexes over that graph. The formatter
consumes syntax rather than rebuilding semantics. The language server snapshots
documents and discards stale results by document generation.

## Pure execution boundary

OPAAL evaluates an analyzed module program with `EvaluationPolicy::PureOpaal`.
Static effects are legal only in typed action declarations and are checked as
closed request sets. Effectful action invocation and dynamically reached
effects are refused before platform or executable access. Successful pure
non-interactive execution retains a value and emits no implicit bytes.

Platform contracts and host observers remain available to test refusal,
terminal presentation, and adapters; their presence grants no source authority.
Outcome composition keeps values, statuses, language errors, cancellation,
refusal, fatal failure, partial evidence, and cleanup evidence distinct.

## Operational embedding boundary

`opaal-runtime` owns the explicit authority and resource context plus the
maintained bounded data/path/file/time/version/integrity/URL/HTTP/process
module APIs. Exact typed
requests are matched only against external grant or deny rows; `opaal-platform`
adapters report enforcement without granting permission. The context also owns
sticky cancellation, a narrow-only monotonic deadline, injected-secret
redaction, and a LIFO cleanup stack. The POSIX and fake platforms implement the
same enforcement query and bounded adapter contracts. POSIX file operations use
retained descriptors and no-follow opens, HTTPS trusts only the endpoint CA,
and process execution clears ambient environment and owns one process group.
Fake calls record identities and sizes without retaining secret payloads.

The source evaluator does not construct or consume this context. Project checks
populate concrete source-facing identities but invoke no adapter. Pure call and
outcome constructors retain empty operational metadata, so the embedding
boundary cannot activate an effectful source route. See
[Authority and resource embedding](authority-and-resources.md) and
[Bounded operational modules](operational-modules.md).

## Language Server Protocol

`opaal-language-server` supports initialize, shutdown, exit, full-document
open/change/close, diagnostics, completion, hover, signature help, definition,
references, and whole-document formatting over standard input/output. It does
not execute open source, discover projects, accept incremental edits, or expose
TCP transport.

Request cancellation and document-generation checks prevent stale semantic
results from being returned as current.

## Validation and support boundaries

`ci/check_product.py` validates product identity, package shape, current-tree
exclusions, workflow parity, and the fail-closed unpublished-release boundary.
`ci/check_benchmarks.py` validates the seven-case host performance contract,
result summaries, digests, environment match, budgets, and regressions. The
public-boundary validator separately rejects private provenance and publishing
authority.

The implementation host boundary is macOS and Linux. No source or test claims
external image packaging, Redox support, or physical-hardware qualification.

[← Documentation index](README.md) · [Development](development.md)
