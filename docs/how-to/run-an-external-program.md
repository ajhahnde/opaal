# Run an external program

## Foreground program in a script or interactive session

Use a literal caret head to run any resolvable program in the foreground:

```opaal
^git status --short
```

To pass a list as two arguments, spread it in the command:

```opaal
let arguments = ["status", "--short"]
^git ...{arguments}
```

An ordinary expanded word becomes one native argument; `...{expression}`
spreads a list into separate arguments. OPAAL does not invoke a shell: quotes
and interpolation belong to OPAAL syntax, and a value
such as `$(command)` is ordinary argument text. An unknown bare head never
searches `PATH`. The CLI snapshots its native cwd and environment before
evaluation. `cd`, `export`, and `unset` change later process context, while
`pwd` returns the retained working directory as a value;
relative program paths and `PATH` entries use the retained cwd. Pipelines and
explicit redirections use the existing byte stream and `Status` rules.

The child inherits the session environment, which may include credentials,
and can perform any effect its OS user permits. OPAAL does not sandbox this
route, redact child output, record an audit, or roll back child effects. Use
the smallest practical launch environment, or `unset` to remove an inherited
entry before a child starts. For sensitive work that needs a declared tool and
reviewed authority, use the controlled project route below; its child effects
remain unenforced internally. The CLI does not load a `.env` file or limit
executable names to a whitelist. Background syntax and job control commands
remain refused. Supported interrupt and termination
signals allow two seconds for the owned group to end before OPAAL sends
`SIGKILL`. Direct execution has no automatic timeout. Cleanup covers only
members that stay in OPAAL's owned foreground group; host death and
descendants that leave it cannot be contained. The
[1.1 specification](../specification/1.1.md) and
[limits](../reference/limits.md) give the exact boundary.

The repository's [golden source](../../tests/golden/direct-external-execution/foreground.opaal)
is checked with a purpose-built executable. Run
`cargo test -p opaal-cli --test direct_external_golden` to verify exact argument,
environment, cwd, stream, redirection, status, refusal, and termination
behavior on a supported host.

## Maintained program in an accepted project

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

A direct source command such as `^git status` uses the ambient foreground
route. It has no accepted plan, declared tool identity, exact authority, or
journal. See [process reference](../reference/std/process.md), [tool lock](../reference/formats/tool-lock.md),
and [capabilities](../reference/operational/capability-matrix.md).
