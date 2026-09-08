# OPAAL documentation

OPAAL `1.0.0-alpha.1` is an unreleased standalone language and toolchain.

| Goal | Document |
| --- | --- |
| Learn from a small pure program | [OPAAL by example](by-example.md) |
| Understand source and semantic boundaries | [Language foundation](language-foundation.md) |
| Author and check typed actions and explicit projects | [Actions and explicit projects](actions-and-projects.md) |
| Embed explicit authority and owned resources | [Authority and resource embedding](authority-and-resources.md) |
| Use maintained bounded host operations | [Bounded operational modules](operational-modules.md) |
| Understand crate and execution boundaries | [Architecture](architecture.md) |
| Build, test, fuzz, and benchmark the repository | [Development](development.md) |
| Inspect performance methodology | [Performance benchmarks](../benchmarks/README.md) |
| Prepare a contribution | [Contributing](../CONTRIBUTING.md) |
| Report a vulnerability privately | [Security policy](../SECURITY.md) |
| Inspect unreleased changes | [Changelog](../CHANGELOG.md) |

Every frontend consumes the same directive-free `.opaal` source model. Typed
actions and explicitly selected project tasks can be formatted, inspected, and
checked without execution. The embedding API has a fail-closed authority and
owned-resource context plus maintained bounded operational adapters, while
effectful source execution, packages, and controlled workflows remain
unavailable.

Host tests establish only the explicitly exercised host surfaces.

[← OPAAL overview](../README.md)
