# OPAAL language 1 foundation workflow fixtures

These fixtures lock one complete source-only migration and pure OPAAL language 1 workflow.

1. Analyze `../migration/flash-v1/preserved.fsh` with
   `opaal-migrate-flash-v1 --format json`. The
   tool reports the required version directive on stdout and exits 1 without
   writing.
2. Check formatting for `../opaal/preserved.opaal` and `../opaal/source.opaal`, then
   statically check `../opaal/source.opaal`. Each command exits 0 without output.
3. Run `../opaal/source.opaal alpha beta gamma`. The process exits 0 without output,
   while the embedding API retains the final value `Int(2)`.
4. Open `workspace/root.opaal` and its support module in an OPAAL language 1 editor. Direct
   and re-exported operation spellings share the canonical
   `std::value::length` descriptor for completion, hover, signature help,
   formatting, static analysis, and execution. Responses from an older document
   generation are discarded.
5. In the OPAAL language 1 interactive shell, import `std::value` as `value`. The same
   operation completes and executes as `value::length`; its result is presented
   as `2`, and `help value::length` renders the canonical descriptor.

The workflow is pure and uses only explicitly named source files and compiled
standard-module metadata. It performs no project discovery or platform action.
