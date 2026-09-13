# `std::version`

The compiled exports are the nominal `Version` type and `parse(input: String) -> Version`, `render(input: Version) -> String`, and `matches(range: String, version: Version) -> Bool`.

Parsing accepts canonical SemVer rather than treating a version as an arbitrary string. Rendering returns its canonical spelling. `matches` evaluates an explicit range against that parsed value. Project manifests use a `required_opaal` range; tool declarations use ranges that the lock's exact canonical version must satisfy. An invalid or mismatched range is a project-control error, not a reason to pick another installed tool.

Version parsing is distinct from a release promise. The [versioning reference](../versioning-and-compatibility.md) explains source, artifact, and Rust API compatibility separately.
