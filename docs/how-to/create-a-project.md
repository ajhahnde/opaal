# Create a project

Create a regular file named `opaal.toml` and pass its exact path on every project command. OPAAL never activates a manifest by walking parent directories.

At minimum, give the project a name, a root module, a compatible toolchain range, contained paths, and an environment that selects authority and lock files:

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

Create both selected files even for a pure task. The authority file can have `rules = []`; the lock can have `tools = []`, `[child_environment]`, and `inherit = []`, but its `platform` must match the selected host. The [first-project lesson](../learn/first-project.md) gives working file contents and commands.

Add named tools, endpoints, or injected-secret identities only when the task needs them. Paths must be relative to and contained by the project root, and symlinks are refused. The [manifest reference](../reference/formats/opaal-toml.md) lists the closed fields.
