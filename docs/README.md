# OPAAL documentation

OPAAL `1.0.0-alpha.1` is an unreleased standalone language and toolchain.

| Goal | Document |
| --- | --- |
| Learn from a small pure program | [OPAAL by example](by-example.md) |
| Understand source and semantic boundaries | [Language foundation](language-foundation.md) |
| Author, check, plan, execute, and audit typed project actions | [Actions and explicit projects](actions-and-projects.md) |
| Embed explicit authority and owned resources | [Authority and resource embedding](authority-and-resources.md) |
| Use maintained bounded host operations | [Bounded operational modules](operational-modules.md) |
| Understand crate and execution boundaries | [Architecture](architecture.md) |
| Build, test, fuzz, and benchmark the repository | [Development](development.md) |
| Inspect performance methodology | [Performance benchmarks](../benchmarks/README.md) |
| Prepare a contribution | [Contributing](../CONTRIBUTING.md) |
| Report a vulnerability privately | [Security policy](../SECURITY.md) |
| Inspect unreleased changes | [Changelog](../CHANGELOG.md) |

Every frontend consumes the same directive-free `.opaal` source model. Typed
actions and explicitly selected project tasks can be formatted, inspected,
checked, rendered as an identity-bound plan, explicitly accepted, executed,
journaled, and audited. The accepted-plan route uses the same fail-closed
authority, owned-resource context, and maintained bounded adapters as the
embedding API. Ordinary source execution remains non-operational, and packages
remain unavailable. Process-bearing accepted execution is Linux-only; macOS
supports inspection, check, refused planning, and audit for those tasks.

Host tests establish only the explicitly exercised host surfaces.

[← OPAAL overview](../README.md)
