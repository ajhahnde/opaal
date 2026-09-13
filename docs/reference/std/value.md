# `std::value`

Import `std::value` for the pure `length` operation:

```opaal
import std::value as value
value::length(["build", "test", "review"])
```

The compiled descriptor has one generic type parameter `T` and two overloads: `length(input: List[T]) -> Int` and `length(input: ValueStream[T]) -> Int`. The first counts retained list items. The second consumes a bounded, single-consumer value stream and counts delivered items. Both return an `Int`; a count that cannot fit is an error.

The operation can be used as an expression and, with a compatible carrier, in a pure operation pipeline. Help, completion, hover, signature help, checking, and evaluation share this descriptor. The core command named [`length`](../language/core-commands.md) is a separate stream command with a registry signature.
