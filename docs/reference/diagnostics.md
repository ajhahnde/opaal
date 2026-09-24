# Diagnostics

A diagnostic names the failing boundary and carries a stable code, message, and source location when a source span exists. Syntax, module, type, and action analysis fail before execution. CLI failures appear on standard error with a nonzero exit; `check --format json` also records ordered static findings in its canonical artifact. The language server projects semantic diagnostics onto the current document snapshot and discards stale results.

| Code family | What to inspect |
| --- | --- |
| Syntax and source codes | Tokens, grammar, incomplete input, and UTF-8 source. |
| `MOD`, `SIG` | Explicit module graph, visible names, call signature, or carrier mismatch. |
| `PROJECT` | Manifest selection, source closure, task identity, or project path. |
| `AUTH` | Closed authority file and exact effect/scope rows. |
| `CHECK` | Static task input, grant, or target-capability finding. |
| `PLAN` | Planning boundary, expiry, destination, or identity capture. |
| `EXECUTE`, `JOURNAL` | Acceptance, stale preflight, execution, or journal publication. |
| `AUDIT` | Journal validation or audit publication. |
| `PROCESS`, `HTTP` | Maintained adapter's bounded operation. |

`PLAN004` is the deliberate refusal for `opaal plan SOURCE`. `CHECK008` reports an unsupported process request against a macOS lock; `CHECK009` reports an unknown target. Neither means that a process was attempted. A denied authority row, unsupported host, malformed control file, and stale plan are different failures; address the named boundary rather than retrying with a broad grant.

In the direct CLI route, missing or non-executable candidates, excessive
`PATH`, and spawn failures are source-spanned errors. A preflight probe is
advisory: a file can change before spawn, and the spawn result determines the
failure. A child exit status is a `Status`, not a missing-program error.
Failure to acquire a complete native cwd/environment snapshot, including a
duplicate environment name, refuses before source evaluation. Snapshot
diagnostics do not print inherited values.

The [CLI reference](tooling/cli.md) explains output channels. [Authority](operational/authority.md), [formats](formats/README.md), and [limits](limits.md) give the corresponding contract.
