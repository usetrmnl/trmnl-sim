"""On-disk cache of the devices the tests start from (a factory-fresh X, onboarded devices),
so a local run doesn't redo the factory flow and onboarding every time.

An entry is keyed by everything that went into it: the firmware build's files, the simulator
binary, the test support code, the fixture's own parameters and the memcheck/turbo settings.
Change any of them and the key changes, so the entry is rebuilt on the next run; nothing
has to be invalidated by hand. Entries live in target/spec-cache/ (`cargo clean` removes
them), and only the newest few per fixture are kept.

`bin/spec --no-cache` (TRMNL_SPEC_NO_CACHE=1) builds everything from scratch, as CI does, and
so does a coverage run (TRMNL_SIM_COVERAGE), whose report should include the setup flows.
Parallel workers that need the same missing entry build it once: the others wait for it.
"""

import atexit
import fcntl
import hashlib
import json
import os
import shutil
import tempfile
from pathlib import Path
from typing import Callable

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
CACHE_DIR = ROOT / "target" / "spec-cache"
ENABLED = not os.environ.get("TRMNL_SPEC_NO_CACHE") and not os.environ.get("TRMNL_SIM_COVERAGE")
# Bump when what an entry holds changes shape.
FORMAT = 1
# Entries kept per fixture name (e.g. while switching between firmware branches).
KEEP = 3
# What the simulator loads from a PlatformIO build dir (see src/firmware.rs).
BUILD_FILES = (
    "firmware.elf", "bootloader.elf", "firmware.bin", "bootloader.bin", "partitions.bin",
    "merged_firmware.bin", "littlefs.bin", "spiffs.bin",
)
# The code that drives the setup flows.
SUPPORT_FILES = (
    HERE / "support.py", HERE / "support_x.py", HERE / "setup_cache.py",
    ROOT / "python" / "trmnl_sim.py", ROOT / "python" / "trmnl_mock.py",
)

_hashes: dict[tuple, str] = {}


def file_hash(path: Path) -> str:
    st = path.stat()
    memo = (str(path), st.st_mtime_ns, st.st_size)
    if memo not in _hashes:
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        _hashes[memo] = h.hexdigest()
    return _hashes[memo]


def build_id(build: Path) -> dict[str, str]:
    return {name: file_hash(build / name) for name in BUILD_FILES if (build / name).exists()}


def sim_id() -> str:
    return file_hash(Path(os.environ.get("TRMNL_SIM_BIN") or ROOT / "target" / "release" / "trmnl-sim"))


def entry(name: str, inputs: dict, make: Callable[[Path], dict]) -> tuple[Path, dict]:
    """The directory `make(dir)` filled for these inputs, and the dict it returned (JSON).

    Built now unless cached. The directory is shared with other runs and workers: copy
    files out of it before changing them."""
    if not ENABLED:
        d = Path(tempfile.mkdtemp(prefix=f"trmnl-{name}-"))
        atexit.register(shutil.rmtree, d, ignore_errors=True)
        return d, make(d)
    inputs = {"name": name, "format": FORMAT, "sim": sim_id(), "support": [file_hash(p) for p in SUPPORT_FILES], **inputs}
    key = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()[:20]
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    d = CACHE_DIR / f"{name}-{key}"
    with open(CACHE_DIR / f"{name}-{key}.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if (d / "meta.json").exists():
            os.utime(d)  # recently used: kept by _prune
            return d, json.loads((d / "meta.json").read_text())
        tmp = Path(tempfile.mkdtemp(prefix=f".{name}-", dir=CACHE_DIR))
        try:
            meta = make(tmp)
            (tmp / "meta.json").write_text(json.dumps(meta))
            tmp.rename(d)
        except BaseException:
            shutil.rmtree(tmp, ignore_errors=True)
            raise
    _prune(name)
    return d, meta


def _prune(name: str) -> None:
    """Keep the KEEP most recently used entries of `name`."""
    entries = [p for p in CACHE_DIR.glob(f"{name}-*") if p.is_dir() and p.name.count("-") == name.count("-") + 1]
    for old in sorted(entries, key=lambda p: p.stat().st_mtime, reverse=True)[KEEP:]:
        shutil.rmtree(old, ignore_errors=True)
        old.with_name(old.name + ".lock").unlink(missing_ok=True)
