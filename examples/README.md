# OPAAL source examples

Run the commands below from the repository root with the built OPAAL CLI.
The [development guide](../DEVELOPMENT.md) covers the build and first run.
See the [language documentation](https://opaal-lang.org/docs/) for a full
reference.

## Language foundation

The [language foundation example](language-foundation.opaal) exercises value
conditionals, typed tuples and indexed iteration.

Loops evaluate the iterable once. Indexed loops accept lists and ascending
ranges and count from zero, regardless of the first item or range start.
Tuple item patterns unpack the original list element and require matching
tuple arities. Each entry into an indexed or tuple-binding loop consumes an
evaluation step, even when its body is empty. Index and item bindings are
immutable and scoped to that iteration.

## Inline closures for List map

Inline `list::map` closure inference is scheduled for OPAAL 1.11.0. A
typed List supplies the closure parameter type. When the call still needs a
result type, the checker can derive it from a single value expression in
the inline closure.

```opaal
import std::list as list

let marks = list::map([1, 2], {|x| x + 1})
let counts: List[Int] = []
let empty_marks = list::map(counts, {|x| x + 1})
```

Both bindings have type `List[Int]`. At the prompt, enter the import and
bindings in separate submissions and reuse `empty_marks` in a later map.
An empty input invokes the closure zero times.

For a bare `[]` or another input with insufficient element evidence, add a
type annotation or explicit map type arguments. Add `-> Type` when the body
does not establish a concrete result type. Bound and grouped closures use
the existing callback rules. The explicit form
`list::map([1, 2], {|x: Int| -> Int x + 1})` also works in OPAAL 1.10.0.

```sh
target/debug/opaal check tests/golden/source-formatting/list-map/inferred.opaal
target/debug/opaal tests/golden/source-formatting/list-map/inferred.opaal
target/debug/opaal tests/golden/source-formatting/list-map/typed-empty.opaal
```

## List comprehensions

List comprehensions are scheduled for OPAAL 1.11.0. Use a build that includes
the syntax for the saved report below. Projects that distribute comprehension
source should require OPAAL 1.11.0 or later.

```opaal
let counts = [0, 2, 0, 1]
let rows = [{index: i, findings: count} for i, count in counts if count > 0]
```

Write `[projection for binding in iterable]` or append `if filter` before
the closing bracket. The header accepts an item name, a tuple item pattern,
or a position name followed by a comma and either item form. Tuple patterns
can nest and use `_` to skip slots. A List or ascending Range supplies the
items. Positions are zero-based and keep their original values after filtering.

The iterable is evaluated once in the enclosing scope. The projection and
filter can read the immutable header bindings, including when a binder
shadows the iterable's name. The filter must produce Bool. Group conditional
chains explicitly, as in `if (count > 0 && count < 10)`. Each selected item
produces one result. The checker visits every operand, including a skipped
projection, and uses the expected List element type and the projection's
checked type to retain evidence for empty results.

The runtime meters each input visit and checks collection limits and
cancellation before publishing the List. Evaluation stops at the first
failure. Subexpressions use the caller's usual effect and error rules.
Multiline submissions continue until the brackets and operands are complete.
Formatting preserves clause comments. Definitions, references, and completion
use the original header and operand spans.

```sh
target/debug/opaal check tests/golden/source-formatting/list-comprehensions/report.opaal
target/debug/opaal tests/golden/source-formatting/list-comprehensions/report.opaal
target/debug/opaal tests/golden/source-formatting/list-comprehensions/negative.opaal
```

The report checks positions 1 and 3, their findings, and the empty report
summary. The negative input raises an error at the first negative count.
The same policy exports a pure `verify` action used by the source-formatting
project qualification. Changes to its source require ordinary replanning
before accepted execution.

## List and String slices

Use `value[start:end]` with two explicit Int endpoints. The start is inclusive
and the end is exclusive. Both can equal the length, so `items[length:length]`
returns an empty List with the receiver's element type. Strings count Unicode
scalars, including combining marks and supplementary characters.

```opaal
let counts: List[Int] = [10, 20, 30]
let preview = counts[1:3]
let caption = "héllo"[1:3]
let empty = counts[3:3]
```

The evaluator runs the receiver, start, and end from left to right. Valid bounds
satisfy `0 <= start <= end <= length`. For a two-item List, `counts[1:3]` raises
`RUN001` at `3`. Use `counts[1:2]` to select the final item. The checker reports
known receiver and endpoint type errors at their respective expressions.

Run the [preview fixture](../tests/golden/source-formatting/list-string-slicing/preview.opaal)
with a build that supports slicing. Projects distributed with slice syntax
require OPAAL `>=1.11.0,<2.0.0` once 1.11.0 ships.

## Module exports

Place `export` before a module-level `def`, `let`, `mut`, `action`, `type`,
or `enum` to expose that declaration. An inline binding pattern exports
every name it binds in pattern order, including a named rest binding. Use
`export { name }` to select individual names, explicitly re-export an
import, or keep source compatible with older OPAAL versions.

Run the [module exports example](module-exports/main.opaal):

```sh
target/debug/opaal check examples/module-exports/main.opaal
target/debug/opaal examples/module-exports/main.opaal
```

An inline declaration retains its original identity for imports, hover,
definition, and references. Both inline and grouped spellings are preserved
by the formatter. In a direct prompt cell, an inline declaration adds its
ordinary bindings to the session. An imported file exposes its declared API.
`export NAME = value` still assigns an environment variable.

### Export prefixes

Use `export { test_*, Item }` to mix exact names with source-local prefixes.
One trailing `*` selects every top-level declaration whose name starts with the
identifier, anywhere in the file. This includes destructured bindings, functions,
actions and type names; imported aliases/members, nested bindings, task aliases
and enum members are excluded. `check*` also matches `check` itself. Spaces/tabs
may precede the star; formatting binds it to the prefix. A newline, comment or
continuation cannot split the pair. Bare stars, globs and prefix imports are invalid.

Each prefix must match (`MOD005`). Selecting a name twice is `MOD006`, including
overlap with an inline declaration. The later selection is the primary range
and the first is secondary. Inline selections point to the original binder.
Exact imported names can still
be explicitly re-exported. Adding a matching declaration deliberately grows the
public API and invalidates existing source-bound project plans. File imports
resolve concrete original identities. Direct interactive exports remain inert
metadata and never retain a selector for later cells.

## Pure policy tests

Export ordinary non-generic zero-argument Bool functions whose names start with
`test_` using `export def` or a grouped selector such as `export { test_* }`.
List their signatures without initialization, or run them sequentially
in fresh module contexts:

```sh
target/debug/opaal test --list examples/image-check/policy-tests.opaal --format json
target/debug/opaal test examples/image-check/policy-tests.opaal
target/debug/opaal test examples/image-check/policy-tests.opaal --case test_negative_revision
```

Source checking and formatting use the ordinary language rules. Test admission
also requires proven-pure initializers and reachable calls; unresolved dynamic
calls refuse. Tests have no environment snapshot, process, filesystem, I/O or
random grant. A false Bool or uncaught ordinary Error fails a case and continues;
uncaught resource errors, refusal and infrastructure stop the suite. Caught
language Errors retain their existing semantics and consumed counters.
`opaal test --help` owns signature, view, budget and exit details.

## More examples

- [Saved numeric reports](numeric-report/report.opaal)
- [Docker image checks](image-check/README.md)
- [Repository checks with a controlled native tool](repository-check/README.md)
