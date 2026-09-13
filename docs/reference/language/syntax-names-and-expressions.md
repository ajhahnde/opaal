# Syntax, names, and expressions

OPAAL reads a UTF-8 `.opaal` file as one module. Source starts with imports, declarations, or expressions; no header or version directive chooses a language mode. The parser reports malformed and incomplete input with source spans. The formatter works from syntax and does not execute expressions.

A bare identifier reads a visible binding, callable, registered core command, or known operation according to its position. The same lexical and qualified-name rules govern reads, assignments, and calls. An unknown bare head is an error, not a request to search the host's `PATH`. Use `::` to name a member through an imported module alias. Local variables are not expanded with a dollar prefix.

Literals include strings, numbers, booleans, `null`, lists, and records. Expressions may be combined with operators, calls, indexing, branches, and blocks. A block's final expression supplies its value unless terminated. A declaration or terminated expression supplies `Null`. The [control-flow reference](control-flow-and-functions.md) gives the remaining block cases.

Inside a command word, `{expression}` evaluates once and contributes one bounded scalar fragment. `...{expression}` evaluates once and spreads a list into separate arguments. A dollar sign inside quoted data remains a character, so `"$HOME"` is text. External-program heads must be literal. [Commands and pipelines](commands-pipelines-and-streams.md) describes the separate carrier rules.

The [specification](../../specification/1.0.md) owns the semantic rules.
