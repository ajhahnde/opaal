# `std::outcome`

The compiled `std::outcome` module exports two generic enums:

```opaal
import std::outcome as outcome

enum Result[T, E] {
    Ok(T),
    Err(E),
}

enum Option[T] {
    Some(T),
    None,
}
```

Import the module with an alias, construct a qualified variant, and match that same nominal identity. `Result::Ok` carries `T`; `Result::Err` carries `E`. `Option::Some` carries `T`; `Option::None` carries no value. A closed unguarded match must handle reachable variants.

These are ordinary immutable source values. They do not convert an execution refusal, cancellation, fatal host failure, or incomplete journal into a successful value. For those executor classifications see [outcomes and errors](../language/outcomes-and-errors.md).
