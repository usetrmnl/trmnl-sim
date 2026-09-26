"""Shared fixtures for the integration tests."""

import os
import shutil
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(ROOT / "python"))

from trmnl_mock import MockTrmnl, big_number  # noqa: E402
from trmnl_sim import Simulator  # noqa: E402

BUILD = Path(os.environ.get("TRMNL_FIRMWARE_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/trmnl"))
BWRY_BUILD = Path(os.environ.get("TRMNL_BWRY_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/trmnl_4clr"))
GOLDEN = HERE / "golden"
TEST_MAC = "7C:DF:A1:00:00:01"
NETWORK = os.environ.get("TRMNL_SIM_NETWORK") == "1"
# Turbo (network-aware fast-forward) unless TRMNL_SIM_REALTIME=1.
TURBO = os.environ.get("TRMNL_SIM_REALTIME") != "1"
# TRMNL_SIM_MEMCHECK=1: every simulator runs with --memcheck=halt, and a test fails on
# any memory error (the simulator halts at it, or leaving the `with` block raises).
MEMCHECK = "halt" if os.environ.get("TRMNL_SIM_MEMCHECK") == "1" else None
# Firmware memory bugs memcheck found, tolerated so the rest of a run is still checked
# (test_memcheck.py has an expected failure for each, which turns into an unexpected
# success once the bug is fixed):
KNOWN_MEMORY_BUGS = (
    # Clock::sync passes a String's c_str() to configTime(), which keeps the pointer for
    # SNTP; the String is freed when setTimeFromNTP returns, and later SNTP retries resolve
    # the freed name (heap-use-after-free in dns_gethostbyname on the tiT task). Matched by
    # the free (while the block is still free) and by the reading side.
    "_ZN5Clock14setTimeFromNTPEv",
    "sntp_request",
    # display_show_image flips an uncompressed BMP as if it were panel-sized: on the X an
    # 800x480 BMP makes flip_image read and write ~280 KB past the 48 KB buffer.
    "_Z10flip_imagePhiib",
    # ...and sends it as the panel's plane: on the BWRY (2 bits/pixel) writePlane reads a
    # 1-bit 800x480 BMP's 48 KB buffer as 96 KB.
    "_Z18bbepWriteImage2bppP10bbepstructh",
    # HttpRetryRequest::bodyAsString uses String::concat(buf, len) on a body that isn't
    # NUL-terminated; concat copies len + 1 bytes, reading one byte past the malloc'd body
    # (and leaving that byte, not a NUL, after the text). Seen when the server sends a
    # Content-Length (the built-in mock server does).
    "_ZNK16HttpRetryRequest12bodyAsStringEv",
)


def _current_test_id() -> str:
    """Name artifacts after the running test (found by walking the call stack)."""
    import inspect
    import unittest

    for f in inspect.stack():
        obj = f.frame.f_locals.get("self") or f.frame.f_locals.get("cls")
        if isinstance(obj, unittest.TestCase):
            return obj.id()
        if isinstance(obj, type) and issubclass(obj, unittest.TestCase):
            return f"{obj.__module__}.{obj.__name__}"
    return "sim"


def sim(build: Path = BUILD, **kw) -> Simulator:
    kw.setdefault("name", _current_test_id())
    kw.setdefault("mac", TEST_MAC)
    kw.setdefault("turbo", TURBO)
    kw.setdefault("memcheck", MEMCHECK)
    kw.setdefault("memcheck_suppress", KNOWN_MEMORY_BUGS)
    return Simulator(build, **kw)


class ProvisionedDevice:
    """A mock API server plus a flash image of a device that already completed
    onboarding against it. `boot()` starts a simulator from a copy of that flash."""

    def __init__(self, build: Path = BUILD):
        self.build = build
        self.mock = MockTrmnl()
        self.dir = Path(tempfile.mkdtemp(prefix="trmnl-provisioned-"))
        self.template = self.dir / "template.bin"
        self.mock.display = {"image": "default", "refresh_rate": 300}
        with sim(build, flash=self.template, erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "password", server=self.mock.device_url)
            self.mock.wait_for_request("/api/display", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
        self.mock.requests.clear()

    def boot(self, **kw) -> Simulator:
        """Power on a copy of the provisioned device."""
        flash = self.dir / f"flash-{len(list(self.dir.iterdir()))}.bin"
        shutil.copy(self.template, flash)
        extra = tuple(kw.pop("extra_args", ())) + ("--offline",)
        return sim(self.build, flash=flash, extra_args=extra, **kw)

    def close(self):
        self.mock.close()
        shutil.rmtree(self.dir, ignore_errors=True)


__all__ = ["BUILD", "BWRY_BUILD", "GOLDEN", "TEST_MAC", "NETWORK", "MEMCHECK", "KNOWN_MEMORY_BUGS", "sim", "ProvisionedDevice", "MockTrmnl", "big_number", "Simulator"]
