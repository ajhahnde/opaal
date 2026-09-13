# Inputs, tools, endpoints, and secrets

A task check binds at most 64 unique parameter names. `--input name=value` parses a declared lexical value without opening a file; a `Path` supplied this way need not exist. Controlled filesystem operations require an absolute target beneath the project root, so passing a relative `Path` input directly to one is refused. Construct a target from a string component with `std::path::join(project::root, child)` or supply an already absolute contained path. `--input-file name=PATH` is only for a declared `Path`: it opens one existing bounded regular file under the project root without following symlinks and records its native path, size, and content digest for stale revalidation.

Scalar input text accepts `String` as UTF-8, `Bool` as `true` or `false`, decimal `Int` and finite `Float`, `null`, unpadded canonical base64url `Bytes`, and integer `ns` or `b` suffixes for `Duration` and `ByteSize`. Types without an exact `name=value` representation cannot be exported as task parameters.

A manifest tool names only the `git` or `cargo` adapter and a version range. The environment's tool lock supplies the exact executable path, canonical version, digest, platform, and complete child environment. Check reads the lock but does not read an executable or run a version probe. Plan binds the executable identity; execute revalidates it.

An endpoint declares its HTTP or HTTPS URL, unique uppercase methods, ordered lowercase secret-header names, and TLS properties. TLS needs a declared lowercase DNS server name and project-local CA file. Plain HTTP is limited to literal loopback IPs. A secret declaration uses only `kind = "injected"`; the value arrives through a controlled context or, for one eligible task requirement, non-terminal `--secret-stdin` after preflight. It is never discovered from an environment variable or credential cache. Multiple secret IDs, repeated reveal, or multiple sinks produce a valid but refused check and plan.

The [manifest](../formats/opaal-toml.md), [tool lock](../formats/tool-lock.md), and [authority](authority.md) pages give the exact file and grant rules.
