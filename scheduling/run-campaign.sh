#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cases=${1:-64}

case "$cases" in
    ''|*[!0-9]*|0)
        echo "case count must be a positive integer" >&2
        exit 2
        ;;
esac
if (( cases > 4096 )); then
    echo "case count must not exceed 4096" >&2
    exit 2
fi

case "$#" in
    0|1)
        mkdir -p "$root/scheduling/campaigns"
        campaign=$(mktemp -d "$root/scheduling/campaigns/$(date -u +%Y%m%dT%H%M%SZ).XXXXXX")
        ;;
    2|3)
        campaign=$2
        if [[ -z "$campaign" ]]; then
            echo "result directory must not be empty" >&2
            exit 2
        fi
        if [[ -e "$campaign" ]]; then
            echo "result directory must not already exist: $campaign" >&2
            exit 2
        fi
        mkdir -p "$(dirname -- "$campaign")"
        mkdir "$campaign"
        ;;
    *)
        echo "usage: $0 [cases [result-directory [campaign-seed]]]" >&2
        exit 2
        ;;
esac

if (( $# == 3 )); then
    seed=$3
else
    seed=0
    while [[ "$seed" == 0 ]]; do
        seed=$(od -An -N8 -tu8 /dev/urandom | tr -d '[:space:]')
    done
fi

manifest="$campaign/manifest.txt"
output="$campaign/output.log"
replay="FLASH_PTY_STRESS_CAMPAIGN_SEED=$seed FLASH_PTY_STRESS_CASES=$cases cargo test -p flash-cli --test pty stress_ -- --nocapture --test-threads=1"
{
    echo "Flash scheduling stress campaign"
    echo "started_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "campaign_seed=$seed"
    echo "cases_per_scenario=$cases"
    echo "scenarios=4"
    echo "host=$(uname -a)"
    echo "rustc=$(rustc --version)"
    echo "cargo=$(cargo --version)"
    echo "workspace=$root"
    echo "replay=$replay"
} > "$manifest"

echo "campaign directory: $campaign"
echo "campaign seed: $seed"
echo "cases per scenario: $cases"

cd "$root"
if FLASH_PTY_STRESS_CAMPAIGN_SEED="$seed" \
    FLASH_PTY_STRESS_CASES="$cases" \
    cargo test -p flash-cli --test pty stress_ -- --nocapture --test-threads=1 \
    2>&1 | tee "$output"; then
    status=passed
    result=0
else
    result=$?
    status=failed
fi

{
    echo "finished_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "result=$status"
} >> "$manifest"

echo "campaign result: $status"
echo "manifest: $manifest"
echo "complete output: $output"
exit "$result"
