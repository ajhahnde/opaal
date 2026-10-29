# Indexed list replacement

These fixtures describe unreleased source behavior. Published OPAAL 1.9.0 does
not accept this syntax; a containing release and minimum version are unassigned.

Use `items[index] = replacement` on one mutable local list binding. The root
must be an unqualified name, with one bracketed expression. Imported, captured
and immutable bindings cannot be updated. Nested targets, indexed compound
assignments, String, Bytes and Record targets are unsupported.

The runtime snapshots the list, evaluates the index once, checks nonnegative
Int bounds, then evaluates the replacement once. A retained `List[T]` requires
an element of T; `List[Any]` admits mixed values, and an Any root or index is
checked at runtime. There is no numeric coercion or type widening. Empty lists
have no valid index; assigning at the length never appends.

Replacement copies the list under the existing work, item, byte and cancellation
limits, then rebinds the original cell once. Work and backing storage are O(n);
unchanged elements share their immutable storage. Saved aliases retain the old
list. Recoverable errors or cancellation before the final store preserve the binding and
its type metadata. Earlier external effects of index or replacement expressions
remain observable and are never retried automatically. Byte admission and
fallible vector reservation protect the configured limits; physical memory
exhaustion in existing Arc/Box constructors may terminate the process and is not
a catchable language error.

`before.opaal` reconstructs a three-element list; `after.opaal` replaces its
middle slot. Both exported tests assert result 1 and an unchanged saved alias.
`alternate.opaal` covers generic and heterogeneous lists; `failure.opaal` covers
caught bounds and dynamic type failures. `output.opaal` prints exactly `ok` plus
LF. I/O cases prove index-before-replacement order, single evaluation and bounds
before replacement effects. Pure-test refusal, raw and terminal continuation,
UTF-8/UTF-16 LSP queries, canonical bytes, and fresh/stale accepted project plans
are asserted by the existing source-formatting qualifier. The project result is
Int 8 with no effect events beyond its normal action lifecycle.

The fixture package checks every filename explicitly. Native archive replay is
required on Linux x86_64, GNU/Linux AArch64 and macOS arm64; a working-source run
does not qualify committed archives.
