# OPAAL

**Operational Programming & Automation Language**

OPAAL combines typed, reusable logic with structured-data pipelines and explicit
task execution for automation. This repository contains the language, runtime,
command-line client, language server and platform adapters.

[Documentation](https://opaal-lang.org/docs/) ·
[Learning guides](https://opaal-lang.org/learn/) ·
[Downloads](https://opaal-lang.org/download/) ·
[Changelog](CHANGELOG.md)

## Try OPAAL

OPAAL programs are UTF-8 `.opaal` files:

```opaal
import std::value as value
import std::io as io

def remaining[T](items: List[T])
{
    match items {
        [] => { return 0 }
        [first, ...rest] => { return value::length(rest) }
    }
}

io::print("Result: ")
io::println("{remaining(["build", "test", "review"])}")
```

Save this as `remaining.opaal`. With OPAAL installed, run:

```sh
opaal remaining.opaal
opaal check remaining.opaal
opaal format --check remaining.opaal
```

The program prints `Result: 2` followed by one line feed. String interpolation
converts the Int result to text for `io::println`. `check` analyzes source without
executing it; `format --check` verifies the canonical style without rewriting it.

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

Binary archives are available for Linux x86_64 and macOS arm64. Controlled
process execution is Linux-only; macOS can check and plan those tasks but
refuses their execution. Host qualification establishes no downstream operating
system or physical-device support. See the
[project reference](https://opaal-lang.org/docs/projects/) and
[compatibility guide](https://opaal-lang.org/docs/compatibility/) for details.

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

Run the example with `target/debug/opaal remaining.opaal`. Reapply the image
policy after rebuilding. Full build, test and native qualification instructions
are in [Development](DEVELOPMENT.md).

## Contributing

See [Contributing](CONTRIBUTING.md) for change guidelines,
[Security](SECURITY.md) for vulnerability reporting and
[Release qualification](RELEASING.md) for the maintainer workflow.

## License

Unless a file states otherwise, OPAAL is licensed under the
[Mozilla Public License 2.0](LICENSE).
