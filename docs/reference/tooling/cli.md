# CLI

`opaal` without arguments starts the interactive client when attached to a terminal. `opaal SCRIPT [ARGUMENT]...` evaluates one explicit `.opaal` root; later UTF-8 operands belong to that script, including option-like values. Successful non-interactive pure evaluation is silent. Use `opaal --help` and a subcommand's `--help` for the parser's current usage text.

The CLI reports these usage forms:

```text
opaal
opaal SCRIPT [ARGUMENT]...
opaal check [--] SOURCE
opaal check --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... [--format json]
opaal check --help
opaal task inspect --project opaal.toml TASK
opaal task --help
opaal plan [--] SOURCE
opaal plan --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... --expires-in SECONDSs --out PATH
opaal plan inspect PATH
opaal plan --help
opaal execute --plan PATH --accept DIGEST [--run-id ID] [--secret-stdin ID] --journal PATH
opaal execute --help
opaal audit --project opaal.toml --journal PATH --out PATH
opaal audit inspect PATH
opaal audit --help
opaal format --check [--] PATH...
opaal format --write [--] PATH...
opaal format --help
```

| Command | Purpose |
| --- | --- |
| `opaal check [--] SOURCE` | Analyze one source closure without execution. |
| `opaal format --check [--] PATH...` | Check formatting of explicit files. |
| `opaal format --write [--] PATH...` | Atomically rewrite explicit files. |
| `opaal task inspect --project opaal.toml TASK` | Show a task's signature and effects. |
| `opaal check --project opaal.toml --task TASK --environment ID …` | Check a selected task and grants; optional `--format json`. |
| `opaal plan --project opaal.toml --task TASK --environment ID … --expires-in SECONDSs --out PATH` | Write one expiring plan. |
| `opaal plan inspect PATH` | Validate and render a plan. |
| `opaal execute --plan PATH --accept DIGEST [--run-id ID] [--secret-stdin ID] --journal PATH` | Run one accepted plan. |
| `opaal audit --project opaal.toml --journal PATH --out PATH` | Validate a journal and write an audit. |
| `opaal audit inspect PATH` | Validate and render an audit. |

Project check and plan accept repeated `--input NAME=VALUE` or `--input-file NAME=PATH` bindings. The selected environment chooses authority and tool lock. `opaal plan [--] SOURCE` remains a host-free `PLAN004` refusal, not a source planning route. A source file never selects a project implicitly.

Diagnostics use standard error and a nonzero exit. A successful check, formatter check, and non-interactive pure run are silent by default. The JSON check form writes one canonical artifact to standard output; plan and artifact inspectors have their own bounded output. Artifact destinations use explicit paths and refuse existing targets. See [project lifecycle](../operational/lifecycle.md).
