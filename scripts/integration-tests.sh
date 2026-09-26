#!/usr/bin/env bash
# Build the simulator (and optionally the firmware), then run the integration tests.
#
#   scripts/integration-tests.sh                       # uses ../trmnl-firmware/.pio/build/trmnl
#   scripts/integration-tests.sh --build-firmware      # runs `pio run -e trmnl` first
#   scripts/integration-tests.sh test_refresh_cycle    # any unittest selector(s)
#
# Env: TRMNL_FIRMWARE (firmware checkout, default ../trmnl-firmware), TRMNL_SIM_REALTIME=1,
#      TRMNL_SIM_NETWORK=1, TRMNL_SIM_UPDATE_GOLDEN=1, TRMNL_SIM_ARTIFACTS=<dir>.
set -euo pipefail
cd "$(dirname "$0")/.."

FIRMWARE="${TRMNL_FIRMWARE:-../trmnl-firmware}"
if [[ "${1:-}" == "--build-firmware" ]]; then
  shift
  pio run -d "$FIRMWARE" -e trmnl
fi
export TRMNL_FIRMWARE_BUILD="${TRMNL_FIRMWARE_BUILD:-$(cd "$FIRMWARE" && pwd)/.pio/build/trmnl}"

cargo build --release
BIN="$(pwd)/target/release/trmnl-sim"
export TRMNL_SIM_BIN="$BIN"

cd tests/integration
if [[ $# -gt 0 ]]; then
  python3 -m unittest -v "$@"
else
  python3 -m unittest -v
fi
