# Flash Documentation

[FlashOS](../../../README.md) › [Flash](../README.md) › Documentation

This page is the central index for the public Flash documentation. It directs users, script authors, and component developers to the appropriate guide; system-wide FlashOS build, image, verification, and hardware documentation remains under the main [FlashOS documentation](../../../docs/README.md).

> **Project status:** FlashOS as a complete operating system remains pre-alpha software. However, these Flash component guides describe the intended stable Flash v1.0 contract. Note that not every v1 feature is automatically available in every current FlashOS image or on every target platform, and successful execution on a Linux or macOS development host is not automatic proof of FlashOS target support.

## Guides

| Goal                           | Guide                               | Scope                                                                                                                                        |
| ------------------------------ | ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| Learn the Flash language  | [Language Guide](language-guide.md) | Source structure, values, bindings, expressions, functions, modules, imports, exports, name resolution, commands, and typed pipelines        |
| Create and verify `.fsh` files | [Scripting](scripting.md)           | Script execution, script arguments, non-executing checks, canonical formatting, external processes, redirections, statuses, jobs, and limits |
| Understand the implementation  | [Architecture](architecture.md)     | Dependency direction, source and module analysis, language-server protocol boundary, runtime planning, platform capabilities, and lifecycle |
| Modify and qualify Flash  | [Development](development.md)       | Toolchains, formatter and checker gates, language-server integration and gates, tests, scheduling stress, fuzzing, target builds, and documentation validation |

Readers who are new to Flash should begin with the [component overview](../README.md), continue with the [Language Guide](language-guide.md), and then use the [Scripting Guide](scripting.md) for practical program execution. Developers changing the implementation should also read the [Architecture](architecture.md) and [Development](development.md) guides.

## Documentation boundaries

Each guide has a distinct responsibility:

- The [Flash overview](../README.md) introduces the component, its role in FlashOS, the v1 contract boundary, implementation responsibilities, and the available documentation.
- The [Language Guide](language-guide.md) owns language semantics, functions, modules, imports and exports, name resolution, typed function metadata, and structured pipelines. It is not the primary reference for build procedures.
- The [Scripting Guide](scripting.md) owns practical `.fsh` execution, script arguments, non-executing checks, formatting modes, external processes, redirections, statuses, and jobs. It does not duplicate the complete language reference.
- The [Architecture Guide](architecture.md) explains implementation
  responsibilities, source and module analysis, the language-server protocol
  boundary, runtime data flow, platform capabilities, adapters, and process
  lifecycle.
- The [Development Guide](development.md) owns component-specific build and
  verification procedures, including language-server invocation, editor
  integration, and quality gates. Repository-wide verification layers remain
  documented in [FlashOS Verification](../../../docs/verification.md).

When documentation and implementation appear to disagree, inspect the current source, tests, and configuration before relying on a behavior or changing a public claim.

## Contract and release availability

These guides describe the intended Flash v1 contract. They do not imply that every current binary, FlashOS image, or target exposes every part of that contract.

Language and tooling responsibilities remain stable at the documentation level, while release notes, target evidence, and capability qualification determine which functions are available in a particular build. Host execution, target compilation, image integration, and runtime qualification are separate forms of evidence.

## Supporting technical references

Focused implementation and verification areas maintain narrower README files beside the corresponding source or fixtures:

- [Cargo workspace manifest](../Cargo.toml) — Workspace membership and shared package metadata.
- [`flash-lsp` crate](../crates/flash-lsp/) — Stdio transport, versioned document workspace, protocol projection, and language-server executable.
- [Scheduling stress](../scheduling/README.md) — Seeded host pipeline-cancellation and job-control campaigns, retained results, and exact replay.
- [Fuzz targets](../fuzz/README.md) — Lexer, parser, and ordinary-word expander fuzz inputs, campaigns, and corpus handling.
- [End-to-end tests](../tests/e2e/README.md) — Location of black-box and pseudoterminal test fixtures.
- [Test fixtures](../tests/fixtures/README.md) — Rust child programs used to observe process, descriptor, status, and stream behavior.
- [Grammar golden corpus](../tests/golden/grammar/README.md) — Parser fixture inventory and expected classifications.
- [Lexical golden corpus](../tests/golden/lexical/README.md) — Lexer fixture inventory and completeness classifications.

These references document specific test or implementation contracts. The [Development Guide](development.md) remains the main entry point for deciding which checks to run and what their results establish.

## Related FlashOS documentation

Flash is developed as a component of FlashOS, but several integration topics belong to the system-wide documentation:

- [Getting Started](../../../docs/getting-started.md) — Build a FlashOS image, boot it in QEMU, and reach the initial shell session.
- [FlashOS Architecture](../../../docs/architecture.md) — Understand system layers, image configuration, package integration, and component boundaries.
- [FlashOS Development](../../../docs/development.md) — Work with the repository, build system, configuration profiles, and general development workflow.
- [Verification and Testing](../../../docs/verification.md) — Distinguish host checks, target compilation, package and image construction, QEMU qualification, and physical hardware evidence.

---

[← Back to Flash Overview](../README.md) · [Next: Language Guide →](language-guide.md)
