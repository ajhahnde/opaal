# Commands, pipelines, and streams

OPAAL distinguishes a value expression from a command stage. A known internal command is resolved through the core registry. Only a literal head with `^` requests external-program resolution. An unknown bare command head never falls back to a process. A computed program name is unsupported.

A command word can contain `{expression}` for one scalar fragment or `...{expression}` for one list spread. Both evaluate once. The spread emits one argument per eligible list element; nested lists are not recursively flattened. Qualified operation names keep `::` spelling. See [syntax](syntax-names-and-expressions.md) for the value-expression side.

A pipeline connects explicit carriers: empty input, byte stream, single value, or value stream. A stage's registered signature specifies what it accepts and produces. The checker rejects incompatible adjacent carriers rather than silently serializing structured data. `decode` and `from` cross from bytes to values; `encode` and `to` cross back. Stream consumers pull lazily under item, byte, terminal-state, cancellation, and cleanup bounds. A stream is single-consumer even when its elements are ordinary values.

[Core commands](core-commands.md) gives signatures. In OPAAL 1.1, a standalone
or interactive CLI session may run a foreground caret stage with explicit
redirections and the listed external-connected transforms: `check`, `decode`,
`from`, `encode`, `to`, `first`, `last`, `collect`, `length`, `lines`, `each`,
`where`, `select`, `get`, `update`, and `sort`. Those transforms do not become
an independent ambient command mode. `cd`, `pwd`, `export`, `unset`, `help`, and
`exit` are available as session controls. Background work, filesystem
commands, operational calls, and unrelated effects remain refused before host
access. An [accepted project plan](../operational/lifecycle.md) is the
controlled route for declared operations.
