# Flash by Example

[FlashOS](../../../README.md) › [Flash](../README.md) › [Documentation](README.md) › By Example

This guide teaches Flash with four small programs. They live in the repository and run as part of the documentation checks. For complete language details, use the [Language Guide](language-guide.md) and [Scripting Guide](scripting.md).

> **Where these run:** the examples are tested with Flash 1.0 on supported Linux and macOS development hosts. FlashOS installs the same language as `/usr/bin/fsh`, but an image may not contain every external command used here. A host run is therefore not an image test.

## Run the examples

From the repository root, build Flash and set a convenient path to the executable:

```bash
cargo build --manifest-path components/flash/Cargo.toml --locked --bin fsh
FSH=components/flash/target/debug/fsh
```

The following sections use that `FSH` shell variable only in the host command that launches Flash. It is not a Flash language variable.

## Keep values structured

[`structured-files.fsh`](../examples/structured-files.fsh) gets directory entries as records, keeps the files, selects their names, and converts the final list to JSON:

```fsh
ls \
| where {|entry| $entry.type == "file"} \
| get name \
| each {|name| "$name"} \
| sort \
| collect \
| to json \
| ^cat
```

Run it in any directory:

```bash
"$FSH" components/flash/examples/structured-files.fsh
```

The result is a JSON array of file names. `ls`, `where`, `get`, `each`, `sort`, and `collect` pass Flash values between them. `to json` performs the visible conversion, then external `cat` receives bytes.

## Parse output from an external command

External processes produce bytes, not Flash tables. [`json-boundary.fsh`](../examples/json-boundary.fsh) reads JSON bytes from `printf`, works with the resulting records, then converts the result back to JSON:

```fsh
^printf '[{"name":"build","active":true},{"name":"deploy","active":false}]' \
| from json array \
| where {|item| $item.active} \
| select name \
| collect \
| to json \
| ^cat
```

```bash
"$FSH" components/flash/examples/json-boundary.fsh
```

The output is:

```text
[{"name":"build"}]
```

Follow the data across the pipeline: external bytes → `from json array` → structured values → `to json` → external bytes.

## Distinguish status from failure

An external process can return an unsuccessful status without crashing the Flash runtime. [`checked-status.fsh`](../examples/checked-status.fsh) uses `check` to turn that status into a catchable error:

```fsh
try {
    ^false | check
} catch error {
    let category = $error.category
    ^printf 'caught %s\n' $category
}
```

```bash
"$FSH" components/flash/examples/checked-status.fsh
```

The program prints `caught control` and exits successfully. Without `check`, the nonzero result from `false` remains an ordinary unsuccessful `Status`. Parse, resolution, I/O, and other runtime failures already follow their own error paths.

## Inspect a plan without running it

`fsh plan` shows how one foreground pipeline would be executed without opening redirections or starting commands. Try it with [`planned-pipeline.fsh`](../examples/planned-pipeline.fsh):

```bash
"$FSH" plan components/flash/examples/planned-pipeline.fsh
```

The report includes the working directory, inherited environment, resolved executables, arguments, value carriers, and pipeline connection. It may contain secrets from the environment, so review it before sharing. `plan` only handles this single-pipeline case; it cannot predict arbitrary external side effects.

## Explore interactively

Running `fsh` without a script opens the interactive shell. The same built-ins and language rules remain available there:

```text
help
help where
ls | where {|entry| $entry.type == "file"} | select name size
```

Interactive output is meant for people and may change. Scripts that need bytes should finish structured results with `to` or `encode`.

Job commands are structured too. `jobs` produces records that can be filtered and projected, while `wait`, `bg`, `fg`, and `kill` work with job IDs. [Job commands](scripting.md#job-commands) documents the platform and terminal details.

## Continue learning

- [Language Guide](language-guide.md) — Values, expressions, functions, modules, commands, and pipelines.
- [Scripting Guide](scripting.md) — Files, arguments, checking, formatting, processes, statuses, errors, and jobs.
- [Flash and FlashOS](../../../docs/flash.md) — How the shipped shell and automation fit the current system, and which interaction ideas remain future direction.

---

[← Flash documentation](README.md) · [Next: Language Guide →](language-guide.md)
