# Flash Scripting

[FlashOS](../../../README.md) › [Flash](../README.md) › [Documentation](README.md) › Scripting

This guide explains how to run and inspect `.fsh` programs, pass script arguments, use non-executing checks and canonical formatting, invoke external processes, connect pipeline stages, redirect file descriptors, handle command statuses and structured errors, and manage background jobs. Language syntax, values, bindings, expressions, modules, function metadata, and structured-data operations are documented in the [Language Guide](language-guide.md).

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

`fsh --help` describes the launcher and script invocation. Inside Flash, the
separate language command `help [NAME]` inspects standard built-ins and visible
named functions. It is not a top-level `fsh help` mode.

Flash reports command-line invocation errors before opening a script. Before
execution, it canonicalizes the root and every static `import '<path>'`, reads
and parses each canonical source once, and rejects load, UTF-8, syntax, cycle,
module-name, or lexical-reference failures. Lexical resolution covers every
loaded source without executing it, so an invalid dormant load-only dependency
also stops program construction. Diagnostics use the applicable source file and
group excerpts when a failure spans multiple files. Dependencies reached
through named imports initialize once in deterministic dependency-first order
before the root. Their explicit exports become immutable snapshot bindings in
the importer. A valid source reached only through a load-only import does not
run initialization or bind names.

Named initializers otherwise run as ordinary Flash code. They share the
program's logical cwd, child environment, current status, output routing, and
background-job coordinator in dependency-first order. Successful `cd`,
`export`, and `unset` effects are visible to later initializers and the root;
normal completion and initializer `exit` commit the final child environment to
the caller. Runtime or required-output failure does not commit that environment.

Output, filesystem writes, and process activity are immediate external effects,
not a whole-program transaction. A later failure cannot retract bytes, restore
a truncated file, or unspawn work. The runtime stops later initializers and the
root, joins every program-owned background job, and retains ordered background
failure evidence. `exit` inside an initializer is whole-program control: it
skips that module's export materialization and all later execution, joins jobs,
and follows normal completion rules, including background-failure precedence.
See [Initializer effects](language-guide.md#initializer-effects) for the
complete per-class contract.

### Source files

Flash scripts conventionally use the `.fsh` extension. Source files:

- must contain UTF-8 text;
- may use LF or CRLF line endings;
- use the same grammar and evaluator as interactive submissions;
- are parsed as Flash rather than POSIX shell source.

A `.fsh` file is not a Bash or `sh` program. Do not use POSIX-specific syntax unless an external POSIX shell is invoked explicitly.

## Script arguments

Invoke a script with zero or more arguments after its path:

```bash
fsh program.fsh first "" --mode
```

The first non-option operand is the script path. Every following operand belongs
to the script, including `--` and option-like values such as `--mode`. A leading
`--` ends `fsh` option parsing and makes the next operand the script path:

```bash
fsh -- --maintenance.fsh first
```

The root module receives the arguments as an immutable `args: List[String]`
binding:

```text
let first = $args[0]
let second = $args[1]
```

The script path and process argument zero are not included. Order and
cardinality are exact: the empty operand above remains one empty `String`.
Arguments must be valid UTF-8; an invalid operand is rejected before source
loading without lossy replacement.

`args` is available only to the root module through an implicit parent scope.
Imported and load-only modules do not receive it. A root declaration or named
import may shadow `args` through ordinary source-order scope rules.

Script arguments are data. They are not reparsed as Flash source, do not undergo implicit whitespace splitting, and do not trigger implicit wildcard expansion. A script must request any later parsing, conversion, or explicit collection expansion itself.

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

### Interactive startup settings

The interactive config is ordinary Flash source. During its isolated startup
transaction, six mutable settings are available:

```text
$pipefail = true
$capture_limit = 1048576
$completion = true
$history = false
$prompt = 'flash> '
$continuation_prompt = 'more> '
```

`$pipefail`, `$completion`, and `$history` require `Bool` values.
`$capture_limit` requires a nonnegative `Int` that fits the host byte-count
range; zero is valid. `$prompt` and `$continuation_prompt` require `String`
values without terminal control characters. These six bindings exist only
while config is evaluated and are not visible to scripts or at the interactive
prompt. A successful config commits the resulting session options, prompts,
and editor settings together with its ordinary bindings and staged environment.
A parse, evaluation, setting, or startup-policy failure discards all of them
and enters visible safe mode with the fixed `[SAFE] >> ` primary prompt and
clean defaults.

`--no-config` wins before config discovery, and `--no-history` wins over a
config request to enable history. No interactive config or setting is loaded by
script, command, checker, formatter, help, or version modes.

Completion candidates refresh before every prompt from the committed command
registry, visible lexical scope, logical working directory, child `PATH`, and
the executable and recursively discovered path entries visible in bounded host
snapshots. This makes config functions, later definitions, cwd changes,
exports, and `PATH` changes visible without performing environment or
filesystem I/O during a Tab keypress. Path completion uses Flash token spans
for bare, single-quoted, double-quoted, interpolated, argument, redirection,
executable, and `glob(...)` positions. It inserts reversible escapes, preserves
wildcard pattern spelling, and never expands or executes the submitted source.
Native names that cannot be represented exactly in the UTF-8 source buffer are
omitted rather than converted lossily.

## Checking and formatting without execution

Flash v1 provides inspection modes for validating and normalizing source without running the program.

The currently available runtime inspection surface is:

```text
help
help NAME
```

It accepts zero or one static name, produces UTF-8 bytes, and uses ordinary
capture, pipeline, and redirection behavior. Lookup is resolved from immutable
metadata during planning; it never calls the documented function or queries an
external executable. Dynamic query forms such as `help $name`, substitutions,
spreads, and closures are rejected rather than evaluated as discovery logic.

### Static checking

Check one root program with:

```bash
fsh check program.fsh
fsh check -- --maintenance.fsh
fsh check --help
```

The checker accepts exactly one native source path and no script arguments.
Exact first-operand `check` selects this launcher mode; `fsh ./check` and
`fsh -- check` still treat `check` as a script path. Stdin, multiple explicit
roots, globs, directory walks, implicit extensions, configuration, and ambient
module search are not supported.

The root and every recursively reachable static import are canonicalized,
loaded, and parsed. Canonical aliases share one source identity, so a symlink to
a regular source is accepted and read once. Roots and imports must resolve to
finite regular files containing UTF-8 source; directories and special files
are rejected.

Checking reports syntax and module-graph failures, name and export/import
failures, assignment mutability and known-type failures, named-function
annotation and known-call signature failures, shared built-in argument-schema
failures, and statically knowable pipeline-carrier failures. Diagnostics use
retained source spans and deterministic phase order: discovery and graph
issues, then name issues, then signature issues, then command and carrier
issues. Within a phase, sources follow canonical first-visit depth-first order
and constructs follow source order. A broken discovery graph suppresses name
and signature analysis; name failures suppress signature analysis. Command and
pipeline analysis still visit every successfully parsed source because they do
not depend on those phases.

Bare command names absent from the built-in registry and explicitly forced
external commands are classified as byte-stream stages without checking
`PATH`. An exact built-in uses its shared runtime carrier and argument contract,
including positional arity and kinds, option arity and conflicts, `--` policy,
and dynamic-tail policy. Help, completion, hover, signature help, planning, and
runtime validation consume the same registry metadata. A dynamic command head
remains unknown, so the checker suppresses only answers that depend on guessing
its runtime command or an interpolation-dependent argument. Diagnostics
therefore describe language-level contract incompatibility, not executable
availability.

A successful check is silent and exits with status 0. Any analysis or source
error is written only to stderr and exits with status 1. Invocation misuse
writes one `fsh:` message to stderr and exits with status 2; checker help writes
to stdout and exits with status 0.

`fsh check` does not load startup configuration or history, initialize any
module, evaluate declarations or substitutions, expand words, probe an
executable, apply redirections, change the working directory, mutate the
environment, access a terminal, or start a process. It does not format source,
predict runtime output or status, validate external-command availability,
infer types that depend on runtime values, discover a multi-root project, or
start a language server. Success means only that the supported non-executing
analyses completed without error diagnostics; target capabilities and
runtime-only data remain separate concerns.

### Execution-plan inspection

Inspect one exact command pipeline with:

```bash
fsh plan command.fsh
fsh plan -- --maintenance.fsh
fsh plan --help
```

The named regular UTF-8 source must contain exactly one top-level foreground
job with one pipeline. Declarations, assignments, imports, environment
statements, control flow, callable definitions, background jobs, and
status-dependent `&&` or `||` chains are rejected because their exact plan can
depend on execution or prior state. Stdin, multiple roots, script arguments,
configuration, and history are not accepted.

Inspection parses and statically analyzes the source with the standard command
registry, then expands the pipeline against an empty lexical scope, the
inherited process environment, the launcher's current working directory, and
default session options. Bare and forced external commands use read-only
executable metadata checks, so the plan shows the executable selected from the
inherited `PATH`. Command substitution is rejected rather than evaluated.

The deterministic plan includes its source spans, native cwd and child
environment, session options, process-group policy, source-ordered stage
resolution, exact escaped native arguments, typed internal arguments, carrier
contracts, redirections, help snapshots, and pipeline edges. Native units use
escaped bytes so distinct non-UTF-8 values do not collapse through replacement
characters. The output is human-facing inspection text, not executable input or
a serialization format.

The plan contains the complete inherited child environment and may therefore
include credentials or other secrets. Treat captured or shared plan output as
sensitive data.

Successful inspection writes the newline-terminated plan to stdout and exits 0.
Source, analysis, shape, expansion, resolution, or preflight failure writes a
source-backed diagnostic to stderr and exits 1. Invocation misuse exits 2.
Inspection never initializes a session, mutates lexical/environment/cwd state,
opens a redirection, creates a pipe, spawns or waits for a process, starts
background work, or accesses a terminal.

### Canonical formatting

Use the launcher formatter modes on one or more explicit files:

```bash
fsh format --check source.fsh library.fsh
fsh format --write source.fsh library.fsh
fsh format --help
```

`--check` visits every operand in command-line order without writing. Canonical
files are silent; each complete source that differs receives an anchored
`FMT001` diagnostic. `--write` parses and formats the complete batch before its
first mutation. A read, UTF-8, incomplete, or invalid-source failure therefore
leaves every operand untouched.

Operands must name existing regular files. Use the formatter `--` delimiter
before a dash-leading path. The formatter does not read stdin, expand globs,
walk directories, follow a final symlink, or discover imported sources. Each
named file is independent, and two operands that resolve to the same canonical
target are rejected.

After a successful write preflight, each changed file is replaced in operand
order through a synchronized sibling temporary file and same-directory atomic
rename. Unchanged files are not opened for write. Changed files preserve their
permission bits, but replacement creates a new file identity and does not
promise hard-link identity, ownership, group, timestamps, ACLs, or extended
attributes. A source or permission change detected after preflight is refused.
An individual replacement is atomic, but a multi-file write is not a
transaction: the first replacement failure stops the batch after any earlier
successful replacements.

Successful check and write operations emit nothing and exit with status 0.
Noncanonical check results and source or filesystem failures use stderr and
status 1. Invocation misuse uses one `fsh:` message on stderr and status 2.
Formatter output never uses stdout except for `format --help`.

Formatting is idempotent: formatting already formatted source does not produce
another change. The canonical result preserves parsed program structure,
significant token spelling, comments, and documentation-comment attachment.
Incomplete input retains `SYN002`, while invalid input retains the parser's
structured diagnostics.

`fsh format` is a non-executing launcher frontend. It does not load startup
configuration or history, traverse imports, initialize a runtime session,
resolve commands, probe executables, or execute source.

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

Use `env("BUILD_MODE")` to read the current child-environment entry. The result
is a `String`, or `null` when the name is absent. Flash does not treat `$NAME` as
an environment lookup, and a present native value that is not valid UTF-8 fails
explicitly rather than being decoded lossily. Reads observe successful
`export` and `unset` changes in source order.

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

### Built-in namespace compatibility

The standard command namespace distinguishes canonical **core** commands,
invocable migration **aliases**, and **reserved** names that cannot fall through
to an external executable. The Flash v1 standard manifest contains no aliases
or reserved names.

The Flash v1 core command inventory is:

```text
bg cd check collect command decode each encode exit fg first from get help jobs
kill last length lines ls open pwd save select sort to update wait where which
```

`export` and `unset` are statement keywords, not commands in this inventory.
The inventory is a source-language compatibility boundary because registering a
previously unknown bare name can redirect an existing script away from an
external executable.

| Namespace change | Flash v1 compatibility |
| --- | --- |
| Change implementation, performance, or documentation without changing observable command behavior | Compatible within the language major |
| Add deprecation metadata while preserving behavior and canonical identity | Compatible within the language major |
| Activate a name already reserved at the start of the language major | Compatible within the language major |
| Add a core command, alias, or reservation under a previously unknown name | Requires the next language major |
| Remove or rename a core command; remove or retarget an alias; or release a reservation to external fallback | Requires the next language major |
| Change an entry between core, alias, and reserved, except activation of an existing reservation | Requires the next language major |
| Change carriers, arguments, flags, effects, output, status, or other behavior in a way that can alter a successful program | Requires semantic review and normally the next language major |

An alias, when present in a future manifest, retains its source spelling but
uses the canonical core command's signature and behavior. A reserved bare name
fails before `PATH` lookup; intentional external execution remains available
through `^name` or `command name`. Runtime execution does not print unsolicited
deprecation warnings. `fsh check` reports deprecated exact command heads as
`CMD001` warnings and reserved exact bare heads as `CMD002` errors without
probing `PATH`.

`help` includes every core, alias, and reserved entry and exposes lifecycle or
migration details. Executable completion includes core commands and aliases,
with aliases reusing canonical flags, but excludes reserved names. `which`
returns ordered records with this schema:

```text
{
  name: Path,
  kind: "internal" | "alias" | "reserved" | "external" | "missing",
  target: String | null,
  path: Path | null
}
```

`target` contains an alias's canonical command or a reserved entry's suggested
replacement; other results use `null`. Only an external result populates
`path`. Reserved and missing results select final status 1; internal, alias, and
external results are successful.

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

`glob` accepts a string or path pattern and returns a sorted list of native path
values. `*`, `?`, and character classes match within one component; a complete
`**` component recurses without following directory symlinks. Leading-dot names
require an explicitly leading dot in that component, and no matches produces an
empty list rather than the original pattern.

For the complete expansion rules and eligible argument value types, see [Commands and argument expansion](language-guide.md#commands-and-argument-expansion).

## Command substitution

Command substitution captures standard output as text or exact bytes:

```text
let directory = "$(pwd)"
let explicit_text = $(text: ^program --version)
let payload = $(bytes: ^program --binary)
```

The `$(...)`, `$(text: ...)`, and `$(bytes: ...)` forms evaluate one foreground
command or conditional chain. They do not start an intermediate shell.

Successful text capture:

- reads standard output only;
- requires valid UTF-8;
- removes trailing LF and CRLF line-ending sequences;
- preserves other whitespace and newlines;
- produces exactly one `String`;
- never reparses or splits the captured text.

An empty capture therefore produces one empty string rather than no value.

Byte capture reads the same reached standard output under the same limit and
status rules, but produces exactly one `Bytes` value. The capture operation
never decodes, trims, displays, serializes, reparses, or splits the data. A byte
capture is not eligible for implicit command-word insertion; bind or pass it as
a value and cross to text only through an explicit decoding boundary.

Standard error remains inherited unless the source redirects it:

```text
let version = "$(^program --version 2> version-errors.log)"
```

A nonzero command exit still produces captured output paired with its actual status. It does not become a runtime error merely because the command was unsuccessful.

Capture is bounded by the session capture limit. If the output exceeds that limit, Flash continues draining and reaping the started processes before returning a capture-limit runtime error. This prevents the memory bound from causing a pipe deadlock.

Invalid UTF-8 similarly produces a runtime error rather than silently replacing
bytes. Use `$(bytes: ...)` when arbitrary binary output must be preserved.

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

Explicit byte boundaries may occur more than once in one pipeline. Flash v1 permits carrier-compatible shapes such as:

```text
^producer
    | decode utf8
    | where {|text| true}
    | encode utf8
    | ^transform
    | decode utf8
    | where {|text| true}
```

The external stages remain byte-oriented, while each internal island keeps its structured values inside the runtime.

Interactive rendering of a record, list, or table is for human inspection. It is not a stable serialization format. Use an explicit encoder or formatter when a file or external process requires bytes.

### Alternating mixed pipelines

Flash executes every carrier-compatible alternating shape without capturing an
external segment into memory. External children start first, then maximal
internal segments drain concurrently through bounded operating-system pipes.
Structured values and lazy pull closures stay inside the internal segment that
owns them; only byte descriptors cross a concurrent segment boundary.

Internal stage preparation remains source ordered. Session commands therefore
apply cwd and child-environment changes deterministically even when segment
drains overlap. Lazy closure changes are visible within their segment and merge
in segment source order only after the complete pipeline succeeds.

On normal completion, every source stage contributes exactly one status leaf.
A `check` immediately after an external stage forwards its bytes before
waiting, then checks that stage's real completion status after all bytes have
drained. A failed check, runtime error, output failure, or wait failure prevents
pending cwd, environment, closure, and status changes from committing. Output
already written, files already changed, and completed process effects are not
rolled back.

Failure or explicit `exit` stops later preparation, closes owned endpoints,
terminates and waits live external children, and restores terminal ownership.
When independent stages fail concurrently, the earliest source-stage failure
is reported. A downstream consumer closing early remains ordinary pipeline
backpressure cleanup rather than fabricated success or an implicit status
conversion.

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

The same rule applies to an internal byte-producing segment tail. If its final
stdout route is a local file, Flash writes the bytes there and closes the unused
pipeline writer, so the downstream external stage observes EOF. A final
inherited, read-only, or closed stdout binding that the internal executor cannot
write is rejected during preflight.

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

### Process exit and output contract

The `fsh` process preserves normally completed command results and uses
separate statuses for launcher misuse and shell-owned failure. CI callers can
rely on the following matrix without parsing diagnostic prose:

| Executable outcome | Process status | Output contract |
|---|---:|---|
| Help, version, successful check or format, or a script with no completed command | 0 | Requested help/version text uses stdout; the other cases are silent |
| Launcher misuse | 2 | Exactly one newline-terminated `fsh:` message on stderr |
| Completed command with code `0..=255` | Exact command code | No automatic status message |
| Completed command with numeric signal `1..=127` | `128 + signal` | No automatic status message |
| Completed status that cannot be represented by those rules | 1 | One shell-boundary report on stderr |
| Parse, module, runtime, source-loading, platform, or output failure | 1 | A diagnostic or shell report on stderr |
| Interactive end of input | 0 | No exit diagnostic |
| Interactive `exit CODE` | Exact requested code | No exit diagnostic |
| Fatal interactive editor, reporting, or platform failure | 1 | A fatal report when stderr remains usable |

Codes 1 and 2 remain valid ordinary command results. A command that really
exits with either code is silent unless the program itself wrote output; it is
not reclassified as a shell failure or launcher misuse. Resolution and spawn
failures did not complete as child processes, so Flash reports them as runtime
errors instead of manufacturing command-not-found 127 or not-executable 126.

Program bytes use stdout. Source diagnostics, shell-owned reports, loading
errors, and failing background-job reports use stderr. Source diagnostics are
deterministic, contain no color or terminal-control bytes, and end with a
newline. Required writes and flushes are checked. A failed stdout write or
flush becomes status 1 and is reported on stderr when possible; a failed stderr
write or flush is never reported recursively through stderr.

Pipeline aggregation and `pipefail` select the completed status before this
process mapping occurs. Script background jobs are joined first: their failure
reports use stderr in job-identity order, and the first failing job in that
order retains the established final-status precedence. Interactive diagnostics
remain recoverable, while a fatal interactive failure hangs up session-owned
jobs before returning status 1.

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

### Catching and raising runtime errors

Use a `try` statement when a script can recover from a runtime failure:

```text
try {
    ^build | check
} catch error {
    ^log $error.category $error.message
}
```

The catch binding is immutable and exists only inside its block. The catch
starts with the lexical bindings, logical working directory, child environment,
session options, and current status from before the `try`. Successful changes
made by the catch then commit normally. Bytes already written, filesystem
changes, and child-process effects remain observable.

`throw "message"` raises a source-anchored user error. `throw $error` rethrows
an existing `Error` with its source, labels, nested-call frames, cause, and
optional status intact. Throwing any other value is itself a typed runtime
error.

Only runtime errors are caught. In particular:

- a nonzero or signalled command completion remains an ordinary `Status` until
  a reached `check` converts it;
- cancellation and explicit `exit` bypass the catch and retain their cleanup
  behavior; and
- parse or analysis failures, stopped-job control, and fatal output/editor
  failures never become catch values.

The complete `Error` member and equality contract is in the
[Language Guide](language-guide.md#structured-error-handling).

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

The script-argument interface is deliberately narrower: every operand after the
script path must decode as UTF-8 before the program is loaded. Arbitrary native
path and external-process argument values do not silently become script
`String` values.

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
