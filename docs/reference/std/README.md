# Standard modules

Import a compiled module by name and give it a local alias: `import std::value as value`. Module identities do not come from a file search, package registry, or ambient prelude.

| Module | Exports |
| --- | --- |
| [`std::value`](value.md) | `length` |
| [`std::outcome`](outcome.md) | `Result`, `Option` |
| [`std::data`](data.md) | `toml_decode`, `get`, `json_encode` |
| [`std::path`](path.md) | `normalize`, `join`, `contained` |
| [`std::filesystem`](filesystem.md) | `read`, `write_atomic` |
| [`std::time`](time.md) | `Timestamp`, `wall_now`, `monotonic_now` |
| [`std::version`](version.md) | `Version`, `parse`, `render`, `matches` |
| [`std::integrity`](integrity.md) | `sha256` |
| [`std::url`](url.md) | `Url`, `parse`, `render` |
| [`std::http`](http.md) | `SecretHeader`, `HttpResponse`, `secret_header`, `request` |
| [`std::process`](process.md) | `ToolResult`, `run` |

The value and outcome modules work in pure source. Operational modules need a controlled host for their effects. Ordinary script and interactive execution refuse those effects before adapter access. An [accepted project plan](../operational/lifecycle.md) provides the source route; embedders can use an [explicit context](../embedding/operational-context.md).
