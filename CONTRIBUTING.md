# Contributing to OPAAL

OPAAL is currently an unreleased language foundation. Contributions should
keep its language identity, pure execution boundary, diagnostics, tooling, and
documentation coherent.

## Development workflow

Create a focused branch from `main`, make one reviewable change, and run the
applicable checks from [the development guide](docs/development.md). Pull
requests must pass the `required` and `security-required` checks. Keep generated
build, fuzz, and benchmark-result files out of commits.

Changes to syntax, semantics, public Rust APIs, diagnostics, source extensions,
environment protocols, limits, or serialized formats need focused tests and
matching public documentation. Preserve predecessor material under `history/`
without presenting it as a current OPAAL contract.

## Developer Certificate of Origin

Every commit must include a `Signed-off-by` trailer certifying the
[Developer Certificate of Origin 1.1](https://developercertificate.org/). Add
the trailer with:

```sh
git commit -s
```

Do not include credentials, private paths, machine-specific state, or unrelated
logs in an issue, commit, or pull request.

## Publication boundary

This repository does not currently publish crates, tags, binaries, packages,
or releases. A contribution must not add publishing authority or claim support
for another operating system, image, target, or physical device without a
separately reviewed release or integration contract.

Security reports follow [the security policy](SECURITY.md), not a public issue.
