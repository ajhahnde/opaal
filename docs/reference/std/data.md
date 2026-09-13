# `std::data`

`std::data` exports `toml_decode(input: Bytes) -> Any`, `get(input: Any, keys: List[String]) -> Any`, and `json_encode(input: Any) -> Bytes`. Import it as `import std::data as data`.

`toml_decode` parses bounded UTF-8 TOML. Duplicate keys and malformed types are errors; nesting stops at 64 and input stops at 16 MiB. A TOML datetime needs an explicit time conversion before it becomes a JSON-compatible value. `get` follows a list of record keys through a decoded value; it is a structured lookup, not a filesystem read.

`json_encode` accepts only values with an exact JSON representation. It sorts object keys, adds no insignificant whitespace, and refuses output beyond 16 MiB. A secret handle or project identity is not an ordinary encodable value. The functions have a compiled source-facing signature, while their controlled implementation requires the [accepted project host](../operational/lifecycle.md). Ordinary source cannot use their operational route to bypass authority. See [limits](../limits.md).
