# OPAAL

**Operational Programming & Automation Language**

OPAAL combines typed, reusable logic with structured-data pipelines and explicit
task execution for automation. This repository contains the language, runtime,
command-line client, language server and platform adapters.

[Documentation](https://opaal-lang.org/docs/) ·
[Learning guides](https://opaal-lang.org/learn/) ·
[Downloads](https://opaal-lang.org/download/) ·
[Changelog](CHANGELOG.md)

## OPAAL at a glance

OPAAL programs are UTF-8 `.opaal` files. This example is also saved as
[examples/readme_example.opaal](examples/readme_example.opaal):

```opaal
import {
    std::value::{length as len},
    std::io::{println},
}

def remaining[T](items: List[T])
{
    match items {
        [] => { return 0 }
        [first, ...rest] => { return len(rest) }
    }
}

let list_string = ["build", "test", "review"]

mut list_int = [1, 2, 3, 4]
println("Result 1: {remaining(list_int)}")
list_int = [...list_int, 5, 6]

println("Result 2: {remaining(list_string)}")
println("Result 3: {remaining(list_int)}")
```

Output:

```text
Result 1: 3
Result 2: 2
Result 3: 5
```

The imports select two standard-library functions, giving `length` the local
name `len`. `remaining[T]` works with lists of integers or strings: `match`
handles the empty list, or separates `[first, ...rest]` and counts `rest`.

`mut` allows `list_int` to be reassigned. `[...list_int, 5, 6]` builds a new
list from its existing items followed by `5` and `6`. Inside a string,
`{remaining(list_int)}` inserts the function's result as text; `println`
prints the resulting string followed by a newline.

See the [learning guides](https://opaal-lang.org/learn/) for a full introduction.

`opaal check examples/readme_example.opaal` analyzes source without executing it,
`opaal format --check examples/readme_example.opaal` verifies the canonical
style without rewriting it,
and `opaal format --write examples/readme_example.opaal` formats the file.

Run `opaal` without a script in a terminal to start the interactive client.
Completed expressions display their values there; scripts produce output through
explicit I/O or foreground programs. An editor can use
`opaal-language-server` for diagnostics, navigation, completion and formatting.

## Execution and authority

A literal `^program` launches a foreground external program; an unknown bare
name never launches a host process. Controlled tasks use a separate project
workflow: inspect, check, plan, explicitly accept, execute and audit. Declared
effects describe required authority; they do not grant it. Effectful actions
run only through the accepted-plan route, while scripts and interactive cells
can use their explicitly bound native standard streams.

## Platform availability

Supported hosts are Linux x86_64, GNU/Linux AArch64 and macOS arm64.
GNU/Linux AArch64 requires glibc 2.35 or newer and Linux kernel 6.3 or newer.
Release archives include the CLI and language server for all three hosts.
Controlled process execution is Linux-only; macOS can check and plan those tasks
but refuses their execution. Host qualification establishes no downstream
operating system or physical-device support. See the
[project reference](https://opaal-lang.org/docs/projects/) and
[compatibility guide](https://opaal-lang.org/docs/compatibility/) for details.

The [repository-check reference](examples/repository-check/README.md) combines
a pinned Linux native checker with reusable repository policy. Its saved-input
route and pure tests work on all qualified hosts. Native pins identify bytes,
with no version claim or child confinement; live use requires explicit acceptance.

## Build from source

The [pinned Rust toolchain](rust-toolchain.toml) and checked-in lockfile define
the build. From the repository root:

```sh
cargo build --workspace --locked
```

On macOS, apply the required native image policy to the built CLI before using
native standard streams or Random:

```sh
codesign --force --sign - --options kill target/debug/opaal
```

Run the example with `target/debug/opaal examples/readme_example.opaal`.
Reapply the image policy after rebuilding. Full build, test and native
qualification instructions are in [Development](DEVELOPMENT.md).

## Contributing

See [Contributing](CONTRIBUTING.md) for change guidelines,
[Security](SECURITY.md) for vulnerability reporting and
[Release qualification](RELEASING.md) for the maintainer workflow.

## License

Unless a file states otherwise, OPAAL is licensed under the
[Mozilla Public License 2.0](LICENSE).
