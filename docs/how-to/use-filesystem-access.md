# Use filesystem access in a task

Start with the four-file [first project](../learn/first-project.md). Create a regular `message.txt` inside `demo`, then replace its `tasks.opaal` with a task that constructs an absolute path beneath the project root:

```opaal
import project::context as project
import std::filesystem as fs
import std::path as path

action read_message() -> Bytes
effects {
    filesystem.read(project::root);
}
{
    let target = path::join(project::root, "message.txt")
    return fs::read(target, 1048576)
}
task read_message = read_message
```

Replace `rules = []` in `authority.toml` with one exact grant, keeping the existing schema, project, and environment fields:

```toml
[[rules]]
decision = "grant"
effect = "filesystem.read"
scope = "project.root"
required_enforcement = "enforced"
```

Check and plan `read_message`, then inspect the artifact before execution:

```sh
opaal check --project opaal.toml --task read_message --environment local
opaal plan --project opaal.toml --task read_message --environment local --expires-in 900s --out read.plan.json
opaal plan inspect read.plan.json
opaal execute --plan read.plan.json --accept sha256:EXACT_PLAN_DIGEST --journal read.run.jsonl
opaal audit --project opaal.toml --journal read.run.jsonl --out read.audit.json
```

Replace the placeholder with the complete digest printed by `plan`. A successful execute reports a run, and its journal contains a `filesystem.read` boundary. Audit validates the complete record. The example uses `path::join` because a relative `Path` input is not itself an absolute target accepted by the controlled file adapter.

Use `std::filesystem::read(target, max_bytes)` for a bounded regular-file read. The adapter retains the physical project root, walks each component without following symlinks, and repeats containment before opening the target. A lexical path check alone is not sufficient. Per-file reads stop at 16 MiB and aggregate evaluation reads at 32 MiB.

`std::filesystem::write_atomic(target, bytes)` publishes one declared evidence target by creating and syncing a private sibling, replacing the exact name, then syncing its parent. It does not roll back other task effects. Workflow plan, journal, and audit files have separate exclusive no-overwrite rules.

Before executing, run a project check and inspect the plan's request verdict and paths. A denied path, symlink, non-regular file, or exceeded limit fails closed. [Filesystem reference](../reference/std/filesystem.md) and [authority](../reference/operational/authority.md) give the exact boundary.
