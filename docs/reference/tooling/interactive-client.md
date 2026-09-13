# Interactive client

Run `opaal` in a terminal to open the interactive client. It evaluates the OPAAL value language and presents completed values. A pure expression can be entered without creating a file:

```opaal
import std::value as value
value::length(["build", "test"])
help value::length
```

Interactive input uses the same parser, visible names, compiled operations, and refusal boundary as ordinary source. Help reads registry and visible-function metadata without executing the named operation. History, hints, completion, highlighting, and terminal editing are client presentation features; they do not create authority or make a project active.

There are no `:project`, `:tasks`, `:inspect`, `:check`, or `:plan` commands in this client. Use the [CLI](cli.md) for project actions and the [language server](lsp.md) for editor diagnostics. Effectful ordinary input is refused before host access.
