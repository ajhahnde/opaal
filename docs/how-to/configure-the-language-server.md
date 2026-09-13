# Configure the language server

Start `opaal-language-server` as a standard-input/standard-output LSP server. Standalone analysis needs no project configuration. To analyze one explicit project, include its absolute local manifest URI in the initialize request:

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

The URI must name a regular, non-symlink `opaal.toml` with a compatible `required_opaal` range. A relative URI, another scheme, invalid file, or malformed manifest gets JSON-RPC `InvalidParams`. The selection cannot change during one server lifetime; restart to use another project. No `rootUri`, workspace folder, parent path, or workspace setting selects it implicitly.

Saved project source agrees with CLI analysis. An unsaved open buffer overlays only its matching source module and produces editor-local results; it does not have a check or plan digest. Control files stay disk-bound, and files outside the project remain standalone. The [LSP reference](../reference/tooling/lsp.md) lists methods and limitations.
