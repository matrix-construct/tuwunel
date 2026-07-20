#!/bin/bash
set -eo pipefail

# Tuwunel's own Complement suite: the TestComplemau* tests under ./tests/complemau.
# Like the debug Complement board it runs the test profile without perf, so it
# renders no runtime-metrics table; it just needs its own heading and results path.
track_name="Complemau Application Services"
results="tests/complement/complemau/results.jsonl"

# shellcheck source=./complement.sh
. "$(dirname "$0")/complement.sh"

summarise_main "$@"
