#!/bin/sh
set -eu
IFS= read -r image_ref < expected-image-ref.txt
IFS= read -r expected_id < expected-image-id.txt
IFS= read -r expected_platform < expected-platform.txt
IFS= read -r expected_revision < expected-revision.txt
opaal check --project opaal.toml --task image --environment ci --input "image=$image_ref" --input "expected_id=$expected_id" --input "expected_platform=$expected_platform" --input "expected_revision=$expected_revision" >&2 || exit 2
accepted=$(opaal plan --project opaal.toml --task image --environment ci --input "image=$image_ref" --input "expected_id=$expected_id" --input "expected_platform=$expected_platform" --input "expected_revision=$expected_revision" --expires-in 900s --out image.plan.json --format digest) || exit 2
code=0
opaal execute --plan image.plan.json --accept "$accepted" --journal image.run.jsonl --receipt-out image.receipt.json --require-true || code=$?
if ! opaal audit --project opaal.toml --journal image.run.jsonl --out image.audit.json >&2; then
    [ "$code" -eq 130 ] && exit 130
    exit 2
fi
exit "$code"
