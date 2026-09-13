# Projects and tasks

Select a project by passing the exact path to a regular file named `opaal.toml`. OPAAL does not search the working directory or its parents. The manifest names one root module, project paths, tool and endpoint declarations, injected-secret identities, and environments. A project environment selects one authority file and one tool lock; the CLI cannot restate those paths during check, plan, or execute.

A `task` declaration belongs only in the manifest-selected root module. It exports exactly one action, taking the action's parameters, result, and declared effects without overrides. Project loading rejects unknown tasks, invalid action graphs, unsupported task parameter types, and imports outside the allowed project source closure. Standalone source has no `task` declaration or `project::` namespace.

Use `opaal task inspect --project opaal.toml TASK` to see the shared signature, effects, tools, and environments without selecting authority. A task check adds an exact environment and bindings. Neither operation invokes the task.

The four project namespaces are declarative: `project::context`, `project::tools`, `project::endpoints`, and `project::secrets`. Importing one identifies a manifest entry; it does not grant a host effect. [`opaal.toml`](../formats/opaal-toml.md) owns the file shape, and [inputs and bindings](inputs-tools-endpoints-and-secrets.md) explains their checks.
