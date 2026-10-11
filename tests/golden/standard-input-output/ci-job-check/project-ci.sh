#!/bin/sh
set -euC
test ! -e acquired.json || exit 2
if ! python3 producer.py > acquired.json; then
    printf '%s\n' 'acquisition failed' >&2
    exit 2
fi
opaal task list --project opaal.toml > /dev/null || exit 2
opaal task inspect --project opaal.toml sample --format json > /dev/null || exit 2
opaal check --project opaal.toml --task sample --environment ci || exit 2
accepted=$(opaal plan --project opaal.toml --task sample --environment ci --expires-in 900s --out ci.plan.json --format digest) || exit 2
code=0
opaal execute --plan ci.plan.json --accept "$accepted" --journal ci.run.jsonl --receipt-out ci.receipt.json --require-true < acquired.json || code=$?
if ! opaal audit --project opaal.toml --journal ci.run.jsonl --out ci.audit.json > /dev/null; then
    [ "$code" -eq 130 ] && exit 130
    exit 2
fi
exit "$code"
