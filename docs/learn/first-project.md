# First project

This project exports a pure task. It is deliberately small so the check, plan,
acceptance, journal, and audit steps are visible without a host effect. Create
a directory named `demo` at the checkout root and put these four files in it.

`opaal.toml` selects the root module and a local environment:

```toml
schema_version = 1

[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0,<2.0.0"

[paths]
root = "."
evidence = "evidence.json"

[environments.local]
authority = "authority.toml"
tool_lock = "tools.toml"
```

The range admits compatible 1.x toolchains. A deployed project may narrow it. In `tasks.opaal`, the task exports an action whose effect set is empty:

```opaal
action greet(name: String) -> String
effects {
}
{
    return name
}
task welcome = greet
```

Put these exact rows in `authority.toml`:

```toml
schema_version = 1
project = "demo"
environment = "local"
rules = []
```

The lock still records the selected host. Put the host triple reported by `rustc -vV` in `tools.toml`; for Apple silicon macOS it is `aarch64-apple-darwin`, and for a typical x86-64 Linux Rust host it is `x86_64-unknown-linux-gnu`:

```toml
schema_version = 1
project = "demo"
environment = "local"
platform = "aarch64-apple-darwin"
tools = []

[child_environment]
inherit = []
```

From the checkout root, build the CLI, put it on your shell's path, and enter
the project. An installed `opaal` binary works as well:

```sh
cargo build --workspace --locked --release
export PATH="$(pwd)/target/release:$PATH"
cd demo
```

Inspect and check the task, then write and inspect a plan:

```sh
opaal task inspect --project opaal.toml welcome
opaal check --project opaal.toml --task welcome --environment local --input name=reader --format json
opaal plan --project opaal.toml --task welcome --environment local --input name=reader --expires-in 900s --out welcome.plan.json
opaal plan inspect welcome.plan.json
```

Check's JSON has `outcome.class = "success"` and an empty request set. Plan prints `plan sha256:…`. Read the inspected plan, then copy its **complete** digest into the next command before it expires:

```sh
opaal execute --plan welcome.plan.json --accept sha256:EXACT_PLAN_DIGEST --journal welcome.run.jsonl
opaal audit --project opaal.toml --journal welcome.run.jsonl --out welcome.audit.json
opaal audit inspect welcome.audit.json
```

Execute reports one successful run. Audit reports `complete` and inspection shows the validated journal evidence. The output paths must not already exist; repeat the exercise with fresh names. The release-readiness harness also runs a complete secret-free project path on Linux and macOS. A process-bearing task has a different host boundary: it is executable on qualified Linux and produces a refused plan on macOS.

For field definitions use [project manifest](../reference/formats/opaal-toml.md), [plan](../reference/formats/plan-artifact.md), and [journal/audit](../reference/formats/README.md).
