# OPAAL architecture

OPAAL separates source semantics, pure analysis/evaluation, host contracts,
frontends, migration, and observation. Dependencies point inward toward
semantic owners; no frontend defines a second language personality.

## Crate graph

```text
opaal-syntax
├── opaal-migrate (explicit Flash-v1 migration feature)
└── opaal-runtime
    ├── opaal-lsp
    └── opaal-cli
        └── opaal-platform-posix

opaal-platform ── opaal-runtime
       └────────── opaal-platform-posix
```

- `opaal-syntax` owns source bytes, spans, tokens, syntax trees, language
  detection, parsing, formatting primitives, completion context, and
  diagnostics.
- `opaal-runtime` owns canonical module analysis, types, operations, values,
  streams, outcomes, semantic queries, deterministic budgets, and pure
  evaluation.
- `opaal-cli` owns invocation parsing, file inspection, reporting, interactive
  presentation, and the current planning refusal.
- `opaal-lsp` owns stdio framing, JSON-RPC lifecycle, document snapshots,
  cancellation, and projection of shared semantic queries.
- `opaal-platform` defines host capability traits and byte-preserving native
  data contracts. `opaal-platform-posix` implements the macOS/Linux adapter and
  test observers.
- `opaal-migrate` owns schema-2 Flash 1 analysis. Its dependency explicitly
  enables syntax migration support; no other workspace crate does.

The standalone Cargo workspace contains those seven crates. The unpublished
`opaal-fuzz` package has its own workspace so nightly instrumentation cannot
change the normal locked graph.

## Source and module flow

File frontends first enforce `.opaal`, UTF-8, and the leading `language 1`
directive. Parsing retains source-local spans. Module loading canonicalizes only
explicit local imports, applies source/module/depth/work ceilings, and builds a
singular identity graph. Standard modules are compiled identities rather than
filesystem search results.

Static analysis resolves aliases, imports, exports, nominal types, generics,
patterns, operations, effects, and semantic-query indexes over that same graph.
The formatter consumes syntax rather than rebuilding semantics. The language
server snapshots documents and discards stale results by generation.

## Pure execution boundary

OPAAL language 1 evaluates an analyzed module program with
`EvaluationPolicy::PureOpaalV1`. Known effects fail analysis. Any dynamically
reached effect is refused before platform or executable access. Successful
non-interactive execution retains a value and emits no implicit bytes.

The repository still contains platform contracts and host observers needed to
test refusal, terminal presentation, and future adapters. Their presence does
not grant source authority. Current OPAAL entry points pass no legacy language
selection, configuration, history, or execution fallback.

Outcome composition keeps completed values/status, catchable language errors,
cancellation, refusal, fatal failure, partial evidence, and cleanup evidence
structurally distinct. Resource counters are deterministic language evidence;
wall-clock and RSS observations belong only to the benchmark harness.

## Language Server Protocol

`opaal-language-server` uses standard input/output only. It supports initialize,
shutdown, exit, full-document open/change/close, diagnostics, completion, hover,
signature help, definition, references, and whole-document formatting. It does
not execute open source, discover projects, accept incremental edits, or expose
TCP transport.

The server uses OPAAL language identity, `.opaal` paths, source `opaal`, and
server name `OPAAL Language Server`. Request cancellation and document
generation checks prevent stale semantic results from being returned as
current.

## Migration isolation

The syntax crate's Flash-v1 lexer/parser/classifier/formatter facade is compiled
only with the default-off `flash-v1-migration` feature. `opaal-migrate` is the
sole enabler and caller. The default syntax API does not export that module,
which is checked with a negative consumer compile.

Migration resolves only explicit `.fsh` roots and static imports, reads each
canonical source once, and emits deterministic schema-2 human or compact JSON
reports. It performs no execution, apply operation, project discovery, tool
probing, or hidden schema downgrade.

## Evidence and support boundaries

`ci/check_transition.py` owns executable identity and reachability claims.
`ci/check_benchmarks.py` validates the exact seven-case host performance
contract, result summaries, digests, environment match, budget derivation, and
regressions. Both use the Python standard library.

The current implementation host boundary is macOS and Linux. No source or test
in this repository claims that OPAAL is packaged in an external image, runs on
Redox, or is qualified on physical hardware.

[← Documentation index](README.md) · [Development](development.md)
