# OPAAL documentation

OPAAL `1.0.0-alpha.1` is an unreleased standalone language and toolchain.

| Goal | Document |
| --- | --- |
| Learn from a small pure program | [OPAAL by example](by-example.md) |
| Understand source and semantic boundaries | [Language foundation](language-foundation.md) |
| Embed explicit authority and owned resources | [Authority and resource embedding](authority-and-resources.md) |
| Understand crate and execution boundaries | [Architecture](architecture.md) |
| Build, test, fuzz, and benchmark the repository | [Development](development.md) |
| Inspect performance methodology | [Performance benchmarks](../benchmarks/README.md) |
| Prepare a contribution | [Contributing](../CONTRIBUTING.md) |
| Report a vulnerability privately | [Security policy](../SECURITY.md) |
| Inspect unreleased changes | [Changelog](../CHANGELOG.md) |

Every frontend consumes the same directive-free `.opaal` source model. The
embedding API has a fail-closed authority and owned-resource context, while
OPAAL source remains pure-only: filesystem, process, environment, network,
terminal, project, action, task, package, and controlled workflow capabilities
remain unavailable from source.

Host tests establish only the explicitly exercised host surfaces.

[← OPAAL overview](../README.md)
