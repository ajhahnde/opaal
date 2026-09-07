# Lexical golden corpus

[FlashOS](../../../../../README.md) › [Flash](../../../README.md) › Lexical Golden Corpus

`manifest.tsv` is the normative inventory for the v1 lexical contract. Each
row contains the expected classification, a relative source path, and the
expected classification reason.

`complete` means lexically complete, not necessarily accepted by the grammar.
`incomplete` is valid input that needs more source. `invalid` can be rejected
without more source.

Lexer and parser tests consume these files directly. Do not copy the source into
a second test table.

---

[← Flash documentation](../../../docs/README.md)
