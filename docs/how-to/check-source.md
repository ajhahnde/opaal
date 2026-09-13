# Check source without running it

Run the source checker on one explicit root:

```sh
target/release/opaal check examples/language-foundation.opaal
```

Success is silent. Syntax, import, name, type, signature, carrier, and static effect errors appear as diagnostics on standard error with a nonzero exit. Checking reads the root and its explicit static imports; it does not evaluate them, search for a project, inspect shell history, or discover host tools.

For a task, name the project, task, and environment. Add `--input` for lexical values or `--input-file` for a bounded regular-file snapshot:

```sh
opaal check --project opaal.toml --task welcome --environment local --input name=reader --format json
```

The JSON form writes one canonical `opaal.check.v2` artifact to standard output. It includes request verdicts and findings; it does not read a secret, probe a tool, or call an adapter. A refused check can still explain a missing grant or unsupported host. See [check artifact](../reference/formats/check-artifact.md).
