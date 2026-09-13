# Run a script

Save UTF-8 OPAAL source in a regular `.opaal` file. From a checkout, build the pinned toolchain and run the checked-in pure example:

```sh
cargo build --workspace --locked --release
target/release/opaal examples/language-foundation.opaal
```

A successful non-interactive run is silent. The evaluator retains the final value; it does not print it automatically. To see completed values while trying expressions, start `target/release/opaal` in a terminal.

The first operand is the source path. Every later UTF-8 operand belongs to that script, even if it begins with `-`. Use `opaal -- SCRIPT` when a script filename could be read as an option. File modules and their static imports must be regular `.opaal` files. The run does not search for `opaal.toml`.

An effectful action or direct `^` program request in ordinary source is refused before host access. To run a declared task under exact authority, use an [explicit project](create-a-project.md) and a [reviewed plan](review-and-execute-a-plan.md). [CLI](../reference/tooling/cli.md) gives all invocation forms.
