#!/bin/bash
set -eo pipefail

BASEDIR=$(dirname "$0")

export complement_tags="conduwuit_blacklist,complemau"
export complement_tests="./tests/complemau/..."
export complement_skip=""
export complement_results_dir="tests/complement/complemau"
export complement_baseline_gate=1

exec "$BASEDIR/complement.sh" "$@"
