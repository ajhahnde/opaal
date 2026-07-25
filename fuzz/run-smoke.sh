#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
runs=${1:-1000}

case "$runs" in
    ''|*[!0-9]*)
        echo "run count must be a nonnegative integer" >&2
        exit 2
        ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/flashshell-fuzz.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM

for target in lexer parser; do
    corpus="$work/$target"
    mkdir "$corpus"
    cargo +nightly fuzz run \
        --fuzz-dir "$root/fuzz" \
        "$target" \
        "$corpus" \
        "$root/tests/golden/grammar/complete" \
        "$root/tests/golden/grammar/incomplete" \
        "$root/tests/golden/grammar/invalid" \
        "$root/tests/golden/lexical/complete" \
        "$root/tests/golden/lexical/incomplete" \
        "$root/tests/golden/lexical/invalid" \
        -- \
        -runs="$runs" \
        -max_len=4096
done
