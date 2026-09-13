# Common format conventions

Project control files are UTF-8 TOML with closed keys and `schema_version = 1`. Each is at most 1 MiB, with aggregate nesting at most 16 and at most 256 collection entries. Unknown keys, duplicate keys, malformed types, invalid identities, and unsupported versions refuse the document. The manifest, authority document, and tool lock each have their own exact fields.

Check and plan are canonical JSON objects without a trailing newline; audit is a canonical JSON object. The journal is canonical JSON Lines with a newline after each complete line. JSON object keys and ordered arrays follow the validator's canonical rules. Each object or journal-line `digest` uses `sha256:` and 64 lowercase hexadecimal digits over the canonical JSON object with its `digest` field omitted. The journal's terminating newline is not hashed into that field. Do not edit a sealed artifact and reuse its digest.

Native Unix paths and values in control artifacts use canonical unpadded base64url with an explicit platform tag where the field calls for native bytes. Project paths are relative and lexically contained, then opened under retained descriptors without following symlinks. This keeps a path's byte identity separate from its display form.

The four artifact schemas are `opaal.check.v2`, `opaal.plan.v2`, `opaal.run-journal.v2`, and `opaal.audit.v2`. Their `v2` suffixes identify file formats, not an OPAAL source-language version. Version-1, future, and unknown-field artifacts are rejected; regenerate with a compatible toolchain instead of editing them in place. See [limits](../limits.md) for per-artifact and aggregate budgets.
