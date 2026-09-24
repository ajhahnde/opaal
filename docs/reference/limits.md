# Limits

OPAAL charges deterministic budgets for one source closure, evaluation, project operation, and artifact. The exact limit is admitted; the first excess refuses without treating a partial result as a completed program.

| Area | Ceiling |
| --- | --- |
| Source closure | 8 MiB source bytes; 256 modules; depth 64; 1,000,000 syntax nodes |
| Analysis | Type depth 64; 100,000 generic instantiations; 100,000 overload candidates; 1,024 diagnostics; 5,000,000 work units |
| Evaluation | 1,000,000 steps; call depth 256; 1,000,000 retained collection items; 16 MiB retained collection bytes |
| Project control | 1 MiB per TOML file; depth 16; 256 entries; 64 unique task inputs |
| Aggregate control reads | 192 MiB per check, plan, execute, or audit invocation |
| Operational files | 16 MiB per regular file; 32 MiB aggregate reads |
| TOML/JSON data | 16 MiB input or output; TOML depth 64 |
| Secrets | Eight identities per evaluation; 64 KiB each |
| HTTP | Eight requests; 128 headers; 64 KiB header bytes; 8 MiB per body; 30 seconds per request |
| Process | Eight attempts; one live child; 8 MiB each of stdout and stderr; ten minutes per attempt |
| 1.1 direct CLI process context | 1,000,000 native environment entries; 16 MiB combined native name and value bytes; 1 MiB searched `PATH`; 4,096 ordered searched `PATH` elements; 4,096 child starts per standalone run or interactive submission |
| Check and plan | 8 MiB per artifact; at most 1,024 entries; depth 64 |
| Journal | 16 MiB; 100,000 lines |
| Audit | 16 MiB |

The 192 MiB control-read ceiling includes manifest, source, file-snapshot inputs, authority, lock, CA material, tool executables, and accepted or audited artifacts. Smaller per-file and per-family limits still apply. A plan expires no more than 900 seconds after creation. Inspection output is bounded separately at 32 MiB.

The direct environment entry and byte limits govern the initial native
snapshot. Later `export` changes are not rechecked against those snapshot
limits; the host may refuse a child start if its environment is too large.
The `PATH` ceilings apply when a caret name needs a `PATH` search; a direct
name containing `/` bypasses that search.
The process row governs accepted project operations. Direct CLI child output
sent to inherited stdout or stderr streams is not retained by OPAAL and has no
OPAAL byte cap; captured carriers retain their existing resource bounds. Direct
CLI execution has no automatic time limit; its two-second signal grace applies
only after a supported interrupt or termination signal reaches the owned group.
The exact direct-context ceiling is admitted, and the first excess refuses
before the relevant host probe or child start. A caller's narrower request, such as `read(...,
max_bytes)` or an HTTP response limit, cannot raise the platform ceiling.
[Operational modules](std/README.md) state operation-specific consequences.
