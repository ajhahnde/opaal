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
| Check and plan | 8 MiB per artifact; at most 1,024 entries; depth 64 |
| Journal | 16 MiB; 100,000 lines |
| Audit | 16 MiB |

The 192 MiB control-read ceiling includes manifest, source, file-snapshot inputs, authority, lock, CA material, tool executables, and accepted or audited artifacts. Smaller per-file and per-family limits still apply. A plan expires no more than 900 seconds after creation. Inspection output is bounded separately at 32 MiB.

A caller's narrower request, such as `read(..., max_bytes)` or an HTTP response limit, cannot raise the platform ceiling. [Operational modules](std/README.md) state operation-specific consequences.
