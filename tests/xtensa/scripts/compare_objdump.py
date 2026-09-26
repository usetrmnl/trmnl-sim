#!/usr/bin/env python3
"""Compare the emulator's disassembly with GNU objdump (ESP32-S3 config).

Run `cargo test --no-default-features --bin trmnl-sim opcode_coverage` first: it writes
target/xtensa-coverage-{firmware,rom}.{bin,dis} (every distinct reachable encoding in an
8-byte slot, and our disassembly of it). This script disassembles the blob with objdump
and reports every slot where mnemonic or operands differ.
"""
import os, re, subprocess, sys

TC = os.path.expanduser("~/.platformio/packages/toolchain-xtensa-esp-elf")
ROOT = os.path.join(os.path.dirname(__file__), "..", "..", "..")

def objdump(binpath):
    env = dict(os.environ, XTENSA_GNU_CONFIG=f"{TC}/lib/xtensa_esp32s3.so")
    out = subprocess.run([f"{TC}/bin/xtensa-esp-elf-objdump", "-D", "-b", "binary", "-m", "xtensa", binpath],
                         env=env, check=True, capture_output=True, text=True).stdout
    r = {}
    for line in out.splitlines():
        m = re.match(r"^\s*([0-9a-f]+):\s+[0-9a-f]+\s+(.*)$", line)
        if m:
            r[int(m.group(1), 16)] = m.group(2).strip()
    return r

def norm(s):
    s = re.sub(r"\s*<[^>]*>", "", s)       # symbol annotations
    s = re.sub(r"\s*\([^)]*\)$", "", s)     # l32r literal value
    s = re.sub(r"\b0x([0-9a-f]+)\b(?=$)", lambda m: m.group(1), s) if s.startswith(("j", "b", "call", "loop", "l32r")) else s
    return re.sub(r"\s+", " ", s).strip()

def main():
    bad = 0
    for name in sys.argv[1:] or ["firmware", "rom", "bootloader"]:
        b = os.path.join(ROOT, "target", f"xtensa-coverage-{name}.bin")
        d = os.path.join(ROOT, "target", f"xtensa-coverage-{name}.dis")
        if not os.path.exists(b):
            print(f"{name}: no dump, skipping"); continue
        od = objdump(b)
        n = 0
        for line in open(d):
            pc, ours = line.rstrip("\n").split("\t", 1)
            pc = int(pc, 16); n += 1
            theirs = od.get(pc, "<missing>")
            # RSR/WSR/XSR of a special register this core does not have: objdump refuses
            # to decode it; we decode it and raise IllegalInstruction when executed.
            if re.match(r"^[rwx]sr\t", ours) and theirs.startswith(".byte"):
                continue
            if norm(ours) != norm(theirs):
                bad += 1
                if bad <= 60:
                    print(f"{name} {pc:x}: ours '{ours}'  objdump '{theirs}'")
        print(f"{name}: compared {n} distinct encodings")
    print(f"{bad} mismatches")
    sys.exit(1 if bad else 0)

main()
