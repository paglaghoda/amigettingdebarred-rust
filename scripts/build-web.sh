#!/bin/sh
# Resolve relative to this script, not the caller's working directory.
set -eu
exec python3 "$(dirname "$0")/build-web.py" "$@"
