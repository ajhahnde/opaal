# Working with the Rust APIs

The six workspace crates are unpublished source components. Build their API
documentation with the pinned toolchain from the repository root.

```sh
cargo doc --workspace --no-deps --locked
```

## Project and syntax APIs

In 1.6, project-model
Rust callers use `ToolDeclaration::contract()` and `LockedTool::contract()` in
place of their former `adapter()` and `version()` getters. Match
`ToolContract::Maintained { adapter, version }` or `ToolContract::Native`, or use
the contract's optional adapter/version accessors; native tools have no version.
The downstream metadata/outcome defaults protected by
`embedding_api_compatibility` remain unchanged. Existing `OperationalAdapter`
implementations inherit snapshot refusal unless they explicitly implement it.

Indexed list replacement adds `StatementKind::IndexedAssignment(IndexedAssignment)`
with original `target`, `index`, `brackets`, `operator` and `value` spans.
Exhaustive statement visitors must handle it, visiting both index and replacement
expressions and resolving the target once as a binding reference. Existing
`Assignment { target, value }` construction is unchanged. Exhaustive
`ModuleTypeError` matches must handle `IndexedAssignmentTargetMismatch` (SIG023)
and `IndexedAssignmentIndexMismatch` (SIG024); incompatible replacements reuse
`AssignmentMismatch` (SIG008). These are unpublished source APIs.

Compound assignment keeps the existing `Assignment { target, value }`
constructor and `StatementKind::Assignment` variant. Parsed `total += amount`
has a binary `value`: its left operand is the original target reference, its
right operand is `amount`, and its `BinaryOperator::Add` node retains the exact
`+=` source span. Existing expression walkers therefore visit the target read
and RHS once. A source consumer that previously assumed every binary operator
span spells a one-character arithmetic token must also accept the corresponding
two-character assignment token. Exhaustive `Operator` matches must handle
`AddAssign`, `SubtractAssign`, `MultiplyAssign`, `DivideAssign` and
`RemainderAssign`; existing AST construction calls need no change. ASTs remain
unpublished source APIs without a serialized or binary compatibility promise.

Member imports extend the unpublished syntax API. `ModuleAliasImport { source,
alias }` and `StatementKind::ModuleImport` keep their existing constructors.
`StatementKind::ModuleMemberImport(ModuleMemberImport { source, members, span })`
represents an origin with selector braces; each `ModuleMemberSelector { member,
local, span }` retains the original exported and local identifier spans.
`ModuleImportGroup` now holds `Vec<ModuleImportEntry>`: wrap existing entries in
`ModuleImportEntry::Alias`, and use `ModuleImportEntry::Members` for selectors.

`Script::module_imports()` and `StatementKind::module_imports()` now iterate
`ModuleImportRef` values. Replace `.source` with `.source()`, use `.alias()` for
an optional original alias entry, and `.members()` for an optional member list.
Call `.end()` for the terminal source offset. Exhaustive statement visitors must
handle `ModuleMemberImport`; grouped visitors can use `.statement_kind()` to
visit each entry. `semantic_statements()` still yields one publication unit per
outer entry, so all selectors in a member declaration are admitted together.
`ModuleExport::target_module()` exposes the original defining module when a
selected name is re-exported. Exhaustive matches on `ModuleAliasError` must also
handle `UnknownMember { module, target, name, span, declaration_span }`, which
reports a missing/private export at its selector and the private declaration
when available. No serialized or binary AST compatibility is promised.

List literals now hold `Vec<ListItem>` in `ExpressionKind::List`. To migrate
an existing `Vec<Expression>` constructor, use
`ExpressionKind::List(expressions.into_iter().map(ListItem::from).collect())`.
An ordinary item is `ListItem::Element(expression)`; an explicit spread is
`ListItem::Spread { marker, operand }`, with separate original marker and operand
spans. Structural visitors call `item.expression()` for either variant; semantic
visitors must distinguish expansion from ordinary insertion. Exhaustive
`ModuleTypeError` matches also handle `ListSpreadValueMismatch { module, span,
actual }`; command spread keeps its existing variants and diagnostics.
No serialized or binary AST compatibility is promised.

Record shorthand keeps the existing `RecordEntry`,
`NominalRecordFieldExpression` and `PatternField` constructors. A parsed
shorthand value is an ordinary `ExpressionKind::Name(NameReference)` whose
identifier and expression spans equal the field token span. A shorthand
pattern uses `Pattern::Binding` at the same token (`Pattern::Wildcard` for
`_`). Existing walkers continue visiting each value or binding once; consumers
must allow a key and its value/binding to share a span. Do not infer two tokens
or insert a colon from the AST: retain source tokens when preserving the chosen
spelling. Lexical queries at that shared token refer to the value or fresh
binding. Nominal field identity remains owned by its record type. No new AST
variant, constructor migration, serialized format or binary promise is added.

## Typed syntax and runtime ingress

The value form of `if` adds `ExpressionKind::If` to the source-tree Rust API.
Exhaustive consumers should visit each arm's `ConditionalChain` and value, then
the fallback expression. The published toolchain ships CLI and language-server
binaries. Workspace crates have publication disabled.

Tuple literals and patterns add `ExpressionKind::Tuple` and `Pattern::Tuple`.
Both carry `TupleSyntax`, which exposes the elements and exact delimiter and
separator spans. Construct named annotations with `TypeReferenceKind::Named`
using a `QualifiedName` and the type arguments. Match
`TypeReferenceKind::Tuple` to visit each tuple type slot. Runtime consumers
handle `Value::Tuple` and `ValueType::Tuple` as ordered slots with distinct
value and type identities. `ValueType::Unknown` and `ValueType::EmptyList`
carry inferred slot evidence. An explicit `Any` remains distinct.

`ForStatement` carries an optional `index` identifier and a `binding` pattern.
Build the existing one-name header with `index: None` and
`Pattern::Binding(identifier)`. For tuple items, visit each tuple slot as a
pattern and preserve its spans. A tuple item contains only nested tuples, names,
and wildcards. Walk the iterable before introducing the loop bindings, then
visit the body with both bindings in the same scope. Visitors of
`RuntimeErrorKind` and `ModuleTypeError` should handle `LoopIndexOverflow` and
`NotIterable`, respectively.

Use `opaal_syntax::check_tuple_depth` to check a Rust-built syntax tree before
passing it to a recursive visitor. Pass borrowed values to `apply_callable`,
`ScopeStack::declare`, `ScopeStack::assign`, `OperationDescriptor::execute_value`
and `execute_value_with_types`. A session takes an already-populated
`ScopeStack`. These entry points check Rust input before cloning it.

Construct eager `ValueStream`s and stream schemas from borrowed input and handle
their result. Lazy value producers, queues, and closure transformers take
`AdmittedValue` created from a borrowed value. Producer failures use
`AdmittedStreamError`. The producer retains responsibility for releasing any
rejected graph and anything captured in opaque host state. Raw Rust traits such
as `Value` equality and formatting still require caller-bounded inputs.

`Callable` now requires `Any`. Use owned state or `'static` references in a
custom implementation so it can be stored as a runtime callable.
`execute_internal_suffix` accepts `InternalPayload<AdmittedValue>`. For a
single-value head result, first call `AdmittedValue::new(&value)?`, then pass
it as `InternalPayload::Value`.

Runtime paths with tuples allow 32 composite levels. Rust ingress also limits
recursive value and metadata paths to 64 edges and caps visited edges at the
default evaluation step limit. Capsule and terminal formats apply their own
limits during decode and rendering.

## Comprehension and slice visitors

Rust AST visitors should handle `ExpressionKind::ListComprehension`.
`ListComprehension::operands` visits the iterable, optional filter, and
projection. A scope-aware visitor adds the original header bindings after
visiting the iterable, and uses one binder identity for the filter and
projection. A structural path containing a comprehension can contain up to
32 levels, including enclosing statement blocks and recursive statement
`else if` edges. This applies to source and Rust-built ASTs.

Rust AST visitors can use `SliceExpression::operands()` to visit the receiver,
start, and end in order. Exhaustive matches include `ExpressionKind::Slice`,
`TupleDepthError::SliceExceeded`, and `ModuleTypeError` cases
`SliceTargetMismatch` (`SIG031`) and `SliceEndpointMismatch` (`SIG032`). Paths
with a slice can contain up to 32 structural levels, including statement
blocks and `else if` edges. The limit also applies to Rust-built ASTs.

## Export and action spans

Rust callers can use `Script::inline_export_span(statement)` to obtain the
modifier range. Statement kinds, declaration spans, and constructors are
unchanged.

The unpublished Rust syntax API changes `ModuleExportStatement.names` to
`Vec<ModuleExportSelector>`. Wrap existing identifiers in `Name(identifier)`;
`Prefix { prefix, marker }` retains both tokens, and `.span()` covers the selector.
Visitors must handle both variants. `ModuleExport::is_prefix()` distinguishes
generated selections. `export_span()` returns the originating selector or binder range.
Prefix tokens have no singular hover/definition target, and concrete reference
searches omit them. Exhaustive `ModuleNameError` matches must handle
`UnmatchedExportPrefix`. There is no serialized/binary AST compatibility promise.

`ActionDefinition` retains its fields: `body.span` covers the outer action braces,
`effects_span` covers the nested keyword and block, and `body.statements` contains
only executable statements. Each `EffectRequest.span` ends at its last request
token, excluding the comma and following trivia. Effects remain static metadata;
their textual nesting supplies neither local scope nor authority. The superseded
detached effects/body form is invalid and the formatter never converts it.

[Development guide](../DEVELOPMENT.md)
