#!/bin/sh
set -eu
test "$#" -eq 3 || exit 2
opaal check --project opaal.toml --task saved --environment offline --input "expected_id=$1" --input "expected_platform=$2" --input "expected_revision=$3" >&2 || exit 2
accepted=$(opaal plan --project opaal.toml --task saved --environment offline --input "expected_id=$1" --input "expected_platform=$2" --input "expected_revision=$3" --expires-in 900s --out saved.plan.json --format digest) || exit 2
code=0
opaal execute --plan saved.plan.json --accept "$accepted" --journal saved.run.jsonl --receipt-out saved.receipt.json --require-true || code=$?
if ! opaal audit --project opaal.toml --journal saved.run.jsonl --out saved.audit.json >&2; then
    [ "$code" -eq 130 ] && exit 130
    exit 2
fi
exit "$code"
