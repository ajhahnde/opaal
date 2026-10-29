# Standard input and output

These migrated offline references require OPAAL 1.9.0 or later and below 2.0.0.
Qualification uses the committed project version requirements unchanged.
The `generic-*.opaal` references additionally require the unreleased generic
printing implementation; published 1.9.0 rejects those calls. No first release
or new project minimum is assigned yet. Candidate qualification temporarily
substitutes `generic-tasks.opaal` into the existing project harness; the shipped
String-only project's minimum stays unchanged.

Extract the matching Linux x86_64 or macOS arm64 program archive and the
`standard-input-output` fixture archive into a fresh directory. Follow the
[installation guide](https://opaal-lang.org/download/) for checksum verification
and put that candidate's program directory on PATH. Enter this fixture directory:

```sh
opaal --version
opaal check source.opaal
opaal format --check source.opaal tasks.opaal
printf 'Grüße\n' | opaal source.opaal
```

Stdout is exactly the input UTF-8 bytes; stderr is `processed` followed by one
LF. Empty input succeeds with empty stdout and the same diagnostic. Invalid
UTF-8 or more than 4096 bytes raises an Error and emits no stdout. A read that
fails never returns a truncated value; consumed input cannot be recovered.

The native CLI binds inherited streams. In an interactive terminal, import
`std::io` and `std::string`, evaluate `let input = io::read_stdin(4096)`, enter
`Grüße` and press Enter, then Ctrl-D on the empty line. The read ends at that
EOF. Evaluate `string::decode_utf8(input)` and `io::println("ready")`; a later
read can accept fresh terminal input. When source cells arrive through a pipe,
the editor owns stdin and a language read refuses without consuming later cells.

## API and limits

| Call | Argument → result | Required effect |
| --- | --- | --- |
| `read_stdin` | `Int` → `Bytes` | `stdin.read` |
| `write_stdout` | `Bytes` → `Null` | `stdout.write` |
| `write_stderr` | `Bytes` → `Null` | `stderr.write` |
| `print`, `println` | `Any` → `Null` | `stdout.write` |
| `eprint`, `eprintln` | `Any` → `Null` | `stderr.write` |

Raw byte writers preserve Bytes, including NUL and non-UTF-8 bytes. Print helpers
preserve String UTF-8 bytes, including controls and NUL. Other ordinary data use
the existing display: numbers, Bool, Null, Bytes, native paths, Duration,
ByteSize, Range, Status, lists, records, ordinary nominal records, variants and
tables. Containers retain insertion order and quoted nested strings/bytes/paths;
Status keeps its existing summary at the top level and full nested display.
Bytes use escaped human text (`A\xFF`), never decoding or raw transfer. Tables
use data display, independent of terminal width. Use explicit serialization for
machine output. `println` and `eprintln` append exactly
one LF, even if the String already ends with LF. Successful writes return Null
only after every requested byte is confirmed.

Each call allows at most 1048576 bytes; LF counts toward that cap. `read_stdin`
requires a nonnegative cap and reserves cap+1 for the excess probe. A zero cap
still probes for EOF and consumes one byte if input exists. Calls share an
8388608-byte host budget with Random, plus evaluation work and retention limits.
Each operation has a 30-second deadline bounded by evaluation cancellation.
Print preparation uses one bounded buffer before any write, charges cumulative
work and scratch even on caught failure, and polls cancellation during traversal.
Root depth is 1; each nested value, type expression or status stage adds one,
with a maximum of 128. Depth/work/scratch exhaustion raises `RESOURCE_LIMIT`.
Escaped expansion and LF count toward the call limit. No preparation failure
emits a prefix. Error, Callable and opaque resource/secret/control records anywhere
raise `IO005` without invoking their formatters or revealing their fields.
Ordinary Strings containing sensitive text are still printed exactly.

`IO001` reports an invalid cap or oversized write, `IO002` excess input,
`IO003` Broken Pipe, `IO005` unsupported display data, and `IO004` a zero-byte
write. Resource exhaustion and cancellation retain their distinct outcomes.
An Error can be caught, but already consumed/emitted bytes stay consumed/emitted.
Unacknowledged transfers retain an uncertain upper bound; never replay them
automatically. An owned worker is closed and reaped on cancellation or protocol
failure. No general stream handles, prompts, capture, or new pipeline syntax
are added.

The generic references exercise these exact boundaries:

- [Report](generic-report.opaal): stdout is `42` plus LF, then
  `{"jobs": 42, "passed": true}` plus LF; stderr is `complete` plus LF.
- [Bytes](generic-bytes.opaal): with stdin bytes 0x41 0xFF, stdout contains
  `Grüße`, NUL, `already` and two LF, escaped `A\xFF` plus LF, then raw 0x41 0xFF.
- [Refusal and recovery](generic-refusal.opaal): a nested function raises `IO005`
  before output; catch writes `unsupported display value` plus LF to stderr,
  then stdout receives `42` plus LF.
- [Accepted report task](generic-tasks.opaal): stdout is the same ordered record
  plus LF and stderr is `complete` plus LF, with separate receipt/journal channels.

## Controlled task

Copy `tools-linux.toml` or `tools-macos.toml` to `tools.toml` for the current
host. The action declares all three effects in evaluation scope and the
environment grants them separately. Pure functions, callbacks and imported
initializers refuse; default embeddings provide no grant or bound endpoint.
Check and plan perform no transfer. From a fresh directory:

```sh
opaal check --project opaal.toml --task sample --environment ci --format json
opaal plan --project opaal.toml --task sample --environment ci --expires-in 900s --out sample.plan.json
```

Inspect the plan with `opaal plan inspect sample.plan.json` and use its exact
digest for `--accept`. Run the following with DIGEST set to that value:

```sh
printf 'Grüße\n' | opaal execute --plan sample.plan.json --accept "$DIGEST" --journal sample.run.jsonl --receipt-out sample.receipt.json
opaal audit --project opaal.toml --journal sample.run.jsonl --out sample.audit.json
opaal audit inspect sample.audit.json
```

Stream-output tasks require a fresh `--receipt-out` file. Existing files,
symlinks, journal/source/input aliases, and `--secret-stdin` conflicts refuse
before transfer. Keep source, input, stdout, stderr, receipt and journal channels
distinct. Controlled language stdout/stderr carry only requested data; inspect
the command exit, receipt and journal for administrative outcomes. After
transfer, a receipt or journal failure makes execution unsuccessful without
replaying output. A failed receipt may be empty, incomplete, or unsynced; do not
accept it as durable success. If both sinks fail, detailed evidence may be lost.

These tasks use v3 check/plan identities, v4 journal/audit identities and metadata-only evidence
for the whole run: no input/output, derived values, error text or payload digests.
Receipts use `opaal.execution-receipt.v2`, bind run ID and plan digest, and retain
primary/secondary outcomes and cumulative confirmed/uncertain counts for input,
both outputs and entropy. Old-effect v2 artifacts remain inspectable. New
effects in v2, mixed/future schemas and stale/corrupt identities fail closed.

The [CI job-check reference](ci-job-check/README.md) adds domain policy and a
successful-acquisition gate using the existing pure report core.
