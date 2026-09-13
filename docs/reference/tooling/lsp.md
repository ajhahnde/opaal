# Language server

`opaal-language-server` speaks JSON-RPC over standard input/output. It supports initialize, shutdown, exit, full-document open/change/close, diagnostics, completion, hover, signature help, definitions, references, and whole-document formatting. It does not evaluate source or call an operational adapter. It has no TCP transport or incremental-edit protocol.

An editor may select exactly one project in the initialize request with `initializationOptions.opaal.projectManifest`: an absolute local `file:` URI for a regular file named `opaal.toml`. A relative or non-file URI, non-string value, missing or symlinked file, invalid manifest, or incompatible `required_opaal` range receives JSON-RPC `InvalidParams`. The selection is immutable until the server restarts. `rootUri`, workspace folders, parent directories, process arguments, and workspace configuration never select a project.

In project mode the server loads the saved manifest-selected root and reachable modules through the bounded project analyzer. An open OPAAL document overlays its matching source module only. Manifest, authority, lock, and other control inputs remain disk-bound. Unopened modules come from regular files under the selected root without following symlinks; documents outside that project use standalone analysis.

A saved buffer can agree with CLI check. An unsaved overlay is editor-local and does not claim a check or plan digest. Cancellation and document-generation checks discard stale results. See [configure the language server](../../how-to/configure-the-language-server.md).
