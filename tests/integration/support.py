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
GOLDEN = HERE / "golden"
TEST_MAC = "7C:DF:A1:00:00:01"
NETWORK = os.environ.get("TRMNL_SIM_NETWORK") == "1"
# Turbo (network-aware fast-forward) unless TRMNL_SIM_REALTIME=1.
TURBO = os.environ.get("TRMNL_SIM_REALTIME") != "1"


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


def sim(**kw) -> Simulator:
    kw.setdefault("name", _current_test_id())
    kw.setdefault("mac", TEST_MAC)
    kw.setdefault("turbo", TURBO)
    return Simulator(BUILD, **kw)


class ProvisionedDevice:
    """A mock API server plus a flash image of a device that already completed
    onboarding against it. `boot()` starts a simulator from a copy of that flash."""

    def __init__(self):
        self.mock = MockTrmnl()
        self.dir = Path(tempfile.mkdtemp(prefix="trmnl-provisioned-"))
        self.template = self.dir / "template.bin"
        self.mock.display = {"image": "default", "refresh_rate": 300}
        with sim(flash=self.template, erase=True, extra_args=("--offline",)) as s:
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
        return sim(flash=flash, extra_args=extra, **kw)

    def close(self):
        self.mock.close()
        shutil.rmtree(self.dir, ignore_errors=True)


__all__ = ["BUILD", "GOLDEN", "TEST_MAC", "NETWORK", "sim", "ProvisionedDevice", "MockTrmnl", "big_number", "Simulator"]
