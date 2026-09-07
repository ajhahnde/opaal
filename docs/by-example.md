# OPAAL by example

The checked-in [`language-foundation.opaal`](../examples/language-foundation.opaal)
demonstrates the pure source surface:

```opaal
import std::value as value

enum Selection[T] {
    Selected(T),
    Empty,
}

def tail[T](items: List[T]) -> Selection[List[T]] {
    match $items {
        [] => { return Selection::Empty }
        [first, ...rest] => { return Selection::Selected($rest) }
    }
}

match tail(["build", "test", "review"]) {
    Selection::Selected(rest) => { value::length($rest) }
    Selection::Empty => { 0 }
}
```

Build, format, check, and run it from the repository root:

```sh
cargo build --workspace --locked
target/debug/opaal format --check examples/language-foundation.opaal
target/debug/opaal check examples/language-foundation.opaal
target/debug/opaal examples/language-foundation.opaal
```

All three successful non-interactive commands are silent. The embedding API
retains the final `Int(2)` rather than serializing a language value to stdout.

The example uses qualified module aliases, immutable nominal variants, generic
list patterns, and the shared `std::value::length` operation descriptor used by
checking, execution, help, completion, hover, and signature help.

Running `target/debug/opaal` in a terminal starts the interactive client:

```opaal
import std::value as value
value::length(["build", "test"])
help value::length
```

Effectful source such as `^touch marker` is rejected or refused before process
access. `opaal plan` returns `PLAN004` until a separately designed authority and
planning contract exists.

[← Documentation index](README.md) · [Language foundation →](language-foundation.md)
