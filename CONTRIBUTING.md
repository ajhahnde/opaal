# Feedback and contribution policy

OPAAL is maintained as a solo project for now. Reports of reproducible bugs,
incorrect documentation, and focused suggestions are welcome. For a
non-security problem, open an issue with the affected version or revision,
steps to reproduce it, the observed result, and the result you expected.

External patches and pull requests are not currently part of the contribution
route. Please report the problem or suggestion instead of preparing an
unsolicited implementation; external pull requests will normally be declined.
The maintainer makes implementation and release decisions.

Security reports follow [the security policy](SECURITY.md) through its private
reporting route. Do not put credentials, private paths, machine-specific
state, or unrelated logs in a public issue.

## Maintainer workflow

Repository changes use a focused branch from `main` and the applicable checks
in the [development guide](docs/development.md). Pull requests for maintainer
branches must pass the `required` and `security-required` checks. Keep
generated build, fuzz, and benchmark-result files out of commits.

Changes to syntax, semantics, Rust APIs, diagnostics, source extensions,
environment protocols, limits, or serialized formats need focused tests and
matching public documentation.

Every commit includes a `Signed-off-by` trailer certifying the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).
Use `git commit -s` to add it. Do not include credentials or machine-specific
state in a commit or pull request.

The repository does not currently publish crates, tags, binaries, packages,
or releases. A change cannot add publishing authority or claim support for
another operating system, image, target, or physical device without a
separately reviewed release or integration contract.
