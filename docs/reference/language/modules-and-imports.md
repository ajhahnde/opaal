# Modules and imports

Each file module is an explicit UTF-8 `.opaal` source file. A local import names a quoted path and requires an alias:

```opaal
import './model.opaal' as model
import std::value as value
export { model }
```

A compiled standard module uses a `std::name` identity. Aliases and re-exports preserve the canonical identity of the imported module. There is no ambient prelude, wildcard import, package discovery, or filesystem search path for an unqualified module name. A module graph is bounded in source size, module count, depth, and analysis work; a failed import does not become a host lookup.

An explicitly selected project adds the closed declarative namespaces `project::context`, `project::tools`, `project::endpoints`, and `project::secrets`. Importing one does not grant access to the host. The names are resolved against the selected manifest and later authority decision. Standalone source cannot import them or declare a task.

The [standard-module catalog](../std/README.md) lists compiled exports. [Projects and tasks](../operational/projects-and-tasks.md) explains when project identities exist.
