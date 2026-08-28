# Flash Documentation

[FlashOS](../../../README.md) › [Flash](../README.md) › Documentation

Use this page to find the Flash guide that matches what you are doing. System-wide build, image, verification, and hardware topics are covered in the main [FlashOS product guide](../../../docs/README.md).

> **Project status:** FlashOS as a complete operating system remains pre-alpha
> software. Flash 1.0.0 is released as the component contract in the current
> source. Availability in a particular
> FlashOS image or on another target is qualified separately; execution on a
> Linux or macOS host is not proof of FlashOS target support.

## Guides

| Goal                           | Guide                               | Scope                                                                                                                                        |
| ------------------------------ | ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| Learn through runnable programs | [Flash by Example](by-example.md)   | Structured values, byte conversion, status/error handling, and non-executing plan inspection                                                |
| Learn the Flash language  | [Language Guide](language-guide.md) | Source structure, values, bindings, expressions, functions, modules, imports, exports, name resolution, commands, and typed pipelines        |
| Create and verify `.fsh` files | [Scripting](scripting.md)           | Script execution, arguments, checks, formatting, external processes, redirections, statuses, jobs, and limits                               |
| Understand the implementation  | [Architecture](architecture.md)     | Dependency direction, source and module analysis, language-server protocol boundary, runtime planning, platform capabilities, and lifecycle |
| Modify and qualify Flash  | [Development](development.md)       | Toolchains, formatter and checker gates, language-server integration and gates, tests, scheduling stress, fuzzing, target builds, and documentation validation |

## Which guide to use

The guides overlap in examples, but each has a main job:

- The [Flash overview](../README.md) introduces the component, its role in FlashOS, its v1 compatibility promise, and its implementation.
- [Flash by Example](by-example.md) is the short, runnable introduction. Its examples demonstrate behavior defined elsewhere.
- The [Language Guide](language-guide.md) defines syntax and semantics: functions, modules, imports and exports, name resolution, type metadata, and pipelines. It is not a build guide.
- The [Scripting Guide](scripting.md) explains how to work with `.fsh` files, arguments, checks, formatting, external processes, redirections, statuses, and jobs.
- The [Architecture Guide](architecture.md) explains implementation
  responsibilities, source and module analysis, the language-server protocol
  boundary, runtime data flow, platform capabilities, adapters, and process
  lifecycle.
- The [Development Guide](development.md) covers component-specific build and
  verification procedures, including language-server invocation, editor
  integration, and quality gates. Repository-wide verification layers remain
  documented in [FlashOS Verification](../../../docs/verification.md).

If a guide appears to disagree with the implementation, check the current source, tests, and configuration before relying on the behavior or updating the claim.

## Documentation classes

These categories show which pages define Flash 1.0 behavior and which pages explain or implement it:

| Class | Current material | Contract effect |
| --- | --- | --- |
| Frozen v1 contract | The Flash overview's v1 contract section, the Language Guide, and the normative behavior in the Scripting Guide | Defines the grammar, runtime, namespace, tooling, and compatibility baseline released as Flash 1.0.0. Changes must preserve v1 compatibility or follow a future language-major decision. |
| Tutorial and usage guidance | Worked examples and task-oriented instructions in the Language and Scripting guides | Teaches the frozen contract and is exercised where runnable, but does not introduce semantics independently of the contract text. |
| Implementation and verification reference | The Architecture and Development guides plus focused source-adjacent README files | Describes current internals, maintenance, and evidence. It does not expand the public language contract unless it explicitly identifies a contract surface. |
| Experimental or future proposal | Any document explicitly labeled experimental or future | Has no current availability or compatibility effect and cannot override the frozen contract. No active guide in this index is in this class. |

An experimental or future document must say so at the top. It stays outside
the active guide set until the behavior has been selected, implemented,
documented, and tested.

## Contract and release availability

The v1 grammar and public runtime contract are frozen and released as Flash
1.0.0. This does not mean that every earlier binary, FlashOS image, or other
target contains every part of that contract. Check the release and target
results for the build you are using.

Language and tool behavior stays compatible with the documented baseline. Release notes and target results tell you which functions are available in a particular build. A host run, a target build, inclusion in an image, and a runtime test each prove something different.

## Supporting technical references

Focused implementation and verification areas maintain narrower README files beside the corresponding source or fixtures:

- [Cargo workspace manifest](../Cargo.toml) — Workspace membership and shared package metadata.
- [Flash changelog](../CHANGELOG.md) — Component release history and current unreleased changes.
- [Executable examples](../examples/) — Curated sources used by Flash by Example and documentation verification.
- [`flash-lsp` crate](../crates/flash-lsp/) — Stdio transport, versioned document workspace, protocol projection, and language-server executable.
- [Scheduling stress](../scheduling/README.md) — Seeded host pipeline-cancellation and job-control campaigns, retained results, and exact replay.
- [Performance benchmarks](../benchmarks/README.md) — Versioned startup, prompt, command, pipeline, structured-stream-memory, and completion measurements with evidence-derived budgets.
- [Flash v1 exercises](../exercises/README.md) — Exhaustive user-path inventory,
  retained host evidence, exact FlashOS owners, and qualification boundaries.
- [Flash 1.0.0 release record](../release/v1.toml) — Released component version,
  exact contract owners, candidate gates, and explicit qualification limits.
- [Fuzz targets](../fuzz/README.md) — Lexer, parser, and ordinary-word expander fuzz inputs, campaigns, and corpus handling.
- [End-to-end tests](../tests/e2e/README.md) — Location of black-box and pseudoterminal test fixtures.
- [Test fixtures](../tests/fixtures/README.md) — Rust child programs used to observe process, descriptor, status, and stream behavior.
- [Grammar golden corpus](../tests/golden/grammar/README.md) — Parser fixture inventory and expected classifications.
- [Lexical golden corpus](../tests/golden/lexical/README.md) — Lexer fixture inventory and completeness classifications.

These are focused test and implementation references. Use the [Development Guide](development.md) to decide which checks to run and how to interpret the results.

## Related FlashOS documentation

Flash is developed as a component of FlashOS, but several integration topics belong to the system-wide documentation:

- [Getting Started](../../../docs/getting-started.md) — Build a FlashOS image, boot it in QEMU, and reach the initial shell session.
- [FlashOS Architecture](../../../docs/architecture.md) — Understand system layers, image configuration, package integration, and component boundaries.
- [FlashOS Development](../../../docs/development.md) — Work with the repository, build system, configuration profiles, and general development workflow.
- [Verification and Testing](../../../docs/verification.md) — Distinguish host checks, target compilation, package and image construction, QEMU qualification, and physical hardware evidence.

---

[← Back to Flash Overview](../README.md) · [FlashOS Product Guide](../../../docs/README.md) · [Next: Flash by Example →](by-example.md)
