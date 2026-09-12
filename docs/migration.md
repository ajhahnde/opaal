# Pre-1.0 language migration

The unreleased `1.0.0-alpha.1` development line now uses one expression model
for names, calls, block values, interpolation, list spread, and explicit
external programs. This is an in-place source migration: there is no legacy
parser, runtime switch, compatibility mode, or shipped converter.

Update existing development source as follows:

| Former form | Current form | Meaning |
| --- | --- | --- |
| `$name` | `name` | Read the visible binding. |
| `$name = value` | `name = value` | Assign the nearest visible mutable binding. |
| `$callable(args)` | `callable(args)` | Call the resolved value once. |
| `${expression}` | `{expression}` | Interpolate one bounded expression value. |
| `...$items` | `...{items}` | Spread one list into command arguments. |
| bare external head | `^literal` | Request one literal external program explicitly. |

There is no replacement command-substitution syntax. Use a typed operation to
obtain bounded process output, bind its result, and then pass or interpolate
that value normally. Dynamic external-program names are unsupported; select a
literal `^` head or a statically known typed operation.

Blocks now consistently yield their final non-terminated expression. Remove an
unnecessary explicit `return` when fallthrough is clearer, but keep `return`
when an early exit is intended. A declaration, terminated expression, empty
block, loop, or `if` without a selected value path yields `Null` under its
documented type rules.

Dollar characters remain valid quoted data, for example `"$HOME"`; they no
longer trigger interpolation. Use `"{home}"` when the value of the OPAAL binding
`home` is intended.

After rewriting source, run:

```sh
cargo run --locked -p opaal-cli --bin opaal -- format --check PATH
cargo run --locked -p opaal-cli --bin opaal -- check PATH
```

Former dollar forms receive an actionable syntax diagnostic. Unknown bare
heads receive a static name/command diagnostic and are never retried as host
processes.

[← Documentation index](README.md) · [Language foundation](language-foundation.md)
