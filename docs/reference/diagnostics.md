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

The [CLI reference](tooling/cli.md) explains output channels. [Authority](operational/authority.md), [formats](formats/README.md), and [limits](limits.md) give the corresponding contract.
