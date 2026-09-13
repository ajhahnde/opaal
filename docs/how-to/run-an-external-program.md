# Run a maintained external program

Declare a tool in `opaal.toml` with the `git` or `cargo` adapter and a version range. The selected environment's lock must identify that tool's exact absolute executable path, canonical version, SHA-256 digest, platform, and complete child environment. Import `project::tools` in the task module, declare `process.run(tools::ID)`, and grant that exact request with `acknowledge-unenforced`.

For a Git task, add `[tools.git]` with `adapter = "git"` and a range matching the intended binary. In the manifest-selected root module, export an action like this:

```opaal
import project::tools as tools
import std::process as process

action status() -> process::ToolResult
effects {
    process.run(tools::git);
}
{
    return process::run(tools::git, ["status", "--short"])
}
task status = status
```

Grant `effect = "process.run"`, `scope = "tool.git"`, and `required_enforcement = "acknowledge-unenforced"` in the selected authority file. Add exactly one `[[tools]]` row for `git` to its lock, with the absolute native executable path, canonical version, digest, and matching adapter; set the lock's top-level `platform` and its complete `[child_environment]`. The [tool-lock reference](../reference/formats/tool-lock.md) gives the closed shape. The checked-in [qualification project](../../tests/golden/release-readiness/) contains a maintained Git invocation and its authority row; its host lock is generated for the machine running the harness.

Run `task inspect`, `check`, `plan`, and `plan inspect` for this task as in the [first project](../learn/first-project.md). On a qualified Linux host, accept the exact digest and execute to obtain a `ToolResult` and a journaled `process.run` boundary. On macOS, check reports `CHECK008` and plan records a refusal, so there is no accepted execute step.

The controlled source operation is `std::process::run(tool, arguments)`. It accepts a manifest-bound tool identity and returns a `ToolResult` with status, stdout, and stderr. A string pathname is not a tool identity. The maintained version probes are fixed to Git and Cargo; project data cannot ask the adapter to probe another command.

Check, plan, inspect, and review the task first. On Linux, execute revalidates the locked executable and starts its retained descriptor under a complete child environment. On macOS, check reports `CHECK008` and planning yields a refused, non-executable artifact. Do not substitute a pathname spawn. The parent binds identity and lifetime; the child's own filesystem and network activity remains unenforced.

A direct source command such as `^git status` is not this accepted project route and is refused under ordinary source authority. See [process reference](../reference/std/process.md), [tool lock](../reference/formats/tool-lock.md), and [capabilities](../reference/operational/capability-matrix.md).
