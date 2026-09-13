# Core commands

OPAAL registers 29 internal command names. The table records the registry's invocation text and carrier contract. `E` means empty input, `B` byte stream, `V` one structured value, and `S` value stream. A command listed here is recognized by analysis and help; its registration does not authorize an effectful source run.

| Command | Invocation | Input → output | Purpose |
| --- | --- | --- | --- |
| `cd` | `cd [PATH]` | E → E | Change the logical working directory. |
| `pwd` | `pwd` | E → V | Return the logical working directory. |
| `which` | `which NAME...` | E → S | Resolve names without running them. |
| `exit` | `exit [CODE]` | E → E | Request session termination. |
| `check` | `check` | E, B, V, S → same | Raise a catchable error unless the upstream stage succeeded. |
| `decode` | `decode CODEC` | B → S | Decode bytes into values. |
| `from` | `from FORMAT [MODE]` | B → S | Parse formatted bytes into values. |
| `encode` | `encode CODEC` | V, S → B | Encode values as bytes. |
| `to` | `to FORMAT` | V, S → B | Serialize values in a named format. |
| `first` | `first [COUNT]` | S → S | Keep the first values. |
| `last` | `last [COUNT]` | S → S | Keep the last values. |
| `collect` | `collect` | S → V | Materialize one list. |
| `length` | `length` | S → V | Count stream values. |
| `lines` | `lines` | S → S | Split textual values into lines. |
| `each` | `each { |VALUE| EXPRESSION }` | S → S | Apply a closure to each value. |
| `where` | `where { |VALUE| PREDICATE }` | S → S | Keep values accepted by a predicate. |
| `select` | `select FIELD...` | S → S | Project fields from records. |
| `get` | `get FIELD` | S → S | Extract one field per record. |
| `update` | `update FIELD VALUE` | S → S | Replace a record field. |
| `sort` | `sort [FIELD]` | S → S | Order values or records by field. |
| `ls` | `ls [PATH]` | E → S | List directory entries as records. |
| `open` | `open PATH` | E → B | Read file bytes. |
| `save` | `save PATH` | B → E | Write pipeline bytes. |
| `jobs` | `jobs` | E → S | List addressable background jobs. |
| `fg` | `fg [%JOB]` | E → E | Resume a job in the foreground. |
| `bg` | `bg [%JOB]` | E → E | Resume a stopped job in the background. |
| `wait` | `wait [%JOB...]` | E → E | Wait for one or all jobs. |
| `kill` | `kill [SIGNAL] %JOB...` | E → E | Signal an addressable job. |
| `help` | `help [NAME]` | E → B | Inspect built-in and visible function metadata without execution. |

`kill` accepts one signal flag before job operands: `--hangup`, `--interrupt`, `--terminate`, `--kill`, `--stop`, or `--continue`. The flags conflict with one another. The registry also validates positional counts and the closure argument for `each` and `where`.

A pipeline cannot pass a value stream to `save` or a byte stream to `collect` without an explicit conversion. `decode` and `from` cross from bytes to values; `encode` and `to` cross back. The [pipeline reference](commands-pipelines-and-streams.md) explains those carriers. `help NAME` reports registry or visible-function metadata without executing the named command. Host-dependent commands remain subject to the [authority](../operational/authority.md) and [platform](../platform-support.md) boundaries.
