# Flash Scripting

[FlashOS](../../../README.md) › [Flash](../README.md) › [Documentation](README.md) › Scripting

This guide explains how to run and inspect `.fsh` programs, pass script arguments, use non-executing checks and canonical formatting, invoke external processes, connect pipeline stages, redirect file descriptors, handle command statuses, and manage background jobs. Language syntax, values, bindings, expressions, modules, function metadata, and structured-data operations are documented in the [Language Guide](language-guide.md).

> **Project status:** FlashOS as a complete operating system remains pre-alpha software. However, this Flash Scripting Guide defines the intended stable Flash v1.0 contract for scripting and execution. Note that not every v1 feature is automatically available in every current FlashOS image or on every target platform, and successful execution on a Linux or macOS development host is not automatic proof of FlashOS target support.

## On this page

- [Running scripts](#running-scripts)
- [Script arguments](#script-arguments)
- [Script and interactive execution](#script-and-interactive-execution)
- [Checking and formatting without execution](#checking-and-formatting-without-execution)
- [Session state](#session-state)
- [Invoking commands](#invoking-commands)
- [Command substitution](#command-substitution)
- [Pipelines](#pipelines)
- [Redirections](#redirections)
- [Statuses and failures](#statuses-and-failures)
- [Background execution](#background-execution)
- [Job commands](#job-commands)
- [Interactive job control](#interactive-job-control)
- [Portability and operational boundaries](#portability-and-operational-boundaries)

## Running scripts

The Flash executable is named `fsh`. Run a UTF-8 source file by passing its path:

```bash
fsh program.fsh
```

A relative script path is resolved by the process that starts `fsh`. Commands inside the script begin with the caller's current working directory and inherited environment.

Use `--` when a script path begins with a hyphen:

```bash
fsh -- --maintenance.fsh
```

The standard informational modes are:

```bash
fsh --help
fsh --version
```

Flash reports command-line invocation errors before opening a script. Before
execution, it canonicalizes the root and every static `import '<path>'`, reads
and parses each canonical source once, and rejects load, UTF-8, syntax, or
cycle failures. Diagnostics use the applicable source file and group excerpts
when an import cycle spans multiple files. Only the root source executes;
load-only imported modules do not run initialization or bind names.

### Source files

Flash scripts conventionally use the `.fsh` extension. Source files:

- must contain UTF-8 text;
- may use LF or CRLF line endings;
- use the same grammar and evaluator as interactive submissions;
- are parsed as Flash rather than POSIX shell source.

A `.fsh` file is not a Bash or `sh` program. Do not use POSIX-specific syntax unless an external POSIX shell is invoked explicitly.

## Script arguments

Flash v1 exposes arguments supplied to a `.fsh` program through its script-argument interface. The interface preserves argument order and cardinality so that an empty argument remains one argument and multiple arguments do not collapse into one string.

Script arguments are data. They are not reparsed as Flash source, do not undergo implicit whitespace splitting, and do not trigger implicit wildcard expansion. A script must request any later parsing, conversion, or explicit collection expansion itself.

The `fsh` command-line parser distinguishes shell options, the script path, and the arguments belonging to that script. An option terminator may be used where an operand could otherwise be interpreted as an `fsh` option. Concrete argument-access syntax follows the language grammar and must not be inferred from POSIX-shell conventions.

## Script and interactive execution

Interactive input and scripts share the Flash parser, value model, command planner, and evaluator. The surrounding session behavior is intentionally different.

| Behavior                                  | Interactive session  | Script execution          |
| ----------------------------------------- | -------------------- | ------------------------- |
| Startup command                           | `fsh`                | `fsh program.fsh`         |
| Language parser and evaluator             | Shared               | Shared                    |
| Line editor and prompts                   | Enabled              | Not used                  |
| Persistent interactive history            | Optional             | Not used                  |
| Interactive startup configuration         | Considered once      | Never loaded              |
| Recover after a source/runtime diagnostic | Return to the prompt | End the script            |
| Background-job completion                 | Prompt-safe notices  | Joined before script exit |

Interactive configuration cannot silently change the meaning of an automation script. A script starts from deterministic session defaults, its inherited environment, and its initial working directory.

Running:

```bash
fsh --no-config
```

starts an interactive session without loading its startup configuration. The `--no-history` option similarly disables interactive history for that session. These policies do not turn script execution into an interactive session.

## Checking and formatting without execution

Flash v1 provides inspection modes for validating and normalizing source without running the program.

### Static checking

The `fsh check` mode parses supplied source and performs the available module, name, signature, and pipeline-compatibility analysis without executing script statements. It must not start external processes, apply redirections, change the working directory, mutate the caller's environment, or run imported module initialization as a substitute for analysis.

A successful check means that the supported non-executing analyses completed without diagnostics classified as errors. It does not prove that external programs exist, that a target platform provides every requested capability, or that runtime data will satisfy conditions that cannot be established statically.

Checker diagnostics and process status are intended for local development, editor integration, and CI use.

### Canonical formatting

The Flash formatter provides a check mode and a write mode. Check mode reports source that differs from canonical formatting without rewriting it. Write mode rewrites source to the canonical representation.

Formatting must be idempotent: formatting already formatted source must not produce another change. The formatted result must preserve the parsed program structure and must remain valid Flash source.

The exact command-line spelling of formatter options belongs to the `fsh` CLI contract and must match the executable's help and tests; it must not be inferred from another formatter.

## Session state

A script owns session-local state that persists between its statements:

- lexical bindings and functions;
- a logical working directory;
- the environment inherited by child processes;
- session options;
- the most recent normally completed command status;
- background jobs started by the script.

### Working directory

Use `cd` to change the logical working directory:

```text
cd workspace
```

Later external commands and relative filesystem operations use the updated directory.

With no argument, `cd` uses the inherited `HOME` environment entry:

```text
cd
```

Use `pwd` to produce the current directory as a structured `Path` value:

```text
pwd
```

A successful directory change updates the child environment entries `PWD` and `OLDPWD`.

### Child environment

Lexical bindings and child-process environment entries are separate.

```text
let mode = "release"
export BUILD_MODE = $mode
```

The lexical binding `$mode` is available to Flash evaluation. The exported `BUILD_MODE` entry is included in the environment of external processes started later.

Remove an environment entry with `unset`:

```text
unset BUILD_MODE
```

Environment changes belong to the running Flash session. They do not modify the environment of the process that started `fsh`.

## Invoking commands

A bare command name is resolved in this order:

1. a registered Flash internal command;
2. an external executable.

```text
pwd
ls
```

In this example, `pwd` and `ls` select the Flash internal commands when those commands are registered.

### Forcing external execution

Prefix a command with `^` to bypass the internal-command registry:

```text
^ls -la
```

The caret is Flash syntax and is not included in the external process argument vector.

Use `command` when the executable name is selected at runtime:

```text
let program = "compiler"
let arguments = ["--mode", "release"]

command $program ...$arguments
```

`command` also bypasses the internal registry.

### Direct process launch

Flash starts external executables directly. It constructs a native executable path, argument vector, environment, working directory, and descriptor map without passing rendered command text through `/bin/sh`.

Consequently:

- spaces inside one argument remain inside that argument;
- pipeline and redirection characters produced by interpolation remain data;
- an interpolated string cannot introduce another command;
- external commands receive exactly the arguments produced by Flash expansion.

Running another shell is always explicit:

```text
^sh -c "external shell source"
```

The quoted source in this example is interpreted by `sh`, not by Flash.

### Executable lookup

A command name containing `/` is treated as a path and is not searched through `PATH`:

```text
^./tools/check
^/usr/bin/example
```

A bare external name is searched through the inherited `PATH` entries in source order:

```text
^example
```

Empty `PATH` entries do not mean the current working directory. Use an explicit relative path such as `./example` when that behavior is intended.

Failure to locate an executable is a runtime error. Flash does not manufacture a successful process launch with a synthetic exit status such as `127`.

### Argument cardinality

Each ordinary command word produces exactly one argument:

```text
let label = "two words"
command "example" $label
```

The value of `$label` remains one argument.

Expand a list into separate arguments with explicit spread syntax:

```text
let flags = ["--verbose", "--check"]
command "example" ...$flags
```

An empty list contributes no arguments. An empty string contributes one empty argument.

Wildcard characters are literal in ordinary words:

```text
command "example" "*.fsh"
```

Use `glob` and spread the resulting list when filesystem matching is required:

```text
let scripts = glob("scripts/**/*.fsh")
command "example" ...$scripts
```

For the complete expansion rules and eligible argument value types, see [Commands and argument expansion](language-guide.md#commands-and-argument-expansion).

## Command substitution

Command substitution captures standard output as one string:

```text
let directory = "$(pwd)"
```

The `$(...)` form evaluates one foreground command or conditional chain. It does not start an intermediate shell.

Successful text capture:

- reads standard output only;
- requires valid UTF-8;
- removes trailing LF and CRLF line-ending sequences;
- preserves other whitespace and newlines;
- produces exactly one `String`;
- never reparses or splits the captured text.

An empty capture therefore produces one empty string rather than no value.

Standard error remains inherited unless the source redirects it:

```text
let version = "$(^program --version 2> version-errors.log)"
```

A nonzero command exit still produces captured output paired with its actual status. It does not become a runtime error merely because the command was unsuccessful.

Capture is bounded by the session capture limit. If the output exceeds that limit, Flash continues draining and reaping the started processes before returning a capture-limit runtime error. This prevents the memory bound from causing a pipe deadlock.

Invalid UTF-8 similarly produces a runtime error rather than silently replacing bytes. Use byte-oriented pipelines and explicit decoding when arbitrary binary output is expected.

## Pipelines

The `|` operator connects one stage to the next:

```text
^producer | ^consumer
```

External stages exchange byte streams through operating-system pipes. Flash starts the required stages before waiting for their completion, allowing producers and consumers to run concurrently.

### Standard error pipelines

Use `|&` to merge a stage's standard output and standard error into one byte stream:

```text
^program |& ^consumer
```

The merged stream is not tagged. Its byte order is the order produced by the underlying writes.

`|&` requires a byte-oriented stage whose standard output and standard error can be redirected. Structured runtime values are not implicitly encoded or merged into a byte stream.

### Structured stages

Internal commands may exchange structured carriers:

- one `Value`;
- a lazy `ValueStream`;
- a `ByteStream`;
- no pipeline payload.

For example:

```text
ls
    | where {|entry| $entry.type == "file"}
    | select name size
    | sort name
```

The structured records in this pipeline are not converted to terminal-formatted text between stages.

### Explicit representation changes

Cross representation boundaries explicitly:

```text
open users.json
    | from json
    | where {|user| $user.active}
    | select name email
    | to json
    | save active-users.json
```

Common conversion families are:

| Command   | Boundary                                   |
| --------- | ------------------------------------------ |
| `decode`  | Byte stream to textual values              |
| `encode`  | Textual values to bytes                    |
| `from`    | Serialized bytes to structured values      |
| `to`      | Structured values to serialized bytes      |
| `collect` | Lazy value stream to one materialized list |

Flash rejects incompatible pipeline edges rather than guessing a conversion.

Interactive rendering of a record, list, or table is for human inspection. It is not a stable serialization format. Use an explicit encoder or formatter when a file or external process requires bytes.

### Pipeline completion

Every normally completed stage has its own status. A multi-stage pipeline additionally produces an aggregate status whose stage list remains in source order.

By default, the last stage selects the aggregate result. When the session's `pipefail` option is enabled, the rightmost unsuccessful stage selects it instead. `pipefail` changes status selection only; it does not stop, reorder, or serialize pipeline stages.

All required stages are reaped before a completed aggregate status is returned.

## Redirections

Flash supports source-ordered descriptor redirections.

| Form       | Meaning                                        |
| ---------- | ---------------------------------------------- |
| `< file`   | Open `file` for standard input                 |
| `> file`   | Create or truncate `file` for standard output  |
| `>> file`  | Create or append to `file` for standard output |
| `n< file`  | Open `file` for descriptor `n`                 |
| `n> file`  | Create or truncate `file` for descriptor `n`   |
| `n>> file` | Create or append to `file` for descriptor `n`  |
| `n>&m`     | Duplicate the current descriptor `m` onto `n`  |
| `n>&-`     | Close descriptor `n`                           |

Examples:

```text
^program < input.dat > output.dat
^program > output.log 2> error.log
^program >> combined.log 2>&1
```

A descriptor number must be adjacent to the operator. These forms are different:

```text
^program 2> error.log
^program 2 > output.log
```

The first redirects descriptor 2. The second passes `2` as an argument and redirects descriptor 1.

### Source order matters

Redirections are applied from left to right:

```text
^program > combined.log 2>&1
```

Both standard output and standard error end at `combined.log`.

```text
^program 2>&1 > output.log
```

Standard error keeps the destination that standard output had before the later `>` action. Only standard output ends at `output.log`.

Descriptor duplication refers to the mapping that exists at that exact point. It is not deferred until process launch.

### Pipelines and local redirections

Pipeline descriptor assignments are installed before a stage's local redirections. A local redirection can therefore replace a pipeline endpoint:

```text
^program 2> errors.log |& ^consumer
```

Only the program's standard output reaches `consumer`.

```text
^program > output.log |& ^consumer
```

Only standard error reaches `consumer`.

```text
^producer | ^consumer < input.dat
```

The consumer reads `input.dat` instead of the pipe.

An upstream process whose pipe no longer has a reader retains its real completion result, including a possible broken-pipe signal. This is not reported as a redirection setup failure.

### Redirect targets

A redirect target uses the same one-word expansion rule as a normal command argument:

```text
let output = "build output.log"
^program > $output
```

The value remains one path. It is not split at the space.

Redirect targets do not support implicit wildcard expansion or list spreading.

### Preparation and failure

Flash completes expansion and execution preflight before it opens redirect targets or starts pipeline stages. Once file actions begin, however, they are not a filesystem transaction.

For example, an earlier `>` action may create or truncate a file even when a later redirection for the same stage fails. Flash closes resources and cancels or reaps already started sibling stages, but it does not pretend to roll back completed filesystem effects.

A failed open, descriptor duplication, descriptor assignment, or descriptor close is a runtime error. The affected command is not represented as a normal nonzero process status.

Flash does not implicitly provide POSIX here-documents, here-strings, `&>`, or a `noclobber` mode through these operators.

## Statuses and failures

Flash distinguishes normal unsuccessful completion from structural failure.

| Outcome | Meaning | Selects <code>&#124;&#124;</code>? |
|---|---|---|
| Successful `Status` | Command completed successfully | No |
| Unsuccessful `Status` | Command completed with a nonzero code or signal | Yes |
| Runtime error | Evaluation, planning, resolution, I/O, or platform operation failed | No |
| Cancellation | Evaluation was interrupted or cancelled | No |
| Parse failure | Source could not be executed | Not evaluated |

### Conditional execution

Use `&&` to continue after success:

```text
^build && ^deploy
```

Use `||` to continue after an unsuccessful status:

```text
^primary || ^fallback
```

`&&` binds more tightly than `||`:

```text
^build && ^deploy || ^report-failure
```

The fallback in this example runs when either the build is unsuccessful or the deployment is unsuccessful.

A skipped branch is not expanded or planned. Its command substitutions, redirects, file opens, and process launches do not occur.

Runtime errors and cancellation abort the chain. They are not treated as false statuses and do not activate `||`.

### Status conditions

A command or pipeline can be used as an `if` or `while` condition:

```text
if ^program --probe {
    ^program --run
} else {
    ^fallback
}
```

A successful status acts as true. A nonzero exit or signal status acts as false.

Pure expression conditions must evaluate to `Bool`. Flash does not treat zero, an empty string, `null`, or an empty collection as implicitly false.

### Converting failure into an error

Use `check` when an unsuccessful status must propagate as a runtime error:

```text
^build | check
```

`check`:

- requires an upstream stage;
- accepts any pipeline carrier;
- forwards the carrier unchanged;
- inspects the completed upstream status;
- produces a structured `UnsuccessfulStatus` error when that status is unsuccessful.

It does not convert parse failures, command-resolution failures, redirection failures, decoding errors, cancellation, or other runtime errors. Those outcomes already propagate through their own error paths.

Output delivered before a streaming failure or unsuccessful checked completion remains observable.

### Explicit script exit

Use `exit` to request script termination:

```text
exit
exit 2
```

An explicit code must be an ASCII decimal value from `0` through `255`.

Without an argument, `exit` uses the current representable command code. It uses zero when no status exists and one when the current completion cannot be represented as an ordinary exit code.

The session boundary performs required background-job cleanup before the `fsh` process exits.

## Background execution

A trailing `&` backgrounds the complete conditional chain:

```text
^long-running-program &
```

```text
^prepare && ^process || ^report-failure &
```

The launch returns immediately with a successful launch status once the job has been published. Its later completion does not asynchronously replace the current foreground status.

Each background chain receives one stable, nonzero Flash job identity. A language job identity is distinct from operating-system process and process-group identifiers.

### Redirect background output

A background job can write while later script statements are running. Redirect output when interleaving would be undesirable:

```text
^long-running-program > task.log 2>&1 &
```

Background jobs do not receive foreground terminal ownership. Programs that require interactive terminal input should normally remain foreground jobs.

### Script lifetime

A script does not orphan jobs that it started. Before the script ends, Flash joins its remaining background jobs on every exit path, including:

- normal end of source;
- an explicit `exit`;
- a runtime failure.

A stopped background job is continued before the script waits for it. Flash then waits for all of its members to reach terminal completion.

When every joined job succeeds, the script retains its foreground result. When a background job fails, that failure participates in the final script exit result; the first failing job in job-identity order is selected.

The join does not impose an automatic timeout or destructive escalation. A program that repeatedly stops or refuses to terminate can therefore keep script shutdown open. Termination should be requested explicitly when the script owns such a process.

## Job commands

Flash provides five internal commands for addressable jobs:

| Command | Purpose                                           |
| ------- | ------------------------------------------------- |
| `jobs`  | Produce a structured snapshot of addressable jobs |
| `fg`    | Move an eligible job to the foreground            |
| `bg`    | Continue a stopped job in the background          |
| `wait`  | Wait for selected jobs                            |
| `kill`  | Send a selected signal to job process groups      |

A job reference has the exact form `%n`, where `n` is a nonzero decimal Flash job identity:

```text
%1
%12
```

Bare numbers, `%+`, `%-`, signs, and suffixed forms are not aliases.

### Inspecting jobs

`jobs` accepts no arguments and produces one structured record per addressable job:

```text
jobs
```

Records include stable fields for:

- `job`;
- `state`;
- `placement`;
- `group`;
- `command`;
- `status`;
- `signal`.

Because `jobs` produces a `ValueStream`, it can participate in a structured pipeline:

```text
jobs
    | where {|job| $job.state == "stopped"}
    | select job command signal
```

Reading the snapshot does not resume jobs, acknowledge pending notices, or remove completed records.

### Waiting

With no arguments, `wait` waits for all addressable non-failed jobs selected at invocation time:

```text
wait
```

Wait for specific jobs by identity:

```text
wait %2 %5
```

Explicit targets are processed in source order. A stopped selected job is continued in background placement before waiting.

The command returns the first unsuccessful selected aggregate status, or the last selected status when all selected jobs succeed.

### Continuing a stopped job

Continue the newest stopped job:

```text
bg
```

Continue one specific job:

```text
bg %3
```

`bg` does not transfer the terminal to the job.

### Foregrounding a job

Move the newest eligible stopped or running background job to the foreground:

```text
fg
```

Select a specific job:

```text
fg %3
```

`fg` requires a real foreground terminal. The shell transfers terminal ownership to the process group and restores it before the next prompt or session action.

This command is primarily useful in an interactive session. Non-interactive automation should generally use `wait`, redirection, and explicit signal delivery instead.

### Sending signals

`kill` requires at least one explicit target:

```text
kill %3
```

The default signal is termination. A selector may choose another supported group-directed signal:

```text
kill --hangup %3
kill --interrupt %3
kill --terminate %3
kill --kill %3
kill --stop %3
kill --continue %3
```

Multiple targets are processed in source order:

```text
kill --terminate %2 %4
```

Destructive escalation is explicit. Flash does not silently replace a failed graceful request with `--kill`.

## Interactive job control

Running `fsh` without a script starts an interactive session.

On a terminal with job-control capabilities:

- one external pipeline runs in one operating-system process group;
- a foreground job owns the terminal while it runs;
- Flash restores terminal ownership before drawing the next prompt;
- terminal-generated interrupts target the foreground process group;
- background jobs remain outside foreground terminal ownership;
- stopped and completed job notices are displayed at prompt-safe boundaries.

A top-level foreground chain consisting of exactly one all-external pipeline can be retained as an addressable stopped job when suspended from the terminal. It can later be inspected with `jobs`, continued with `bg`, or resumed with `fg`.

More complex execution shapes, including mixed internal/external pipelines and longer conditional chains, do not claim identical suspend-and-retain behavior. Flash preserves cleanup and terminal ownership rather than pretending that every internal execution island can be suspended like an external process group.

### Leaving an interactive session

When live jobs exist, the first interactive exit attempt is refused and lists those jobs. A second consecutive attempt proceeds by continuing stopped jobs, sending hang-up to their process groups, and waiting for completion.

Submitting another command between the two attempts resets the refusal. This ensures that the warning always describes the current job table.

No implicit deadline or destructive fallback is applied during interactive shutdown. A process that ignores hang-up can delay exit; use an explicit `kill --kill %n` only when destructive termination is intended.

### Interactive input control

In the interactive editor:

- `Ctrl-C` cancels the current edit buffer and starts a fresh prompt;
- `Ctrl-D` on an empty buffer requests end of input;
- parse and runtime diagnostics return control to the same session;
- lexical scope, environment, working directory, options, and current status survive recoverable diagnostics.

These editor behaviors do not apply to non-interactive script input.

## Portability and operational boundaries

### Language versus platform behavior

Flash language parsing, expansion, values, control flow, and status rules are platform-independent contracts. Process execution, files, native paths, signals, process groups, terminal ownership, clocks, and configuration directories cross an explicit platform boundary.

A target may support ordinary foreground execution while lacking a more advanced capability such as terminal ownership or process groups. Flash should report or deliberately degrade the affected capability rather than silently claim full job-control behavior.

### Native paths and arguments

Flash source is UTF-8, but operating-system paths, environment values, and external argument units are preserved through the native platform representation where supported.

A value containing a null byte cannot cross an external process, environment, or filesystem boundary that rejects it. Such conversion failures are runtime errors rather than string truncation.

### External program availability

A script should not assume that every development-host utility is installed in FlashOS or on another host.

Prefer:

- Flash internal commands for behavior owned by the language;
- explicit executable paths when the deployment layout guarantees them;
- `which` when a script needs to inspect command resolution;
- clear failure handling for optional external tools.

Successful execution on macOS or Linux does not by itself establish that the same external executable or platform capability exists in a FlashOS image.

### No implicit shell compatibility

Flash deliberately does not provide:

- POSIX source compatibility;
- implicit whitespace splitting;
- implicit wildcard expansion;
- aliases that rewrite source tokens;
- an `eval` operation that reparses strings as code;
- automatic execution of project-local startup files;
- hidden routing through a host default shell.

These boundaries are part of the scripting model rather than optional safety modes.

## Related documentation

- [Language Guide](language-guide.md) — Syntax, values, bindings, expressions, commands, expansion, functions, function metadata, modules, and structured pipelines.
- [Architecture](architecture.md) — Parser, runtime, platform, process, and CLI boundaries.
- [Development](development.md) — Build, test, lint, fuzz, checker, formatter, language-server gates, and verification procedures for the Flash workspace.
- [Flash overview](../README.md) — Component role, design boundaries, and integration with FlashOS.
- [FlashOS Getting Started](../../../docs/getting-started.md) — Build and boot a FlashOS image.
- [FlashOS Verification](../../../docs/verification.md) — Distinguish host checks, target builds, image validation, and runtime evidence.

---

[← Previous: Language Guide](language-guide.md) · [Flash documentation](README.md) · [Next: Architecture →](architecture.md)
