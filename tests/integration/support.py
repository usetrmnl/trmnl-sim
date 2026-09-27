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

import setup_cache  # noqa: E402

BUILD = Path(os.environ.get("TRMNL_FIRMWARE_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/trmnl"))
BWRY_BUILD = Path(os.environ.get("TRMNL_BWRY_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/trmnl_4clr"))
E1002_BUILD = Path(os.environ.get("TRMNL_E1002_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/seeed_reTerminal_E1002"))
# PlatformIO build directories of the firmware checkout, one per environment
# (TRMNL_FIRMWARE_BUILDS=<checkout>/.pio/build for another checkout).
BUILDS = Path(os.environ.get("TRMNL_FIRMWARE_BUILDS", ROOT.parent / "trmnl-firmware/.pio/build"))


def build_of(env: str) -> Path:
    """The build of PlatformIO environment `env` (it may not exist: see `require_build`)."""
    return BUILDS / env


def require_build(build: Path):
    """Skip the calling module (from setUpModule) when `build` hasn't been built."""
    import unittest

    if not (build / "firmware.elf").exists():
        raise unittest.SkipTest(f"no {build.name} build at {build} (pio run -e {build.name})")
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


_built_fixtures: list = []


def fixture(make):
    """A module fixture built on first use, so a worker running one class of a module
    (run.py splits modules that set PARALLEL_BY_CLASS) only builds what that class needs.
    Call the returned function to get it; close_fixtures() (in tearDownModule) closes the
    ones built."""
    value = None

    def get():
        nonlocal value
        if value is None:
            value = make()
            _built_fixtures.append(value)
        return value

    return get


def close_fixtures():
    """Close the fixtures built so far, newest first."""
    while _built_fixtures:
        _built_fixtures.pop().close()


class ProvisionedDevice:
    """A mock API server plus a device that already completed onboarding against a server
    like it: `boot()` powers on a copy of its flash, `boot_asleep()` resumes it in the deep
    sleep that followed onboarding (a save point), skipping the first refresh cycle.

    The onboarded device comes from the setup cache (see setup_cache). It knows the server
    by the port of the mock it was onboarded with, so every simulator started here gets
    `--host-port` to send that port to this fixture's mock."""

    NAME = "provisioned"
    SIM_ARGS: tuple[str, ...] = ("--offline",)
    DEVICE_HOST = "10.0.2.2"

    def __init__(self, build: Path = BUILD):
        self.build = build
        inputs = {"build": setup_cache.build_id(build), "memcheck": MEMCHECK, "turbo": TURBO, "args": self.SIM_ARGS, "host": self.DEVICE_HOST}
        self.cache, meta = setup_cache.entry(f"{self.NAME}-{build.name}", inputs, self._onboard)
        self.mock = MockTrmnl()
        self.mock.device_host = self.DEVICE_HOST
        self.mock.display = {"image": "default", "refresh_rate": 300}
        self.host_ports = {meta["port"]: self.mock.port}
        self.dir = Path(tempfile.mkdtemp(prefix="trmnl-provisioned-"))

    def _onboard(self, out: Path) -> dict:
        with MockTrmnl() as mock:
            mock.device_host = self.DEVICE_HOST
            mock.display = {"image": "default", "refresh_rate": 300}
            with sim(self.build, flash=out / "flash.bin", erase=True, extra_args=self.SIM_ARGS, name=f"{self.NAME}-setup") as s:
                s.wait(portal=True, timeout_s=90)
                s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
                mock.wait_for_request("/api/display", timeout_s=120)
                # a slow panel (Spectra 6) can still be refreshing when the chip sleeps
                s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
                s.save_point(out / "asleep.trmnlsave", label="onboarded, asleep")
            return {"port": mock.port}

    def _sim(self, **kw) -> Simulator:
        extra = tuple(kw.pop("extra_args", ())) + self.SIM_ARGS
        return sim(self.build, extra_args=extra, host_ports=self.host_ports, **kw)

    def boot(self, **kw) -> Simulator:
        """Power on a copy of the provisioned device."""
        flash = self.dir / f"flash-{len(list(self.dir.iterdir()))}.bin"
        shutil.copy(self.cache / "flash.bin", flash)
        return self._sim(flash=flash, **kw)

    def boot_asleep(self, **kw) -> Simulator:
        """The provisioned device in deep sleep right after onboarding (showing the mock's
        default image); wake it, press, touch... to carry on."""
        return self.restore(self.cache / "asleep.trmnlsave", **kw)

    def restore(self, path, **kw) -> Simulator:
        """A simulator resumed from a save point of this device (reaching this mock)."""
        return self._sim(restore=path, **kw)

    def close(self):
        self.mock.close()
        shutil.rmtree(self.dir, ignore_errors=True)


__all__ = ["BUILD", "BUILDS", "build_of", "require_build", "BWRY_BUILD", "E1002_BUILD", "GOLDEN", "TEST_MAC", "NETWORK", "MEMCHECK", "KNOWN_MEMORY_BUGS", "sim", "fixture", "close_fixtures", "ProvisionedDevice", "MockTrmnl", "big_number", "Simulator"]
