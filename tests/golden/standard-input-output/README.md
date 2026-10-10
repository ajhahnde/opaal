# Standard input and output

These offline references require the standard-stream APIs in OPAAL 1.3.0 or
later. The 1.3.0 candidate uses the committed project version requirements
without adjustment. Released 1.2.0 binaries do not implement these calls.

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
| `print`, `println` | `String` → `Null` | `stdout.write` |
| `eprint`, `eprintln` | `String` → `Null` | `stderr.write` |

Bytes are written unchanged, including NUL and non-UTF-8 bytes. Text is encoded
as UTF-8 without coercion or formatting. `println` and `eprintln` append exactly
one LF, even if the String already ends with LF. Successful writes return Null
only after every requested byte is confirmed.

Each call allows at most 1048576 bytes; LF counts toward that cap. `read_stdin`
requires a nonnegative cap and reserves cap+1 for the excess probe. A zero cap
still probes for EOF and consumes one byte if input exists. Calls share an
8388608-byte host budget with Random, plus evaluation work and retention limits.
Each operation has a 30-second deadline bounded by evaluation cancellation.

`IO001` reports an invalid cap or oversized write, `IO002` excess input,
`IO003` Broken Pipe, and `IO004` a zero-byte
write. Resource exhaustion and cancellation retain their distinct outcomes.
An Error can be caught, but already consumed/emitted bytes stay consumed/emitted.
Unacknowledged transfers retain an uncertain upper bound; never replay them
automatically. An owned worker is closed and reaped on cancellation or protocol
failure. No general stream handles, prompts, capture, or new pipeline syntax
are added.

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

These tasks use v3 check/plan/journal/audit identities and metadata-only evidence
for the whole run: no input/output, derived values, error text or payload digests.
Receipts use `opaal.execution-receipt.v1`, bind run ID and plan digest, and retain
primary/secondary outcomes and cumulative confirmed/uncertain counts for input,
both outputs and entropy. Old-effect v2 artifacts remain inspectable. New
effects in v2, mixed/future schemas and stale/corrupt identities fail closed.

The [CI job-check reference](ci-job-check/README.md) adds domain policy and a
successful-acquisition gate using the existing pure report core.
