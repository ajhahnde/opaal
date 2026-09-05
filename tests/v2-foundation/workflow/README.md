# Flash 2 foundation workflow fixtures

These fixtures lock one complete source-only migration and pure Flash 2 workflow.

1. Analyze `../v1/preserved.fsh` with `fsh-migrate-v1-v2 --format json`. The
   tool reports the required version directive on stdout and exits 1 without
   writing.
2. Check formatting for `../v2/preserved.fsh` and `../v2/source.fsh`, then
   statically check `../v2/source.fsh`. Each command exits 0 without output.
3. Run `../v2/source.fsh alpha beta gamma`. The process exits 0 without output,
   while the embedding API retains the final value `Int(2)`.
4. Open `workspace/root.fsh` and its support module in a Flash 2 editor. Direct
   and re-exported operation spellings share the canonical
   `std::value::length` descriptor for completion, hover, signature help,
   formatting, static analysis, and execution. Responses from an older document
   generation are discarded.
5. In the Flash 2 interactive shell, import `std::value` as `value`. The same
   operation completes and executes as `value::length`; its result is presented
   as `2`, and `help value::length` renders the canonical descriptor.

The workflow is pure and uses only explicitly named source files and compiled
standard-module metadata. It performs no project discovery or platform action.
