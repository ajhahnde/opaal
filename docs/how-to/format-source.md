# Format source

Check an explicit list of regular `.opaal` files before changing them:

```sh
target/release/opaal format --check examples/language-foundation.opaal
```

A successful check is silent. To rewrite those files atomically, use `--write` with the same explicit paths:

```sh
target/release/opaal format --write examples/language-foundation.opaal
```

The formatter does not recurse through directories, expand globs, follow imports, accept stdin as `-`, or accept a final symlink. It operates on syntax; formatting does not run code or turn an invalid program into a valid one. Run [source check](check-source.md) after a rewrite if you need semantic validation.
