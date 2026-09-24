# Run a script

Save UTF-8 OPAAL source in a regular `.opaal` file. From a checkout, build the pinned toolchain and run the checked-in pure example:

```sh
cargo build --workspace --locked --release
target/release/opaal examples/language-foundation.opaal
```

A pure non-interactive run does not print its final value automatically. A
foreground external child can write to stdout and stderr. To see completed
values while trying expressions, start `target/release/opaal` in a terminal.

The first operand is the source path. Every later UTF-8 operand belongs to that script, even if it begins with `-`. Use `opaal -- SCRIPT` when a script filename could be read as an option. File modules and their static imports must be regular `.opaal` files. The run does not search for `opaal.toml`.

OPAAL 1.1 permits an explicit foreground program in a script. For example,
`^git status --short` starts the first executable named `git` on the retained
`PATH` and passes the two arguments directly, without a shell. The child writes
to the script's stdout and stderr. See [direct external execution](run-an-external-program.md)
for environment, status, and security limits.

Effectful actions and unrelated host commands remain refused before host
access. To run a declared task under exact authority, use an [explicit project](create-a-project.md)
and a [reviewed plan](review-and-execute-a-plan.md). [CLI](../reference/tooling/cli.md)
gives all invocation forms.
