#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 PLATFORM REPORT_DIRECTORY" >&2
    exit 2
fi

platform=$1
reports=$2
case "$platform" in
    linux-x86_64) host=linux; docker_report=docker-image-checks-linux ;;
    macos-arm64) host=macos; docker_report=docker-image-checks-macos ;;
    linux-arm64) host=linux-arm64; docker_report=docker-image-checks-offline-linux-arm64 ;;
    *) echo "unsupported candidate platform: $platform" >&2; exit 2 ;;
esac

version=$(python3 -c 'from ci.package_release import VERSION; print(VERSION)')
source=$(git rev-parse HEAD)
prefix="dist/opaal-v${version}"
candidate=(--archive "$prefix-$platform.tar.gz" --expected-source "$source" --platform "$platform")

python3 ci/package_data_processing.py --platform "$platform"
python3 ci/package_source_formatting.py
python3 ci/package_random_values.py
python3 ci/package_standard_input_output.py
python3 ci/package_docker_image_checks.py
python3 ci/package_controlled_native_tools.py

python3 ci/qualify_source_formatting.py "${candidate[@]}" \
    --fixtures "$prefix-source-formatting.tar.gz" --report "$reports/source-formatting-$host.json"
python3 ci/qualify_data_processing.py "${candidate[@]}" --expected-version "$version" \
    --examples "$prefix-data-processing.tar.gz" --report "$reports/data-processing-$host.json"
python3 ci/qualify_numeric_operations.py "${candidate[@]}" --report "$reports/numeric-operations-$host.json"
python3 ci/qualify_random_values.py "${candidate[@]}" \
    --fixtures "$prefix-random-values.tar.gz" --report "$reports/random-values-$host.json"
python3 ci/qualify_standard_input_output.py "${candidate[@]}" \
    --fixtures "$prefix-standard-input-output.tar.gz" --report "$reports/standard-input-output-$host.json"
python3 ci/qualify_docker_image_checks.py --offline "${candidate[@]}" \
    --fixtures "$prefix-docker-image-checks.tar.gz" --report "$reports/$docker_report.json"
