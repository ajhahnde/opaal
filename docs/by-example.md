# OPAAL by example

The checked-in [`language-foundation.opaal`](../examples/language-foundation.opaal)
demonstrates the pure language 1 surface:

```opaal
language 1

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

All three successful non-interactive commands are silent. The runtime retains
the final `Int(2)` for an embedding caller; it does not implicitly serialize a
language value to stdout.

The example shows four important rules:

- every module declares `language 1`;
- imports use qualified aliases and preserve canonical module identity;
- nominal variants and generic list patterns keep their declared types; and
- `std::value::length` uses the same compiled descriptor in checking,
  execution, help, completion, hover, and signature help.

Run `target/debug/opaal` in a terminal for the interactive client. Interactive
cells preselect OPAAL language 1 and omit the file directive:

```opaal
import std::value as value
value::length(["build", "test"])
help value::length
```

The first expression is presented as `2`; help renders the same operation
descriptor used by static analysis and execution.

Effectful examples are deliberately absent. A source such as
`^touch marker` is rejected or refused before process access. `opaal plan`
similarly returns `PLAN004` until a later, separately reviewed authority and
planning contract exists.

[← Documentation index](README.md) · [Language 1 foundation →](opaal-language-1-foundation.md)
