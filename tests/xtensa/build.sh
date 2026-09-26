#!/bin/sh
# Rebuild the Xtensa core test programs (elf/*.elf are checked in so CI does not need
# the toolchain). Uses the ESP-IDF 5.x xtensa-esp-elf GCC with the ESP32-S3 dynconfig.
set -e
cd "$(dirname "$0")"
TC=${XTENSA_TC:-$HOME/.platformio/packages/toolchain-xtensa-esp-elf}
CC="$TC/bin/xtensa-esp-elf-gcc -mdynconfig=$TC/lib/xtensa_esp32s3.so"
LIBGCC=$TC/lib/gcc/xtensa-esp-elf/14.2.0/esp32s3/libgcc.a
CFLAGS="-Wl,--no-warn-rwx-segments -O2 -g -ffreestanding -fno-builtin -fno-common -nostdlib -Irt -Wall -Wno-unused-function"
mkdir -p elf
for src in src/*.c src/*.S; do
    [ -e "$src" ] || continue
    name=$(basename "${src%.*}")
    extra=""
    # asm tests may come with a C part (src/<name>_c.c)
    case "$name" in *_c) continue ;; esac
    [ -e "src/${name}_c.c" ] && extra="src/${name}_c.c"
    $CC $CFLAGS -T rt/link.ld -o "elf/$name.elf" rt/crt0.S rt/rt.c "$src" $extra "$LIBGCC"
    echo "built elf/$name.elf"
done
