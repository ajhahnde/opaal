# OPAAL documentation

OPAAL `1.0.0-alpha.1` is an unreleased, standalone language 1 foundation.
These pages document the current OPAAL source, tooling, and verification
boundary; preserved Flash material under `history/` is lineage, not a current
OPAAL contract.

| Goal | Document |
| --- | --- |
| Learn from a small pure program | [OPAAL by example](by-example.md) |
| Understand language 1 semantics and exclusions | [Language 1 foundation](opaal-language-1-foundation.md) |
| Understand crate and execution boundaries | [Architecture](architecture.md) |
| Build, test, fuzz, and benchmark the repository | [Development](development.md) |
| Inspect performance methodology | [Performance benchmarks](../benchmarks/README.md) |
| Prepare a contribution | [Contributing](../CONTRIBUTING.md) |
| Report a vulnerability privately | [Security policy](../SECURITY.md) |
| Inspect release history | [Changelog](../CHANGELOG.md) |

Current source modules require `.opaal` and `language 1`. The `opaal` CLI,
formatter, checker, runtime, interactive client, and language server share that
identity. Flash 1 parsing is feature-gated and reachable only through the
read-only migration crate.

The current foundation has no effect or authority model. Filesystem, process,
environment, network, terminal, project, action, task, package, and controlled
workflow capabilities remain unavailable. Host tests do not establish another
system's packaging, image, target, or hardware support.

[← OPAAL overview](../README.md)
