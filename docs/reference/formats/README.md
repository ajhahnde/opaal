# File and artifact formats

OPAAL accepts closed, bounded control documents. A project selects a manifest, authority file, and tool lock. Check, plan, journal, and audit have distinct v2 artifact identities.

Those identities are `opaal.check.v2`, `opaal.plan.v2`,
`opaal.run-journal.v2`, and `opaal.audit.v2`.

Start with [common conventions](common-conventions.md) for canonical bytes, paths, limits, and version identities. Then look up the document:

- [`opaal.toml`](opaal-toml.md) names the project, root module, bindings, and environments.
- [Authority TOML](authority-toml.md) records exact grant and deny rows.
- [Tool lock](tool-lock.md) binds platform, executables, and child environment.
- [Check](check-artifact.md) records static findings and verdicts.
- [Plan](plan-artifact.md) binds the task to expiring identities.
- [Journal](journal-artifact.md) records a run as synced JSON lines.
- [Audit](audit-artifact.md) validates a complete or incomplete journal prefix.

Unknown schemas and fields are refused. A v2 suffix identifies an artifact format, not a language version.
