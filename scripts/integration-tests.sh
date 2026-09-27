#!/usr/bin/env bash
# Build the simulator (and optionally the firmware), then run the integration tests.
#
#   scripts/integration-tests.sh                       # uses ../trmnl-firmware/.pio/build/{trmnl,trmnl_4clr,TRMNL_X,seeed_reTerminal_E1002}
#   scripts/integration-tests.sh --build-firmware      # also runs `pio run` for those envs first
#   scripts/integration-tests.sh test_refresh_cycle    # any unittest selector(s)
#   scripts/integration-tests.sh -j 4                  # parallel workers (default: CPU count)
#
# Env: TRMNL_FIRMWARE (firmware checkout, default ../trmnl-firmware), TRMNL_FIRMWARE_BUILD,
#      TRMNL_X_BUILD, TRMNL_BWRY_BUILD, TRMNL_E1002_BUILD, TRMNL_SIM_REALTIME=1,
#      TRMNL_SIM_NETWORK=1, TRMNL_SIM_UPDATE_GOLDEN=1, TRMNL_SIM_ARTIFACTS=<dir>,
#      TRMNL_SIM_COVERAGE=<dir> (firmware code coverage; merged report printed at the end).
set -euo pipefail
cd "$(dirname "$0")/.."

FIRMWARE="${TRMNL_FIRMWARE:-../trmnl-firmware}"
if [[ "${1:-}" == "--build-firmware" ]]; then
  shift
  pio run -d "$FIRMWARE" -e trmnl -e trmnl_4clr -e TRMNL_X -e seeed_reTerminal_E1002
fi
export TRMNL_FIRMWARE_BUILD="${TRMNL_FIRMWARE_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/trmnl}"
# The TRMNL X, BWRY and reTerminal E1002 tests skip themselves when their build is missing.
export TRMNL_X_BUILD="${TRMNL_X_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/TRMNL_X}"
export TRMNL_BWRY_BUILD="${TRMNL_BWRY_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/trmnl_4clr}"
export TRMNL_E1002_BUILD="${TRMNL_E1002_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/seeed_reTerminal_E1002}"

cargo build --release
BIN="$(pwd)/target/release/trmnl-sim"
export TRMNL_SIM_BIN="$BIN"

cd tests/integration
exec python3 run.py -v "$@"
