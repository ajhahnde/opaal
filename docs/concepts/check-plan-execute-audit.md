# Check, plan, execute, audit

A task moves from a declaration to evidence in distinct steps. `task inspect` shows the exported action. `check` validates its source, inputs, grants, lock, and target capability without calling an adapter. `plan` adds bounded observations and expiry, then seals those facts under one digest for a human to inspect.

The reviewer passes that exact digest to `execute --accept`. Nothing is permanently marked accepted: a new request needs its own digest. Execution checks that the source, files, authority, lock, executable, platform, and toolchain still match the plan. If they do, the task runs under a controlled host and writes a synced journal. If they do not, it stops before the affected work.

The journal records ordered action and effect boundaries, including partial effects and cleanup. `audit` validates the canonical lines and hash chain and projects them into an inspectable complete or incomplete artifact. It does not continue an interrupted task. The sequence provides a review trail; it does not make external effects transactional or reversible.

Follow [first project](../learn/first-project.md) for a working path. The [lifecycle reference](../reference/operational/lifecycle.md) owns the exact command and artifact behavior.
