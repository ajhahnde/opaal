#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
seconds=${1:-600}

case "$seconds" in
    ''|*[!0-9]*|0)
        echo "duration must be a positive integer number of seconds" >&2
        exit 2
        ;;
esac

case "$#" in
    0|1)
        mkdir -p "$root/fuzz/campaigns"
        campaign=$(mktemp -d "$root/fuzz/campaigns/$(date -u +%Y%m%dT%H%M%SZ).XXXXXX")
        ;;
    2)
        campaign=$2
        if [ -z "$campaign" ]; then
            echo "result directory must not be empty" >&2
            exit 2
        fi
        if [ -e "$campaign" ]; then
            echo "result directory must not already exist: $campaign" >&2
            exit 2
        fi
        mkdir -p "$(dirname -- "$campaign")"
        mkdir "$campaign"
        ;;
    *)
        echo "usage: $0 [seconds [result-directory]]" >&2
        exit 2
        ;;
esac

nightly_cargo=$(rustup which --toolchain nightly cargo)
PATH=${nightly_cargo%/*}:$PATH
export PATH

mkdir "$campaign/corpus" "$campaign/artifacts"
echo "campaign directory: $campaign"

for target in lexer parser expander; do
    corpus="$campaign/corpus/$target"
    artifacts="$campaign/artifacts/$target"
    mkdir -p "$corpus" "$artifacts"
    cargo fuzz run \
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
        -max_total_time="$seconds" \
        -max_len=4096 \
        -timeout=10 \
        -rss_limit_mb=2048 \
        -artifact_prefix="$artifacts/"
done
