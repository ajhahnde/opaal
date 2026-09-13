# `opaal.toml`

The project manifest is an explicitly selected regular file named `opaal.toml`. Its top-level keys are exactly `schema_version`, `project`, `paths`, and optional `tools`, `endpoints`, `secrets`, and `environments` tables. It uses `schema_version = 1`.

```toml
schema_version = 1

[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"

[paths]
root = "."
evidence = "evidence.json"

[environments.ci]
authority = "authority-ci.toml"
tool_lock = "tools-host.toml"
```

`project.name` is an OPAAL identifier. `root_module` names the source file that may export tasks; `required_opaal` is a SemVer range checked against the running toolchain. `paths.root` and `paths.evidence` are project-contained paths. The example range admits the current development build and the 1.0 release; a deployed project can choose a narrower requirement.

A `[tools.ID]` table has only `adapter` (`git` or `cargo`) and `version`. An `[endpoints.ID]` table has `url`, `methods`, `secret_headers`, `tls`, and, for TLS, `tls_server_name` and `ca`. A `[secrets.ID]` table has only `kind = "injected"`. An `[environments.ID]` table names exactly one `authority` and one `tool_lock`. Named entries across these tables share the 256-entry limit.

All manifest paths are relative, lexically contained by the selected project root, and opened without following symlinks. OPAAL does not search for this file implicitly. See [projects and tasks](../operational/projects-and-tasks.md) and [common conventions](common-conventions.md).
