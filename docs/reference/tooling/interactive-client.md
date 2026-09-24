# Interactive client

Run `opaal` in a terminal to open the interactive client. It evaluates the OPAAL value language and presents completed values. A pure expression can be entered without creating a file:

```opaal
import std::value as value
value::length(["build", "test"])
help value::length
```

Interactive input uses the same parser, visible names, compiled operations, and
foreground process boundary as an ordinary script. Enter `^program ARG...` to
run an explicit external program; its stdout and stderr reach the terminal and
the prompt returns after the owned foreground group ends. `^program &` and job
control commands remain refused. Help reads registry and visible-function
metadata without executing the named operation. History, hints, completion,
highlighting, and terminal editing do not create project authority.

There are no `:project`, `:tasks`, `:inspect`, `:check`, or `:plan` commands in
this client. Use the [CLI](cli.md) for project actions and the [language server](lsp.md)
for editor diagnostics. Operational calls, filesystem commands, and background
work remain refused before host access.
