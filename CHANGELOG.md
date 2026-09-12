# Changelog

All notable changes to OPAAL are documented here.

## [Unreleased]

### Changed

- Make every supported accepted project plan executable: distinguish lexical
  `--input` from digest-bound `--input-file`, support zero or one run-local
  secret while refusing larger cardinality before acceptance, derive execution
  authority and tools from the plan, generate omitted run IDs, and replace v1
  check/plan artifacts with closed v2 schemas.
- Use one predictable value language: bare names for reads, assignment, and
  calls; final-expression block values; `{expression}` interpolation;
  `...{expression}` list spread; and explicit `^literal` external heads.
- Add canonical project check and expiring plan artifacts, exact request-local
  acceptance, stale-revalidated task execution, exclusive hash-chained run
  journals with paired secret-reveal evidence, and read-only complete or
  incomplete audit artifacts while ordinary source remains non-operational;
  process-bearing accepted execution uses retained executable descriptors on
  Linux and is explicitly unsupported on macOS.
- Add maintained bounded data, path, filesystem, time, version, integrity,
  URL, HTTP/TLS, and locked Git/Cargo process APIs with deterministic fakes,
  exact resource ceilings, a one-use typed secret-header sink, explicit child
  environments, and POSIX no-follow/cancellation cleanup.
- Add typed actions with explicit effect declarations, exported project tasks,
  strict project/authority/tool-lock documents, non-executing task inspection,
  and explicit project checks.
- Carry a concrete action identity across analysis, help, and editor queries,
  carry project, task, and environment identities through project checks and
  downstream metadata, and expose project-qualified tool identities through
  manifest and task inspection.
- Add one explicit fail-closed embedding context for exact authority verdicts,
  cancellation and deadlines, secret redaction, and owned-resource cleanup
  while keeping OPAAL source pure-only.
- Treat every UTF-8 `.opaal` module as ordinary directive-free OPAAL source.
- Use one parser, syntax tree, module identity, formatter, runtime, and semantic
  query model across file, interactive, embedding, and editor frontends.
- Key command-namespace compatibility metadata to the installed OPAAL
  toolchain major while retaining independent protocol and result schemas.
- Keep the pure source boundary, structured outcomes, deterministic
  resource ceilings, module aliases, nominal types, operations, and streams.

### Removed

- Remove dollar-prefixed references, braced dollar expansion, command
  substitution, dynamic external-command forms, and unknown-name process
  fallback without a compatibility mode.
- Remove all alternate source-selection, conversion, and compatibility paths
  from the shipped product tree.
- Remove obsolete package, binary, feature, fixture, fuzz, and documentation
  surfaces that were outside the current OPAAL product.

### Validation

- Add permanent source-product and fail-closed unpublished-release validators.
- Preserve stable `required` and `security-required` workflow aggregates,
  public-boundary checks, benchmark validation, and five-target fuzz smoke.

`1.0.0-alpha.1` remains an unreleased, non-publishable development version.
