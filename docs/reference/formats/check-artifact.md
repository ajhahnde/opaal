# Check artifact

`opaal check --project … --format json` writes one canonical `opaal.check.v2` object to standard output. Without `--format json`, successful check is silent. Check never invokes the task, reads an executable, probes a tool, reads a secret, or calls an operational adapter.

The exact top-level fields are `schema`, `schema_version`, `toolchain`, `project`, `task`, `inputs`, `secrets`, `sources`, `authority`, `tools`, `findings`, `outcome`, and `digest`. `schema_version` is 2. Inputs preserve their explicit `value` or `file` binding mode. Sources, tools, and authority requests carry bounded identities and deterministic order. `findings` is the ordered static diagnostic set.

An executable check has outcome class `success`, no error finding, every authority request granted with an admissible enforcement verdict, and at most one supported secret requirement. A missing or denied grant, an unsupported or unknown host verdict, or multiple secret requirements produces a refused check. The validator rejects a file whose findings, verdicts, and outcome disagree.

The digest covers the canonical object with its `digest` field omitted. Unknown fields and unsupported schema versions refuse parsing. [Plan](plan-artifact.md) builds on the selected check but adds time and observed identities; a successful check is not an authorization to execute by itself.
