#!/bin/sh
set -eu
test "$#" -eq 3 || exit 2
command -v python3 >/dev/null 2>&1 || exit 2
exec python3 ./comparison.py "$@"
