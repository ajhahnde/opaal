# `std::filesystem`

The source module exports `read(target: Path, max_bytes: Int) -> Bytes` and `write_atomic(target: Path, bytes: Bytes) -> Null`. These are controlled operations, not pure file conveniences.

A read accepts an absolute path to a bounded regular file beneath the retained project root. Use `std::path::join(project::root, "file.txt")` to construct that path from a relative name; a relative `Path` input alone is refused. The adapter opens path components one at a time without following symbolic links and repeats physical containment before access. Reads have a 16 MiB per-file ceiling and a 32 MiB aggregate ceiling for one evaluation. The requested `max_bytes` can narrow the read, not enlarge those ceilings.

`write_atomic` creates a private sibling of the exact evidence target, syncs it, atomically replaces that evidence name, and syncs the retained parent directory. It is limited to the declared write scope; it is not a transaction over the rest of the task. A denied path, symlink, non-regular file, or exceeded bound fails closed. Project artifact publication has its own exclusive, no-overwrite rules in [formats](../formats/README.md).

An action must declare the corresponding `filesystem.read` or `filesystem.write` request. Its [authority](../operational/authority.md) must grant that exact request and the host must enforce it.
