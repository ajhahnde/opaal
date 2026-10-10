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
cargo build --manifest-path "$repository_root/Cargo.toml" -p opaal-platform-posix --bin opaal-standard-host-fixture --locked
export PATH="$nightly_directory:${PATH:-}"

work=$(mktemp -d "${TMPDIR:-/tmp}/opaal-fuzz.XXXXXX")
cleanup() {
  rm -rf "$work"
}
trap cleanup EXIT

mkdir "$work/source-formatting"
python3 - "$repository_root/tests/golden/source-formatting" "$work/source-formatting" <<'PY'
import shutil
import sys
from pathlib import Path

source, destination = map(Path, sys.argv[1:])
for path in sorted(source.rglob('*.opaal')):
    target = destination / path.relative_to(source)
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(path, target)
PY

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

for target in lexer parser expander resources data_operations_limits numeric_operations_limits random_limits stdio_limits secret_sinks; do
  corpus="$work/$target"
  mkdir "$corpus"
  seeds=("${corpus_roots[@]}")
  if [ "$target" = parser ]; then
    seeds+=("$work/source-formatting")
  fi
  if [ "$target" = data_operations_limits ] || [ "$target" = numeric_operations_limits ] || [ "$target" = random_limits ] || [ "$target" = stdio_limits ]; then
    seeds=("$script_dir/seeds/$target")
  fi
  python3 - "$target" "${seeds[@]}" <<'PY'
import hashlib
import sys
from pathlib import Path

for operand in sys.argv[2:]:
    path = Path(operand)
    files = sorted(path.rglob('*')) if path.is_dir() else [path]
    for seed in files:
        if seed.is_file():
            print(f"seed {sys.argv[1]} {hashlib.sha256(seed.read_bytes()).hexdigest()} {seed}", flush=True)
PY
  cargo fuzz run \
    --fuzz-dir "$repository_root/fuzz" \
    "$target" \
    "$corpus" \
    "${seeds[@]}" \
    -- \
    "-runs=$runs" \
    -max_len=4096 \
    -timeout=10 \
    -rss_limit_mb=2048
done
