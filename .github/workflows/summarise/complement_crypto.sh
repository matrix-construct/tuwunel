#!/bin/bash
set -eo pipefail

track_name="Complement Cryptography"
results="tests/complement-crypto/results.jsonl"

# shellcheck source=./complement.sh
. "$(dirname "$0")/complement.sh"

summarise_main "$@"
