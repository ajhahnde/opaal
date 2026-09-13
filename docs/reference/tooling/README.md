# Tooling reference

The [CLI](cli.md) runs source and provides explicit project commands. The [interactive client](interactive-client.md) evaluates the value language at a terminal. The [language server](lsp.md) analyzes saved source and editor overlays over stdio without execution.

The frontends share parsing and semantic analysis. Project selection is explicit in the CLI and language server; neither infers a manifest from a source file's parent directory. The interactive client has no project-management commands.
