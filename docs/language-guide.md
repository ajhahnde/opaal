# Flash Language Guide

[FlashOS](../../../README.md) › [Flash](../README.md) › [Documentation](README.md) › Language Guide

This guide documents the Flash 1.0 language: source text, runtime values, bindings, expressions, commands, expansion, control flow, functions, function metadata, modules, name resolution, and typed pipelines. Practical script invocation, process execution, redirection, static checking, formatting, status handling, and job control are covered in the [Scripting Guide](scripting.md).

> **Project status:** FlashOS as a complete operating system remains pre-alpha software. However, this Flash Language Guide defines the intended stable Flash v1.0 contract. Note that not every v1 feature is automatically available in every current FlashOS image or on every target platform, and successful execution on a Linux or macOS development host is not automatic proof of FlashOS target support.

## On this page

- [Language model](#language-model)
- [Source text and statements](#source-text-and-statements)
- [Values and literals](#values-and-literals)
- [Bindings, environment, and scope](#bindings-environment-and-scope)
- [Expressions and operators](#expressions-and-operators)
- [Commands and argument expansion](#commands-and-argument-expansion)
- [Control flow](#control-flow)
- [Functions and closures](#functions-and-closures)
- [Modules and name resolution](#modules-and-name-resolution)
- [Pipelines and structured data](#pipelines-and-structured-data)
- [Status values and errors](#status-values-and-errors)
- [Related documentation](#related-documentation)

## Language model

Flash is a non-POSIX command language. It is not intended to parse or execute `sh`, Bash, or other POSIX shell programs.

The language follows several central rules:

- Source text is parsed as Flash syntax rather than passed through another shell.
- Runtime values retain their types until an explicit conversion is requested.
- An ordinary command word produces exactly one argument.
- Variables are not implicitly split on whitespace.
- Wildcard characters are not expanded implicitly.
- Collections are expanded into command arguments only through explicit spread syntax.
- External processes exchange byte streams; structured pipeline stages use typed values.
- Conditions accept only Boolean or status values rather than applying general truthiness rules.

These rules keep data, command arguments, and executable source distinct. A string produced at runtime is data and is not reparsed as Flash code.

## Source text and statements

### Encoding and line endings

Flash source files are UTF-8 text. Both LF and CRLF line endings are accepted.

A lone carriage return is invalid. Flash scripts conventionally use the `.fsh` extension.

Statements are normally separated by a newline:

```text
let name = "Flash"
let version = 1
```

A semicolon may separate statements on the same line:

```text
let name = "Flash"; let version = 1
```

A backslash followed immediately by a line ending continues the current logical line. Open delimiters and unfinished operators may also cause interactive input to remain incomplete until the construct is closed.

### Identifiers and reserved words

Identifiers use ASCII letters, digits, and underscores. The first character must be a letter or underscore.

```text
name
user_name
_value2
```

Unicode text is supported in strings, paths, comments, and command words, but not in identifier names.

The following words are reserved:

```text
break
continue
def
else
export
false
for
if
import
in
let
match
mut
null
return
true
unset
while
```

### Comments

A comment begins with `#` when the character appears where a new token may begin, such as at the start of a line or after whitespace:

```text
# A complete-line comment
let retries = 3 # An end-of-line comment
```

A `#` embedded in an existing word is part of that word rather than the start of a comment.

A complete line whose first non-horizontal-whitespace characters are `##` is
a documentation comment. Consecutive documentation lines attach only to the
named function on the immediately following physical line:

```text
## Return a greeting.
##
## The name is preserved as Unicode text.
def greet(name: String) -> String {
    "Hello, $name"
}
```

The `##` marker and at most one following ASCII space are removed from the
stored prose. An empty documentation line creates a paragraph break. A blank
line, ordinary `#` comment, or another statement breaks attachment. Inline,
detached, and orphaned `##` text remains an inert ordinary comment for
compatibility. Documentation attaches to named functions at any lexical depth,
not to closures, bindings, modules, imports, exports, or commands.

### Quoting

Flash provides bare, single-quoted, and double-quoted words.

| Form           | Behavior                                                        |
| -------------- | --------------------------------------------------------------- |
| `plain-text`   | Literal text with supported interpolation and backslash quoting |
| `'plain text'` | Exact text without escapes or interpolation                     |
| `"plain text"` | Text with escapes and interpolation                             |

Single-quoted text is literal:

```text
let example = '$name\n'
```

The value contains the characters `$name\n`; neither interpolation nor escape processing occurs.

Double-quoted text supports these escapes:

| Escape    | Result               |
| --------- | -------------------- |
| `\\`      | Backslash            |
| `\"`      | Double quote         |
| `\$`      | Literal dollar sign  |
| `\n`      | Line feed            |
| `\r`      | Carriage return      |
| `\t`      | Horizontal tab       |
| `\0`      | Null character       |
| `\u{...}` | Unicode scalar value |

Example:

```text
let message = "first line\nsecond line"
```

In a bare word, a backslash quotes the following Unicode scalar so that it becomes literal text.

### Interpolation

Bare and double-quoted words support three interpolation forms:

| Form            | Meaning                        |
| --------------- | ------------------------------ |
| `$name`         | Read a binding                 |
| `${expression}` | Evaluate an expression         |
| `$(command)`    | Capture command output as text |

Example:

```text
let project = "FlashOS"
let major = 1
let label = "$project ${major}.0"
```

Interpolation does not reparse its result, split it on whitespace, or perform wildcard expansion. Adjacent literal and interpolated parts are concatenated into one value.

Command substitution evaluates one foreground command or conditional chain:

```text
let location = "$(pwd)"
```

Its standard output must be valid UTF-8. Trailing LF and CRLF line endings are removed, and the remaining text becomes one `String` value. The result is not reparsed as source or divided into multiple arguments.

Process behavior and substitution failures are described in the [Scripting Guide](scripting.md).

## Values and literals

Flash evaluates source into structured runtime values.

| Value family | Purpose                                         |
| ------------ | ----------------------------------------------- |
| `Null`       | Absence of a value                              |
| `Bool`       | `true` or `false`                               |
| `Int`        | Signed integer                                  |
| `Float`      | Finite floating-point number                    |
| `String`     | UTF-8 text                                      |
| `Bytes`      | Arbitrary binary data                           |
| `Path`       | Native filesystem path                          |
| `Duration`   | Signed duration                                 |
| `ByteSize`   | Non-negative byte quantity                      |
| `List`       | Ordered sequence of values                      |
| `Record`     | Ordered mapping from field names to values      |
| `Table`      | Structured tabular data                         |
| `Range`      | Integer range                                   |
| `Status`     | Result of command execution                     |
| `Function`   | Named Flash function                       |
| `Closure`    | Anonymous callable value with captured bindings |

### Scalar literals

```text
null
true
false
42
-17
3.5
"Flash"
'exact text'
```

Integers use checked signed arithmetic. An operation that exceeds the supported integer range produces a runtime error.

Floating-point values are finite. Non-finite values such as NaN and positive or negative infinity are not regular Flash values.

### Lists

A list contains an ordered sequence of values:

```text
let names = ["Ada", "Grace", "Linus"]
let first = $names[0]
```

List elements may have different runtime types.

### Records

A record associates ordered field names with values:

```text
let project = {
    name: "Flash",
    version: 1,
    stable: true
}

let name = $project.name
let version = $project["version"]
```

Member access and string indexing address record fields. Record order is preserved.

### Ranges

An exclusive range uses `..`:

```text
1..5
```

It represents the integers `1`, `2`, `3`, and `4`.

An inclusive range uses `..=`:

```text
1..=5
```

It also includes `5`.

Ranges advance in ascending unit steps. A range whose start is greater than its end is empty.

### Immutability

Runtime values are immutable. A mutable binding permits reassignment of the binding; it does not make the bound list, record, or other value mutable in place.

Operations that conceptually modify structured data produce a new value.

### Equality and ordering

Equality and inequality are defined across the value model. Values of different families compare as unequal rather than being converted implicitly.

Ordering comparisons are available only for compatible ordered domains, including:

- numbers;
- strings;
- byte sequences;
- paths;
- durations;
- byte sizes;
- lists whose elements can be compared.

Ordering unrelated or unsupported value families produces a runtime error.

## Bindings, environment, and scope

### Immutable bindings

Declare an immutable binding with `let`:

```text
let base = 10
let total = $base + 5
```

The binding name is written without `$` in the declaration. Reading the binding requires `$`.

### Mutable bindings

Declare a reassignable binding with `mut`:

```text
mut total = 10
$total = $total + 5
```

Assignment is a statement and also requires the `$` prefix on its left-hand side.

Assigning to an immutable binding is a runtime error.

Declarations and function signatures may include type references:

```text
let value: Type = expression

def convert(input: Type) -> Type {
    expression
}
```

### Lexical scope

Blocks introduce child scopes:

```text
let value = 1

if true {
    let value = 2
    let inside = $value
}

let outside = $value
```

A child scope may shadow a name from an enclosing scope. Declaring the same name twice in the same scope is an error.

Assignment updates the nearest visible mutable binding with that name.

Bindings declared inside a block are not visible after the block ends.

### Process environment

Lexical bindings and process environment variables are separate namespaces.

Use `export` to set a value in the environment inherited by later external processes:

```text
export EDITOR = "fsh"
```

Use `unset` to remove an environment entry:

```text
unset EDITOR
```

Exporting a name does not create a normal lexical binding, and declaring a lexical binding does not automatically export it.

## Expressions and operators

### Variable reads and grouping

Read a binding with `$name`:

```text
let width = 20
let doubled = $width * 2
```

Parentheses group expressions:

```text
let result = (2 + 3) * 4
```

### Calls, indexing, and members

Named functions use direct call syntax:

```text
calculate(1, 2)
```

A callable value stored in a binding is called through that binding:

```text
$operation(1, 2)
```

Postfix operations may be combined:

```text
$records[0].name
```

### Operator precedence

From highest to lowest precedence:

| Level          | Operators                              |
| -------------- | -------------------------------------- |
| Postfix        | Function call, indexing, member access |
| Unary          | `!`, unary `+`, unary `-`              |
| Multiplicative | `*`, `/`, `%`                          |
| Additive       | `+`, `-`                               |
| Range          | `..`, `..=`                            |
| Relational     | `<`, `<=`, `>`, `>=`, `in`             |
| Equality       | `==`, `!=`                             |

The conditional operators `&&` and `||` operate on complete expressions or command chains rather than acting as general value-producing arithmetic operators. `&&` binds more tightly than `||`.

### Arithmetic

Arithmetic operators accept numeric values. Flash does not implicitly parse strings as numbers or concatenate strings through numeric addition.

Integer division rounds toward negative infinity. Division by zero and checked-arithmetic overflow are runtime errors.

### Membership

The `in` operator supports defined membership relationships:

```text
let selected = 3 in 1..=5
let known = "fsh" in ["fsh", "sh"]
let contained = "Shell" in "Flash"
let has_name = "name" in {name: "Flash"}
```

Supported forms include:

- an integer in a range;
- a value in a list;
- a substring in a string;
- a field name in a record.

Unsupported operand combinations produce a runtime error.

### Conditions

Conditions must evaluate to `Bool` or `Status`.

```text
if $count > 0 {
    let state = "non-empty"
}
```

There is no general truthiness conversion. In particular, these values are not automatically false:

- `null`;
- integer zero;
- an empty string;
- an empty list;
- an empty record.

A successful status acts as true; an unsuccessful status acts as false.

## Commands and argument expansion

### Command recognition

A non-reserved identifier followed immediately by `(` begins a function call:

```text
process($value)
```

The same identifier followed by command words begins a command:

```text
process $value
```

Bare command names are resolved against Flash internal commands first. If no internal command matches, Flash attempts to start an external executable.

Prefix a command name with `^` to require external execution:

```text
^program argument
```

The prefix is part of Flash syntax and is not included in the external argument vector.

For a command name selected at runtime, use the `command` internal command:

```text
let program = "example-program"
let arguments = ["--mode", "check"]

command $program ...$arguments
```

The standard namespace classifies spellings as canonical core commands,
invocable migration aliases, or reserved names protected from implicit
external fallback. The Flash v1 standard manifest begins with no aliases and no
reserved names.

The Flash v1 core command inventory is:

```text
bg cd check collect command decode each encode exit fg first from get help jobs
kill last length lines ls open pwd save select sort to update wait where which
```

`export` and `unset` are statement keywords rather than command-namespace
entries. Core and alias entries may carry introduction, deprecation, and
replacement metadata. An alias always targets one core entry and reuses its
signature and behavior; a reserved entry is not executable as an internal
command and blocks bare-name `PATH` fallback. `^name` and `command name` bypass
every namespace class for explicit external execution.

The namespace is part of the language-major contract:

| Change | Compatibility class |
| --- | --- |
| Preserve an entry's name, class, canonical target, signature, and behavior while changing implementation or documentation | Compatible within the language major |
| Add deprecation metadata without changing runtime behavior or canonical identity | Compatible within the language major |
| Activate a name reserved at the start of the language major | Compatible within the language major |
| Add a core command, alias, or reservation under a previously unknown name | Next language major |
| Remove or rename a core command; remove or retarget an alias; or release a reservation | Next language major |
| Change namespace class except when activating an existing reservation | Next language major |
| Alter a command contract in a way that can change an existing successful program | Semantic review and normally the next language major |

This classification concerns Flash source semantics, not whether a particular
host currently has a same-named executable. Static analysis therefore never
probes `PATH` to decide whether a namespace change is safe.

### Ordinary command words

Each ordinary command word produces exactly one argument.

```text
let label = "two words"
command "example-program" $label
```

The value of `$label` remains one argument. Flash does not split it at the space.

An ordinary interpolated value must be one of these scalar families:

- `Bool`;
- `Int`;
- `Float`;
- `String`;
- `Path`;
- `Duration`;
- `ByteSize`.

Values such as `Null`, `Bytes`, `List`, `Record`, `Table`, `Range`, `Status`, `Function`, and `Closure` cannot be inserted into an ordinary command word implicitly.

An empty string still produces one empty argument. `null` does not mean “omit this argument”; using it as an ordinary command word is a type error.

Quoting changes how source text is interpreted, but it does not change argument cardinality.

### Explicit spread

Spread a list into separate arguments with `...$name`:

```text
let arguments = ["--mode", "check", "input.fsh"]
command "example-program" ...$arguments
```

Spread syntax must appear as a standalone command word. The binding must contain a `List`, and every list element must be eligible for conversion to an individual command argument.

An empty list contributes zero arguments. Nested lists are not flattened recursively.

### Explicit globbing

Wildcard characters in ordinary words are literal:

```text
command "example-program" "*.fsh"
```

Use `glob` when filesystem pattern matching is intended:

```text
let files = glob("scripts/**/*.fsh")
command "example-program" ...$files
```

`glob` returns path values. Expansion into separate command arguments remains explicit through `...`.

### No source reparsing

Neither interpolation nor command substitution can introduce new syntax.

For example, a string containing spaces, pipes, redirection characters, or semicolons remains one data value when inserted into an ordinary command word. It cannot create additional arguments, pipeline stages, redirects, or statements.

## Control flow

### Conditional branches

```text
if $count > 0 {
    let description = "non-empty"
} else {
    let description = "empty"
}
```

An `else` branch may contain another `if` statement.

### Conditional chains

`&&` evaluates its right side only when the left side is true or successful:

```text
first_step && second_step
```

`||` evaluates its right side only when the left side is false or unsuccessful:

```text
primary_step || fallback_step
```

Runtime errors and cancellation are not ordinary unsuccessful statuses and do not trigger the right side of `||`.

Because `&&` binds more tightly than `||`, use parentheses whenever grouping should be explicit.

### While loops

```text
mut index = 0

while $index < 3 {
    $index = $index + 1
}
```

The condition is evaluated before each iteration and must produce a Boolean or status value.

### For loops

`for` iterates over a list or range:

```text
for name in ["parser", "runtime", "platform"] {
    let current = $name
}
```

```text
for number in 1..=3 {
    let current = $number
}
```

The loop variable is scoped to the loop body.

### Break and continue

`break` exits the nearest enclosing loop:

```text
while true {
    break
}
```

`continue` starts the next iteration of the nearest enclosing loop:

```text
for number in 1..=5 {
    if $number == 3 {
        continue
    }
}
```

Using either statement outside a loop is an error.

### Match expressions

`match` evaluates its input once and selects the first matching arm:

```text
match $code {
    0 => {
        "success"
    }

    value if $value > 0 => {
        "non-zero"
    }

    _ => {
        "other"
    }
}
```

Supported pattern forms include:

- literal patterns;
- identifier patterns that bind the matched value;
- `_` as a wildcard.

An arm may include an `if` guard. The guard may refer to names introduced by that arm's pattern.

Arms are considered from top to bottom. A `match` with no matching arm produces a runtime error.

## Functions and closures

### Named functions

Define a named function with `def`:

```text
def add(left, right) {
    $left + $right
}

let result = add(2, 3)
```

Parameters are local bindings. Function bodies have their own lexical scope.

Use `return` for an explicit result:

```text
def absolute(value) {
    if $value < 0 {
        return -$value
    }

    $value
}
```

Without an explicit `return`, the value of the final expression becomes the function result. A body with no resulting expression returns `null`.

Named functions may call themselves:

```text
def factorial(number) {
    if $number <= 1 {
        return 1
    }

    $number * factorial($number - 1)
}

let result = factorial(5)
```

### Closures

A closure is an anonymous callable value:

```text
let increment = {|value| $value + 1}
let result = $increment(4)
```

A closure with no parameters uses an empty parameter list:

```text
let current = {|| pwd}
```

A closure body contains one expression, command, pipeline, or conditional chain rather than a general list of statements.

Closures capture referenced lexical bindings by value:

```text
let offset = 10
let add_offset = {|value| $value + $offset}
```

Later reassignment of an outer mutable binding does not change the captured snapshot. Captured bindings are immutable inside the closure.

Closures are commonly passed to structured commands:

```text
ls | where {|entry| $entry.type == "file"}
```

### Typed function metadata

Bindings, named-function parameters and results, and closure parameters may use
type annotations:

```text
let retries: Int = 3
mut labels: List[String] = ["stable"]

def greet(name: String) -> String {
    "Hello, $name"
}

let normalize = {|value: String| $value}
```

Type names are case-sensitive. The closed built-in namespace is:

```text
Any Null Bool Int Float String Bytes Path Duration ByteSize
List[T] Record Table Range Status Function Closure
```

`List` requires exactly one type argument; the other built-in types accept no
type arguments. `Any` accepts every runtime value. An omitted binding,
parameter, or named-function result annotation has the same unrestricted
runtime contract as `Any`. There are no user-defined types, unions, implicit
coercions, or callable generics in this boundary. `Function` and `Closure` are
distinct value families.

Canonical program construction resolves every annotation in every loaded
module, including dormant load-only modules. It exposes source-spanned
annotation records and named-function signatures without executing source.
Unknown names and invalid type-argument counts are analysis errors.

For a statically resolved local or imported named function, analysis always
checks call arity. It checks argument and result compatibility when their types
are conservatively known from literals, annotations, script input, closures,
known signatures, or supported operators. Untyped or data-dependent
information remains unknown rather than being guessed, so dynamic cases are
deferred to runtime.

Runtime enforcement is exact and performs no conversion. Annotated declaration
initializers and later assignments must match their binding type. Function and
closure arguments are checked before body entry. A named function's explicit
result, implicit final value, or `null` fallthrough must match its result type.
`List[T]` checks every element recursively; an empty list satisfies every
`List[T]` contract. Closures have typed parameters but no result annotation in
the current grammar.

### Documentation comments and help

Attached `##` blocks are normalized into the canonical signature metadata for
their named function. The first nonempty normalized line is its summary; the
complete text is its detail body. Unicode and interior blank lines are
preserved, and no markup language is interpreted.

Use the language command `help` to list standard built-ins and currently
visible named functions, or `help NAME` for every exact, case-sensitive match:

```text
help
help greet
```

The list includes each entry's kind, canonical signature or invocation, and
summary. Core entries use kind `builtin`. Alias entries use kind `alias`, name
their canonical target, and share the target's invocation, carriers, flags, and
prose. Reserved entries use kind `reserved`, show their purpose and optional
replacement, and claim no executable signature. Deprecated core and alias
entries expose their release and optional replacement. Detailed function output
shows resolved parameter/result types and defining location. An undocumented
named function is still inspectable and is identified as `undocumented`.
Command entries and functions occupy distinct namespaces, so both entries are
shown when they share a name.

Runtime lookup follows ordinary lexical visibility and shadowing. A visible
non-callable hides an outer function of the same name, imported functions keep
their defining metadata, and anonymous closures are omitted. Results are
ordered by name and then kind.

`help` accepts only an empty pipeline input and returns a UTF-8 `ByteStream`
ending in one newline, so it can be captured, piped, or redirected through the
ordinary byte path. Its optional query must be one static source word; variable
interpolation, command substitution, spreads, closures, and additional
arguments are rejected.

Help is inspection-only. Planning snapshots immutable entries from the command
registry and current scope; rendering does not execute a function body, probe
an executable, access a platform capability, or mutate session state.

## Modules and name resolution

Flash v1 supports maintainable multi-file programs through explicit module boundaries. Module loading and name resolution are analysis responsibilities that must be understandable before program execution begins.

### Canonical module identity

A module is identified through a canonical resolved path rather than the spelling of an individual import. Different relative paths that resolve to the same source file refer to the same module identity.

Resolution errors must identify both the importing source and the requested module path. Import cycles must be rejected with diagnostics that show the relevant cycle rather than failing later through partial execution.

### Explicit imports and exports

Names cross module boundaries only through explicit imports and exports. A module does not mutate unrelated caller scopes, and importing a module does not create ambient wildcard access to all of its internal bindings.

A static source dependency uses a top-level import declaration:

```text
import './lib/math.fsh'
```

The path is one nonempty single-quoted literal. It is resolved relative to the importing module unless it is absolute. Import paths are exact and static: they do not interpolate values, expand environment entries, apply globbing, add an implicit extension, or execute source to discover another module. An import is a module-level declaration and is invalid inside a function or control-flow block.

This load-only form adds the source to the analyzed module graph but binds no name and runs no module initialization. Explicit imported-name and exported-name syntax is a separate part of the module contract; the load-only declaration never creates wildcard ambient access.

When `fsh` runs a script file, it canonicalizes and loads the complete static
dependency graph before executing the root source. A load, UTF-8, syntax, or
cycle failure ends the script before any root statement runs, and diagnostics
identify the registered source files involved. Imported sources remain
analysis-only in this form; their statements are not executed.

Expose top-level lexical declarations and functions with an explicit export
list:

```text
let answer = 42
def add(left, right) { return $left + $right }
export { answer, add }
```

Import only the names a source needs:

```text
import { answer, add } from './lib/math.fsh'
```

Both lists are nonempty identifier lists and may include a trailing comma.
Exports and named imports are allowed only at module top level. An exported name
must refer to a top-level `let`, `mut`, or `def` in the same source. A named
import must identify an explicit target export and cannot replace a local or
earlier imported binding. Aliases, re-exports, wildcard names, expressions in
name lists, and dynamic paths are not part of this boundary.

Module export lists are distinct from `export NAME = value`, which continues to
write the child-process environment. Internal bindings remain private unless a
module export list names them.

Module-name analysis builds deterministic tables and diagnostics without
evaluating dependencies. Script execution then activates only named-import
edges. Named dependencies initialize once per canonical module in deterministic
source-edge depth-first postorder, before the importing module; a source reached
only through a load-only import remains dormant.

Each initialized module owns an isolated lexical root. After a dependency
completes, its exported values are copied into the importer as immutable
snapshots, so an importer cannot assign through an imported binding or observe a
live mutable cell. Working directory, child-process environment, status, output,
processes, and background jobs remain shared across the program in the defined
initialization order. Imported functions and closures retain their defining
source for body evaluation and diagnostics.

### Static module analysis

Canonical program construction resolves lexical reads in every loaded module without executing source, including a module reached only through a load-only import. Resolution follows source-order declaration visibility and the evaluator's block, loop, match-arm, function, parameter, closure-capture, recursion, and shadowing scopes. Unknown reads and duplicate bindings in one scope stop construction with source-anchored diagnostics; a child scope may shadow an outer binding.

The program-owned reference table retains each complete read span and its local declaration. A reference to an imported binding also retains the local import identifier and the target module's declaration and explicit export spans. Record and member keys, process-environment names, literal command text, and type references remain distinct namespaces rather than lexical reads. Resolved type annotations and named-function signatures occupy a separate program-owned registry; assignment-mutability analysis remains separate work.

The module graph, exported names, imported names, and cross-file lexical references are therefore available to non-executing shared analysis. Checker, help, editor, and language-server frontends can consume that information without maintaining a competing resolver.

Name resolution must be deterministic and source-anchored. Missing names, duplicate declarations, inaccessible private names, incompatible signatures, and import cycles must produce diagnostics without relying on side effects from program execution.

## Pipelines and structured data

### Pipeline operators

| Operator                 | Behavior                                                          |
| ------------------------ | ----------------------------------------------------------------- |
| <code>&#124;</code>      | Connect the standard output of one stage to the next              |
| <code>&#124;&amp;</code> | Connect both standard output and standard error to the next stage |
| `&&`                     | Continue only after a true or successful result                   |
| <code>&#124;&#124;</code>| Continue only after a false or unsuccessful result                |
| trailing `&`             | Run the complete conditional chain as a background job            |

Redirection syntax, pipeline status aggregation, background jobs, and terminal control are documented in the [Scripting Guide](scripting.md).

### Pipeline carriers

Flash distinguishes four pipeline carrier forms:

| Carrier       | Meaning                      |
| ------------- | ---------------------------- |
| `Empty`       | No pipeline payload          |
| `ByteStream`  | Streaming bytes              |
| `Value`       | One structured runtime value |
| `ValueStream` | Streaming structured values  |

External processes consume and produce byte streams. Internal commands declare the carrier forms they accept and return.

A pipeline may alternate between external byte-stream stages and internal typed stages any number of times. Each edge is checked independently; there is no language-level limit of one internal stage island. Every transition must still use carriers accepted by both adjacent stages.

Flash does not automatically:

- decode bytes as text;
- parse serialized data;
- serialize records or tables;
- wrap bytes in structured values;
- collect a stream into a list;
- flatten structured values into command arguments.

An incompatible pipeline edge is an error rather than an implicit conversion.

### Explicit conversion boundaries

Use explicit commands to cross representation boundaries:

| Command family | Boundary                              |
| -------------- | ------------------------------------- |
| `decode`       | Bytes to textual values               |
| `encode`       | Textual values to bytes               |
| `from`         | Serialized bytes to structured values |
| `to`           | Structured values to serialized bytes |
| `collect`      | A value stream to a materialized list |

For example, a JSON transformation makes both parsing and serialization visible:

```text
open users.json \
    | from json \
    | where {|user| $user.active} \
    | select name email \
    | sort name \
    | to json \
    | save active-users.json
```

### Structured filesystem data

The internal `ls` command produces structured entries rather than terminal-formatted lines. Entries expose fields including:

- `name`, as a path;
- `type`, such as `file`, `dir`, `symlink`, or `other`;
- `size`, as a byte size or `null` when unavailable.

Structured commands can consume those values directly:

```text
ls \
    | where {|entry| $entry.type == "file"} \
    | select name size \
    | sort name
```

Use `^ls` when the external executable named `ls` is required instead of the internal structured command.

### Common structured operations

Flash provides internal operations for common data-flow tasks, including:

- `first` and `last`;
- `collect` and `length`;
- `lines`;
- `each` and `where`;
- `select`, `get`, and `update`;
- `sort`;
- `open` and `save`;
- `decode`, `encode`, `from`, and `to`.

Their accepted carriers and output values are part of their command contracts. A stage must receive a compatible carrier.

Structured values displayed in an interactive terminal are rendered for human inspection. That presentation is not a stable serialization format; use `to` or `encode` when bytes in a defined format are required.

## Status values and errors

### Status values

A completed command or pipeline produces status information rather than treating every non-zero exit as a language failure.

A `Status` records information such as:

- whether execution succeeded;
- an optional exit code;
- an optional terminating signal;
- per-stage pipeline status;
- execution duration.

A successful status behaves as true in a condition. An unsuccessful status behaves as false.

An unsuccessful status is not automatically a runtime error. Use the `check` command when unsuccessful execution must terminate the current evaluation as an error.

### Error categories

Flash distinguishes several kinds of failure:

- **Incomplete source** needs more input, such as an unclosed delimiter in an interactive session.
- **Invalid source** cannot form a valid Flash program.
- **Runtime errors** include unknown bindings, invalid operand types, unsupported conversions, arithmetic failures, and invalid pipeline edges.
- **Unsuccessful statuses** represent commands that completed without success.
- **Cancellation** represents interrupted execution.

Conditional fallback with `||` handles a false Boolean or unsuccessful status. It is not a general runtime-error handler.

Diagnostics preserve source locations where available so that syntax and evaluation failures can identify the relevant part of the program.

## Related documentation

- [Scripting](scripting.md) — Run `.fsh` files, pass script arguments, perform non-executing `fsh check` validation, apply canonical formatting, invoke external processes, redirect streams, inspect statuses, and manage jobs.
- [Architecture](architecture.md) — Understand parser, runtime, platform, and CLI boundaries.
- [Development](development.md) — Build, test, lint, fuzz, and maintain the Flash workspace.
- [Flash overview](../README.md) — Review the component's role in FlashOS and its implementation boundaries.
- [FlashOS Verification](../../../docs/verification.md) — Distinguish host tests, target compilation, image construction, and runtime qualification.

---

[← Previous: Documentation Index](README.md) · [Flash documentation](README.md) · [Next: Scripting →](scripting.md)
