#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: fuzz/run-campaign.sh [seconds [result-directory]]" >&2
  exit 2
}

seconds=${1:-600}
if [ "$#" -gt 2 ] || ! [[ "$seconds" =~ ^[1-9][0-9]*$ ]]; then
  usage
fi

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd "$script_dir/.." && pwd -P)

if [ "$#" -eq 2 ]; then
  campaign=$2
  if [ -z "$campaign" ] || [ -e "$campaign" ]; then
    echo "result directory must be nonempty and must not already exist: $campaign" >&2
    exit 2
  fi
  mkdir -p "$(dirname -- "$campaign")"
  mkdir "$campaign"
else
  mkdir -p "$repository_root/fuzz/campaigns"
  timestamp=$(date -u +%Y%m%dT%H%M%SZ)
  campaign=$(mktemp -d "$repository_root/fuzz/campaigns/${timestamp}.XXXXXX")
fi

nightly_cargo=$(rustup which --toolchain nightly cargo)
nightly_directory=$(dirname -- "$nightly_cargo")
export PATH="$nightly_directory:${PATH:-}"

mkdir "$campaign/corpus" "$campaign/artifacts"
echo "campaign directory: $campaign"

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
  corpus="$campaign/corpus/$target"
  artifacts="$campaign/artifacts/$target"
  mkdir -p "$corpus" "$artifacts"
  cargo fuzz run \
    --fuzz-dir "$repository_root/fuzz" \
    "$target" \
    "$corpus" \
    "${corpus_roots[@]}" \
    -- \
    "-max_total_time=$seconds" \
    -max_len=4096 \
    -timeout=10 \
    -rss_limit_mb=2048 \
    "-artifact_prefix=$artifacts/"
done
