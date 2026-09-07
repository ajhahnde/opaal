# OPAAL foundation workflow fixtures

These fixtures lock one direct-source pure OPAAL workflow.

1. Check formatting for the workflow sources, then statically check the root.
   Each command exits successfully without output.
2. Run the root with `alpha beta gamma`. The process exits successfully without
   output while the embedding API retains `Int(2)`.
3. Open `workspace/root.opaal` and its support module in the editor. Direct and
   re-exported operation spellings share the canonical `std::value::length`
   descriptor across completion, hover, signature help, formatting, checking,
   and execution. Stale document generations are discarded.
4. In the interactive client, import `std::value` as `value`. The operation
   completes and executes as `value::length`; its result is presented as `2`,
   and `help value::length` renders the canonical descriptor.

The workflow uses only explicitly named source files and compiled
standard-module metadata. It performs no project discovery or platform action.
