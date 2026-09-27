#!/bin/sh
# Download Espressif's mask ROM ELFs (github.com/espressif/esp-rom-elfs) into rom/ of this
# checkout, where trmnl-sim looks first. PlatformIO's tool-esp-rom-elfs package (2024.10.11)
# lacks the ROM of production ESP32-C5 silicon (esp32c5_rev100_rom.elf); this release has it.
#
#   scripts/fetch-rom-elfs.sh              # into rom/
#   scripts/fetch-rom-elfs.sh ~/.cache/trmnl-sim/rom-elfs
set -eu
RELEASE=20260528
SHA256=caa463d3cbef2430a5a35847c1d9f2f152403b17a802050927ff60c8da54fe46
here=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$here/rom}
url=https://github.com/espressif/esp-rom-elfs/releases/download/$RELEASE/esp-rom-elfs-$RELEASE.tar.gz
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $url"
curl -fsSL -o "$tmp/r.tgz" "$url"
got=$( (sha256sum "$tmp/r.tgz" 2>/dev/null || shasum -a 256 "$tmp/r.tgz") | cut -d' ' -f1)
if [ "$got" != "$SHA256" ]; then
  echo "checksum mismatch: $got" >&2
  exit 1
fi
mkdir -p "$dest"
tar -xzf "$tmp/r.tgz" -C "$dest"
echo "ROM ELFs in $dest:"
ls "$dest"
