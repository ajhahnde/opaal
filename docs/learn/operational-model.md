# The operational model

A plain source file can be checked and evaluated without project selection. To expose an action as a task, place its `task` declaration in a manifest-selected root module and name the exact `opaal.toml` on the CLI. An action declares a closed set of effect requests; an environment then supplies authority rows and a tool lock. Neither declaration nor import is permission by itself.

The path through one task is `task inspect`, `check`, `plan`, human review of `plan inspect`, `execute --accept` with the exact digest, `audit`, and `audit inspect`. Check does not run the action. Plan binds source, inputs, control files, host and tool identity, and expiry. Execute revalidates those identities before admitting work. Its journal records what crossed the controlled boundary, including partial effects; audit validates the journal without continuing it.

Acceptance is supplied on one execute request. There is no stored accepted state. A changed input or expired plan needs a new plan and review. A journal is evidence, not a rollback mechanism.

The first project uses a pure task, so it exercises the complete artifact path on the supported hosts without requiring a tool or secret. Controlled filesystem, HTTP, secret, and process operations add exact grants and host conditions; process-bearing accepted execution is Linux-only. Continue with [first project](first-project.md), then use the [operational reference](../reference/operational/README.md) for exact contracts.
