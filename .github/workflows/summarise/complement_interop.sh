#!/bin/bash
set -eo pipefail

track_name="Complement Interoperability"
results="tests/complement/interop/results.jsonl"
#metrics_tar="tests/complement/interop/runtime_metrics.tar.zst"

# Diff against the homogeneous Complement board so known tuwunel failures render
# orange (a shared gap) rather than red (an interop regression), and render the
# grid over that board's full test list so the two line up square-for-square.
# logs_jsonl drives the peer-homeserver detection that blacks out non-federation
# tests.
baseline="tests/complement/results.jsonl"
logs_jsonl="tests/complement/interop/logs.jsonl"
render_over_baseline=1

# Mark progressed cells with a green crossed box to set the interop board apart
# from the homogeneous boards' green check.
adv_cell="❎"

# Known peer-side false positives, scored orange (accounted-for) not red
# (regression). These media tests exercise the legacy unauthenticated
# /_matrix/media/v3 path over federation; the Synapse peer removed it per MSC3916
# and 404s its own endpoint, while tuwunel serves the authenticated v1 path
# correctly. The failure is the peer's deprecation, not a tuwunel regression.
interop_fp_regress="TestMediaFilenames|TestRemotePngThumbnail"

# shellcheck source=./complement.sh
. "$(dirname "$0")/complement.sh"

summarise_main "$@"
