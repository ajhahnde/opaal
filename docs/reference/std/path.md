# `std::path`

`std::path` exports `normalize(input: Path) -> Path`, `join(root: Path, child: String) -> Path`, and `contained(root: Path, target: Path) -> Path`.

Paths retain native Unix bytes. Normalization is lexical: it does not open a file, resolve a symbolic link, or establish physical containment. `join` rejects an absolute child and parent traversal that would escape the named root. `contained` checks a lexical relationship; an adapter repeats containment against retained directory descriptors before any physical file access.

Do not use a normalized path as a substitute for authority. A filesystem effect needs an exact path request, an applicable grant, and an enforcing adapter. See [filesystem access](filesystem.md) and [authority](../operational/authority.md).
