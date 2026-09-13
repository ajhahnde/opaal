# Tool lock

A tool lock belongs to one project environment. Its exact top-level keys are `schema_version`, `project`, `environment`, `platform`, `child_environment`, and `tools`. It uses version 1 and must match the selected manifest and environment.

`[child_environment]` has `inherit = []` and optional explicit `variables`; ambient inheritance is forbidden. A process-bearing run needs explicit values for `HOME`, `TMPDIR`, `PATH`, `CARGO_HOME`, `RUSTC`, `RUSTDOC`, `LC_ALL`, `TZ`, `CARGO_NET_OFFLINE`, `GIT_CONFIG_NOSYSTEM`, and `GIT_CONFIG_GLOBAL`. Native names and values use canonical unpadded base64url with `platform = "unix"`; values cannot contain NUL.

There is exactly one `[[tools]]` row per manifest tool. Each row binds its ID, adapter, absolute native executable path, canonical SemVer version, and SHA-256 digest. The lock's top-level `platform` applies to every row; a tool row has no `platform` field. A path may not contain NUL. Check validates the lock's shape without reading an executable or probing a version. Plan reads and binds the executable; execute revalidates its identity before any admitted probe or run.

A matching pathname alone is not sufficient. On Linux the maintained adapter starts the retained exact executable descriptor. macOS refuses process-bearing execution because that descriptor-backed start is unavailable. See [`std::process`](../std/process.md) and [common conventions](common-conventions.md).
