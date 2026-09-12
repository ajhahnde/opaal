# Editor and project analysis

`opaal-language-server` serves OPAAL over standard input/output. It provides
diagnostics, completion, hover, signature help, definitions, references, and
whole-document formatting without evaluating source or invoking an operational
adapter.

## Select one project explicitly

An editor selects a project only in the initialize request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "capabilities": {},
    "initializationOptions": {
      "opaal": {
        "projectManifest": "file:///absolute/path/to/opaal.toml"
      }
    }
  }
}
```

`projectManifest` must be one absolute local `file:` URI for a bounded regular
file named `opaal.toml`. A relative or non-file URI, a non-string value, a
missing or non-regular file, a symbolic link, an invalid manifest, or another
project selection is rejected as JSON-RPC `InvalidParams`. A manifest whose
`required_opaal` range excludes the running language server is rejected too.
The selection is immutable for the server lifetime; restart the server to
select a different manifest.

The server never derives a project from `rootUri`, workspace folders, parent
directories, process arguments, or workspace configuration. Omitting
`initializationOptions.opaal.projectManifest` therefore keeps standalone
analysis even when an `opaal.toml` exists above the source file.

## Saved source and overlays

Project mode loads the manifest-selected root and its reachable modules through
the same bounded project analyzer used by command-line project checks. Unopened
modules come from regular files beneath the explicit project root without
following symbolic links. An open OPAAL document overlays only the matching
source module for diagnostics and semantic queries. Manifest, authority,
tool-lock, and other control documents always remain disk-bound.

CLI and editor analysis agree when source is saved. An unsaved source buffer is
an editor-local overlay: its diagnostics, definitions, and other answers do not
claim to match a check or plan digest. Open documents outside the selected
project continue to use standalone analysis. Project and standalone analysis
share cancellation, stale-result rejection, and deterministic resource limits.

## Interactive scope

The interactive client evaluates the OPAAL value language. It does not provide
`:project`, `:tasks`, `:inspect`, `:check`, or `:plan` commands. Use the explicit
command-line project workflow and configure the language server as shown above.

[← Documentation index](README.md) · [Actions and explicit projects](actions-and-projects.md) · [Architecture](architecture.md)
