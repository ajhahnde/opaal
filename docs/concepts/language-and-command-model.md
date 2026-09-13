# The language and command model

OPAAL does not guess whether an unknown word names a value or a program. Bare names resolve in the visible language and command namespace. An unknown name is diagnosed. Only a literal `^` head asks for an external program. That spelling makes the host boundary visible at the call site and keeps a misspelled function from turning into a process launch.

Expressions produce values; command stages exchange one of four carriers: empty, byte stream, single value, or value stream. The distinction keeps a byte stream from becoming a record merely because a later stage wants one. A conversion must be named. `decode` and `from` turn bytes into structured values; `encode` and `to` go the other way. Lazy value streams are single-consumer resources, so pulling, collecting, and cleanup have observable bounds.

The same principle applies inside a command word. `{expression}` makes one scalar fragment, while `...{expression}` spreads one list into arguments. Neither performs ambient shell expansion. Dollar characters in quoted input are data. This keeps command construction inside the typed evaluator instead of delegating it to a shell.

The [syntax reference](../reference/language/syntax-names-and-expressions.md), [pipeline reference](../reference/language/commands-pipelines-and-streams.md), and [core-command catalog](../reference/language/core-commands.md) give the accepted forms.
