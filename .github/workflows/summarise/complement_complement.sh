#!/bin/bash
set -eo pipefail

# The optimized (bench) and debug (test) runs share this driver and the same
# baseline; the cargo profile only selects the track heading shown in the
# summary. Both produce identical results against tests/complement/results.jsonl.
if test "${cargo_profile:-}" = "test"; then
	track_name="Complement (debug)"
else
	track_name="Complement"
	metrics_tar="tests/complement/runtime_metrics.tar.zst"
fi

results="tests/complement/results.jsonl"

# shellcheck source=./complement.sh
. "$(dirname "$0")/complement.sh"

summarise_main "$@"
