# Language reference

A source file is a UTF-8 `.opaal` module without a version directive. Imports are explicit. A bare name resolves in the visible language namespace and never falls back to a host executable.

| Topic | Lookup |
| --- | --- |
| Tokens, names, literals, expressions | [Syntax, names, and expressions](syntax-names-and-expressions.md) |
| Values, types, records, variants, patterns | [Values, types, and patterns](values-types-and-patterns.md) |
| Blocks, branches, loops, callables | [Control flow and functions](control-flow-and-functions.md) |
| Imports, aliases, exports | [Modules and imports](modules-and-imports.md) |
| Command composition and carriers | [Commands, pipelines, and streams](commands-pipelines-and-streams.md) |
| Registered command names and signatures | [Core commands](core-commands.md) |
| Typed actions and effect requests | [Actions and effects](actions-and-effects.md) |
| Values, errors, refusal, cleanup | [Outcomes and errors](outcomes-and-errors.md) |

The [standard modules](../std/README.md) are separate from the core command registry. A registered command signature does not grant effectful source execution.
