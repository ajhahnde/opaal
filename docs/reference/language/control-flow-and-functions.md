# Control flow and functions

A block evaluates statements in order. Its last expression is the block value when it has no terminator. An empty block, declaration, or terminated expression yields `Null`. `return` exits the current function or action immediately; falling through its body returns the body's value.

A selected `if` or `match` block keeps that block's value. Loops discard body values and return `Null`. This distinction matters when a branch is used as an expression rather than only for side effects. The checker rejects an inferred result that conflicts with an explicit return type.

Functions and closures are callable values in the pure language. Generic functions may declare invariant type parameters and the closed `Equal` and `Ordered` constraints. Action calls follow stricter rules: actions are statically named, are not first-class values, and cannot be called from a function. An action caller must declare every effect reachable through its callees. See [actions and effects](actions-and-effects.md).

Use [values, types, and patterns](values-types-and-patterns.md) for binding and matching forms. The [specification](../../specification/1.0.md) owns the conformance rule for block results.
