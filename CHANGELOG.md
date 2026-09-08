# Changelog

All notable changes to OPAAL are documented here.

## [Unreleased]

### Changed

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

- Remove all alternate source-selection, conversion, and compatibility paths
  from the shipped product tree.
- Remove obsolete package, binary, feature, fixture, fuzz, and documentation
  surfaces that were outside the current OPAAL product.

### Validation

- Add permanent source-product and fail-closed unpublished-release validators.
- Preserve stable `required` and `security-required` workflow aggregates,
  public-boundary checks, benchmark validation, and five-target fuzz smoke.

`1.0.0-alpha.1` remains an unreleased, non-publishable development version.
