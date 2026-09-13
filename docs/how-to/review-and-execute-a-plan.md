# Review and execute a plan

Check the selected task, environment, and inputs first. Then write one expiring plan to an absent path beneath the explicit project root:

```sh
opaal check --project opaal.toml --task welcome --environment local --input name=reader --format json
opaal plan --project opaal.toml --task welcome --environment local --input name=reader --expires-in 900s --out welcome.plan.json
opaal plan inspect welcome.plan.json
```

Read the inspected task, outcome, bound input mode, authority verdicts, platform, and expiry. A `refused` plan is useful for diagnosis but cannot run. For a successful plan, copy the **complete** `sha256:` digest printed by `plan` into the execute request:

```sh
opaal execute --plan welcome.plan.json --accept sha256:EXACT_PLAN_DIGEST --journal welcome.run.jsonl
```

`--accept` applies to this request only. Execute does not accept a task name, environment, authority path, or new inputs in place of those bound by the plan. It checks expiry and rereads the bound source, input snapshots, manifest, authority, lock, child environment, CA material, executables, platform, and toolchain before the action receives a host. A stale plan needs a new check, plan, and review; OPAAL does not silently update the old one.

Journal creation is exclusive; use an absent project-contained path. A run ID is generated when omitted. For a one-secret plan add the exact `--secret-stdin ID` and non-terminal input; omit it for a secret-free plan. Then [audit the run](audit-a-run.md). The [plan format](../reference/formats/plan-artifact.md) lists the bound fields.
