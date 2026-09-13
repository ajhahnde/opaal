# Authority TOML

An environment's authority file has exactly `schema_version`, `project`, `environment`, and `rules`. Version 1 binds the exact manifest project name and selected environment. A mismatch refuses the file.

```toml
schema_version = 1
project = "demo"
environment = "ci"

[[rules]]
decision = "grant"
effect = "filesystem.read"
scope = "project.root"
required_enforcement = "enforced"
```

Every row has `decision`, `effect`, and `scope`. A grant also has `required_enforcement`; a deny must not. Decisions are `grant` or `deny`. Grants use `enforced` for filesystem, network, secret, and clock effects. `process.run` grants require `acknowledge-unenforced` because a locked child can still perform its own filesystem or network calls.

The scope must belong to the effect family and name an existing project identity. Rows are exact: no wildcard, prefix, inherited, or default grant exists. Duplicate normalized effect/scope rows, including conflicting decisions, refuse the file. At most 256 rules are accepted. A missing rule denies a request.

This file expresses operator authority; it does not change an action's declared effects or a host adapter's enforcement result. See [authority verdicts](../operational/authority.md) and [capabilities](../operational/capability-matrix.md).
