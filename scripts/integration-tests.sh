#!/usr/bin/env bash
# Build the simulator (and optionally the firmware), then run the integration tests.
#
#   scripts/integration-tests.sh                       # uses ../trmnl-firmware/.pio/build/{trmnl,trmnl_4clr,TRMNL_X}
#   scripts/integration-tests.sh --build-firmware      # runs `pio run -e trmnl -e trmnl_4clr -e TRMNL_X` first
#   scripts/integration-tests.sh test_refresh_cycle    # any unittest selector(s)
#
# Env: TRMNL_FIRMWARE (firmware checkout, default ../trmnl-firmware), TRMNL_FIRMWARE_BUILD,
#      TRMNL_X_BUILD, TRMNL_BWRY_BUILD, TRMNL_SIM_REALTIME=1,
#      TRMNL_SIM_NETWORK=1, TRMNL_SIM_UPDATE_GOLDEN=1, TRMNL_SIM_ARTIFACTS=<dir>,
#      TRMNL_SIM_COVERAGE=<dir> (firmware code coverage; merged report printed at the end).
set -euo pipefail
cd "$(dirname "$0")/.."

FIRMWARE="${TRMNL_FIRMWARE:-../trmnl-firmware}"
if [[ "${1:-}" == "--build-firmware" ]]; then
  shift
  pio run -d "$FIRMWARE" -e trmnl -e trmnl_4clr -e TRMNL_X
fi
export TRMNL_FIRMWARE_BUILD="${TRMNL_FIRMWARE_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/trmnl}"
# The TRMNL X and BWRY tests skip themselves when their build is missing.
export TRMNL_X_BUILD="${TRMNL_X_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/TRMNL_X}"
export TRMNL_BWRY_BUILD="${TRMNL_BWRY_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/trmnl_4clr}"

cargo build --release
BIN="$(pwd)/target/release/trmnl-sim"
export TRMNL_SIM_BIN="$BIN"

cd tests/integration
exec python3 run.py -v "$@"
