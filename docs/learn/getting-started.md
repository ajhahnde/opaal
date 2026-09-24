# Getting started

Build the CLI from a checkout with the pinned Rust toolchain:

```sh
cargo build --workspace --locked --release
```

The checked-in [language example](../../examples/language-foundation.opaal) is a pure `.opaal` file. Format-check it, analyze it, then run it:

```sh
target/release/opaal format --check examples/language-foundation.opaal
target/release/opaal check examples/language-foundation.opaal
target/release/opaal examples/language-foundation.opaal
```

All three successful non-interactive commands are silent. The final value is retained by the evaluator rather than printed as an implicit line. Run `target/release/opaal` in a terminal to enter expressions interactively and see completed values.

The example imports `std::value`, defines a generic enum and function, matches
a list, and counts its tail. It needs no project or authority file. In 1.1,
an explicit foreground command such as `^touch marker` can perform the OS
user's host effects. Review the [direct process boundary](../how-to/run-an-external-program.md)
before running untrusted source.

Continue with the [language tour](language-tour.md). If you want to execute a typed task under explicit authority, follow [first project](first-project.md) after the [operational model](operational-model.md).
