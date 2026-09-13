# Language tour

The [checked-in example](../../examples/language-foundation.opaal) starts with `import std::value as value`. Standard modules are compiled identities, and the alias makes their exports visible as qualified names. A local module would use a quoted `.opaal` path and an alias instead.

`Selection[T]` is a nominal generic enum. Its `Selected(T)` and `Empty` variants are different from a record that happens to have similar fields. The `tail` function matches an empty list or binds `first` and the remaining list. Both arms return a `Selection[List[T]]`; a closed match must cover reachable variants.

The final match calls `value::length(rest)` and leaves its result as the file's value. In a function, `return` exits early; an un-terminated final expression can also supply the block value. A loop, declaration, or terminated expression supplies `Null`.

OPAAL has a separate command model. An internal command name resolves through the core registry. A literal `^` introduces an external program; an unknown bare word is an error rather than a host search. In command words `{expression}` contributes one scalar fragment and `...{expression}` spreads a list into arguments. Pipeline stages exchange explicit byte or structured carriers, so conversion is named rather than guessed.

Use the [language reference](../reference/language/README.md) when you need the exact syntax and [standard modules](../reference/std/README.md) for exports. Next, [operational model](operational-model.md) explains why an effect declaration alone cannot run a host operation.
