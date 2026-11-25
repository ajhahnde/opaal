#!/usr/bin/env python3
"""Package the exact committed source-formatting fixtures for archive replay."""
from __future__ import annotations

import subprocess

if __package__:
    from .package_data_processing import ROOT, package_examples
else:
    from package_data_processing import ROOT, package_examples

FIXTURES = "tests/golden/source-formatting"
MEMBER_IMPORT_FIXTURES = (
    "before.opaal", "after.opaal", "alternate.opaal", "model.opaal", "facade.opaal",
    "local-main.opaal", "io-allowed.opaal", "io-pure.opaal", "missing.opaal", "private.opaal",
    "duplicate.opaal", "incomplete.opaal", "noncanonical.opaal", "canonical.opaal",
    "pipeline.opaal", "project-tasks.opaal", "value.opaal", "policy-tests.opaal", "policy-negative.opaal",
)
COMPOUND_ASSIGNMENT_FIXTURES = (
    "before.opaal", "after.opaal", "alternate.opaal", "failure.opaal",
    "noncanonical.opaal", "canonical.opaal", "incomplete.opaal", "wrong-type.opaal",
    "immutable.opaal", "captured.opaal", "project-tasks.opaal", "policy-tests.opaal",
    "policy-negative.opaal", "io-allowed.opaal", "io-pure.opaal",
)

LIST_LITERAL_SPREAD_FIXTURES = (
    "before.opaal", "after.opaal", "alternate.opaal", "failure.opaal",
    "known-non-list.opaal", "invalid.opaal", "incomplete.opaal",
    "noncanonical.opaal", "canonical.opaal", "project-tasks.opaal",
    "policy-tests.opaal", "policy-negative.opaal", "io-allowed.opaal",
    "io-failure.opaal", "io-pure.opaal",
)

RECORD_SHORTHAND_FIXTURES = (
    "before.opaal", "after.opaal", "alternate.opaal", "model.opaal", "qualified.opaal",
    "patterns.opaal", "failure.opaal", "duplicate.opaal", "unknown.opaal", "missing.opaal",
    "wrong-type.opaal", "invalid.opaal", "incomplete.opaal", "noncanonical.opaal",
    "canonical.opaal", "project-tasks.opaal", "policy-tests.opaal", "policy-negative.opaal",
    "io-allowed.opaal", "io-failure.opaal", "io-pure.opaal",
)

GROUPED_EXPORT_FIXTURES = (
    "after.opaal",
    "before.opaal",
    "canonical.opaal",
    "duplicate.opaal",
    "explicit-reexports.opaal",
    "imports-only.opaal",
    "incomplete.opaal",
    "invalid-inventory.opaal",
    "invalid.opaal",
    "main.opaal",
    "model.opaal",
    "no-match.opaal",
    "noncanonical.opaal",
    "project-tasks.opaal",
    "purity.opaal",
)

ACTION_DECLARATION_FIXTURES = (
    "canonical.opaal", "noncanonical.opaal", "pure.opaal", "io.opaal",
    "old-form.opaal", "semicolon.opaal", "missing-effects.opaal", "missing-result.opaal",
    "duplicate-block.opaal", "misplaced.opaal", "incomplete.opaal", "requests.opaal",
)


INDEXED_REPLACEMENT_FIXTURES = (
    "README.md", "after.opaal", "alternate.opaal", "before.opaal", "canonical.opaal",
    "captured.opaal", "dynamic-failure.opaal", "failure.opaal", "immutable.opaal",
    "incomplete.opaal", "invalid.opaal", "io-allowed.opaal", "io-failure.opaal",
    "io-pure.opaal", "noncanonical.opaal", "output.opaal", "policy-negative.opaal",
    "policy-tests.opaal", "project-tasks.opaal", "wrong-index.opaal", "wrong-target.opaal",
    "wrong-type.opaal",
)

IF_EXPRESSION_FIXTURES = (
    "after-driver.opaal", "after.opaal", "before-driver.opaal", "before.opaal",
    "canonical.opaal", "incomplete.opaal", "invalid.opaal", "noncanonical.opaal",
    "project-tasks.opaal", "raw-input.opaal", "routes.opaal", "wrong-condition.opaal",
    "wrong-type.opaal",
)

TYPED_TUPLE_FIXTURES = (
    "after.opaal", "before.opaal", "command-commas.opaal", "summary.opaal",
    "wrong-pattern.opaal", "wrong-result.opaal",
)

INDEXED_ITERATION_FIXTURES = (
    "bad-element.opaal", "indexed-and-paired.opaal", "noncanonical.opaal",
)

LIST_COMPREHENSION_FIXTURES = (
    "bad-filter.opaal", "bad-input.opaal", "loop-report.opaal", "negative.opaal",
    "order.opaal", "policy.opaal", "report.opaal", "skipped-effect.opaal",
    "tasks.opaal", "typing-and-scope.opaal",
)

LIST_MAP_FIXTURES = (
    "explicit.opaal", "grouped.opaal", "incompatible-consumer.opaal", "inferred.opaal",
    "missing.opaal", "missing-result.opaal", "parameter-conflict.opaal", "policy.opaal",
    "result-conflict.opaal", "tasks.opaal", "typed-empty.opaal",
)

SLICE_FIXTURES = (
    "bad-operand.opaal", "bad-target.opaal", "before.opaal", "canonical.opaal",
    "compatibility.opaal", "effect.opaal", "incomplete.opaal", "invalid.opaal",
    "noncanonical.opaal", "past-end.opaal", "policy.opaal", "preview.opaal",
    "qualified-bounds.opaal", "rethrow.opaal", "tasks.opaal", "typed-empty.opaal",
)

INLINE_EXPORT_FIXTURES = (
    "alias-conflict.opaal", "check-policy.opaal", "duplicate-exact.opaal",
    "duplicate-prefix.opaal", "facade.opaal", "failed-init.opaal",
    "grouped-policy.opaal", "incomplete.opaal", "invalid.opaal", "main.opaal",
    "member-conflict.opaal", "noncanonical.opaal", "policy.opaal", "private.opaal",
    "project-tasks.opaal", "reverse-exact.opaal", "reverse-prefix.opaal",
    "test-effect.opaal", "test-signature.opaal",
)


def main() -> None:
    # Every shipped input and qualification assertion belongs to the named head.
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("fixture packaging requires a clean committed checkout")
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    paths = subprocess.check_output(["git", "ls-files", "-z", "--", FIXTURES], cwd=ROOT)
    names = [path[len(FIXTURES) + 1:] for path in paths.decode().split("\0") if path]
    members = {name.removeprefix("explicit-member-imports/") for name in names if name.startswith("explicit-member-imports/")}
    if members != set(MEMBER_IMPORT_FIXTURES):
        raise ValueError("member import fixture census differs from the committed source")
    compound = {name.removeprefix("compound-assignment/") for name in names if name.startswith("compound-assignment/")}
    if compound != set(COMPOUND_ASSIGNMENT_FIXTURES):
        raise ValueError("compound assignment fixture census differs from the committed source")
    spreads = {name.removeprefix("list-literal-spread/") for name in names if name.startswith("list-literal-spread/")}
    if spreads != set(LIST_LITERAL_SPREAD_FIXTURES):
        raise ValueError("list literal spread fixture census differs from the committed source")
    records = {name.removeprefix("record-shorthand/") for name in names if name.startswith("record-shorthand/")}
    if records != set(RECORD_SHORTHAND_FIXTURES):
        raise ValueError("record shorthand fixture census differs from the committed source")
    exports = {name.removeprefix("grouped-export-prefixes/") for name in names if name.startswith("grouped-export-prefixes/")}
    if exports != set(GROUPED_EXPORT_FIXTURES):
        raise ValueError("grouped export fixture census differs from the committed source")
    actions = {name.removeprefix("action-declarations/") for name in names if name.startswith("action-declarations/")}
    if actions != set(ACTION_DECLARATION_FIXTURES):
        raise ValueError("action declaration fixture census differs from the committed source")
    indexed = {name.removeprefix("indexed-list-replacement/") for name in names if name.startswith("indexed-list-replacement/")}
    if indexed != set(INDEXED_REPLACEMENT_FIXTURES):
        raise ValueError("indexed replacement fixture census differs from the committed source")
    conditionals = {name.removeprefix("if-expressions/") for name in names if name.startswith("if-expressions/")}
    if conditionals != set(IF_EXPRESSION_FIXTURES):
        raise ValueError("if expression fixture census differs from the committed source")
    tuples = {name.removeprefix("typed-tuples/") for name in names if name.startswith("typed-tuples/")}
    if tuples != set(TYPED_TUPLE_FIXTURES):
        raise ValueError("typed tuple fixture census differs from the committed source")
    loops = {name.removeprefix("indexed-iteration/") for name in names if name.startswith("indexed-iteration/")}
    if loops != set(INDEXED_ITERATION_FIXTURES):
        raise ValueError("indexed iteration fixture census differs from the committed source")
    inline = {name.removeprefix("inline-module-exports/") for name in names if name.startswith("inline-module-exports/")}
    if inline != set(INLINE_EXPORT_FIXTURES):
        raise ValueError("inline export fixture census differs from the committed source")
    comprehensions = {name.removeprefix("list-comprehensions/") for name in names
                      if name.startswith("list-comprehensions/")}
    if comprehensions != set(LIST_COMPREHENSION_FIXTURES):
        raise ValueError("list comprehension fixture census differs from the committed source")
    mappings = {name.removeprefix("list-map/") for name in names if name.startswith("list-map/")}
    if mappings != set(LIST_MAP_FIXTURES):
        raise ValueError("List map fixture census differs from the committed source")
    slices = {name.removeprefix("list-string-slicing/") for name in names
              if name.startswith("list-string-slicing/")}
    if slices != set(SLICE_FIXTURES):
        raise ValueError("List and String slice fixture census differs from the committed source")
    archive = package_examples(ROOT, names, source, directory=FIXTURES, name="source-formatting")
    print(archive.relative_to(ROOT))


if __name__ == "__main__":
    main()
