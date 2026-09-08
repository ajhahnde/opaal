#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: fuzz/run-smoke.sh [runs]" >&2
  exit 2
}

runs=${1:-1000}
if [ "$#" -gt 1 ] || ! [[ "$runs" =~ ^[0-9]+$ ]]; then
  usage
fi

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd "$script_dir/.." && pwd -P)
nightly_cargo=$(rustup which --toolchain nightly cargo)
nightly_directory=$(dirname -- "$nightly_cargo")
export PATH="$nightly_directory:${PATH:-}"

work=$(mktemp -d "${TMPDIR:-/tmp}/opaal-fuzz.XXXXXX")
cleanup() {
  rm -rf "$work"
}
trap cleanup EXIT

corpus_roots=(
  "$repository_root/tests/opaal-foundation/language/grammar/complete"
  "$repository_root/tests/opaal-foundation/language/grammar/incomplete"
  "$repository_root/tests/opaal-foundation/language/grammar/invalid"
  "$repository_root/tests/opaal-foundation/language/grammar/repl"
  "$repository_root/tests/opaal-foundation/language/lexical"
  "$repository_root/tests/opaal-foundation/language/modules/complete"
  "$repository_root/tests/opaal-foundation/language/modules/invalid"
  "$repository_root/tests/opaal-foundation/language/outcomes/complete"
  "$repository_root/tests/opaal-foundation/language/outcomes/invalid"
  "$repository_root/tests/opaal-foundation/language/outcomes/refused"
  "$repository_root/tests/opaal-foundation/language/operations/complete"
  "$repository_root/tests/opaal-foundation/language/operations/invalid"
  "$repository_root/tests/opaal-foundation/language/rest-spread/complete"
  "$repository_root/tests/opaal-foundation/language/rest-spread/invalid"
  "$repository_root/tests/opaal-foundation/language/types/complete"
  "$repository_root/tests/opaal-foundation/language/types/invalid"
)

for target in lexer parser expander resources secret_sinks; do
  corpus="$work/$target"
  mkdir "$corpus"
  cargo fuzz run \
    --fuzz-dir "$repository_root/fuzz" \
    "$target" \
    "$corpus" \
    "${corpus_roots[@]}" \
    -- \
    "-runs=$runs" \
    -max_len=4096 \
    -timeout=10 \
    -rss_limit_mb=2048
done
