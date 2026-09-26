"""Shared fixtures for the TRMNL X integration tests (ESP32-S3, 1872x1404 panel, C5 modem)."""

import os
import shutil
import tempfile
from pathlib import Path

import setup_cache
from support import KNOWN_MEMORY_BUGS, MEMCHECK, ROOT, TURBO, MockTrmnl, Simulator

X_BUILD = Path(os.environ.get("TRMNL_X_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/TRMNL_X"))
X_MAC = "D8:3B:DA:00:00:01"
SSID_24 = "TRMNL-Sim"
SSID_5 = "TRMNL-Sim-5G"


def x_sim(**kw) -> Simulator:
    from support import _current_test_id

    kw.setdefault("name", _current_test_id())
    kw.setdefault("mac", X_MAC)
    kw.setdefault("turbo", TURBO)
    kw.setdefault("memcheck", MEMCHECK)
    kw.setdefault("memcheck_suppress", KNOWN_MEMORY_BUGS)
    extra = tuple(kw.pop("extra_args", ()))
    if "--offline" not in extra:
        extra += ("--offline",)
    return Simulator(X_BUILD, extra_args=extra, **kw)


class ShippedX:
    """A TRMNL X straight out of the factory: QA passed, modem flashed, waiting in shipment
    mode (light sleep until it is docked). From the setup cache (see setup_cache)."""

    def __init__(self):
        inputs = {"build": setup_cache.build_id(X_BUILD), "memcheck": MEMCHECK, "turbo": TURBO}
        self.cache, meta = setup_cache.entry("x-shipped", inputs, self._factory)
        self.template = self.cache / "flash.bin"
        self.factory_console = meta["console"]
        self.dir = Path(tempfile.mkdtemp(prefix="trmnl-x-"))

    @staticmethod
    def _factory(out: Path) -> dict:
        with x_sim(flash=out / "flash.bin", erase=True, name="x-factory") as s:
            s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
            return {"console": s.console(0)}

    def copy(self, name: str) -> Path:
        flash = self.dir / f"{name}-{len(list(self.dir.iterdir()))}.bin"
        shutil.copy(self.template, flash)
        return flash

    def boot(self, **kw) -> Simulator:
        return x_sim(flash=self.copy("shipped"), **kw)

    def close(self):
        shutil.rmtree(self.dir, ignore_errors=True)


def onboard(s: Simulator, mock: MockTrmnl, ssid: str) -> None:
    """Take a shipped device off shipment mode and through the setup portal."""
    s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
    s.dock(True)
    s.wait(portal=True, timeout_s=180)
    s.dock(False)
    s.portal_connect(ssid, "password", server=mock.device_url)
    mock.wait_for_request("/api/display", timeout_s=240)
    s.wait(state="deep_sleep", timeout_s=240)


class ProvisionedX:
    """A mock server plus an X that completed onboarding (on `ssid`) against a server like
    it; see ProvisionedDevice, which this mirrors (`boot`, `boot_asleep`, `restore`)."""

    def __init__(self, shipped: ShippedX, ssid: str = SSID_5):
        self.shipped = shipped
        self.ssid = ssid
        inputs = {"shipped": shipped.cache.name, "ssid": ssid, "memcheck": MEMCHECK, "turbo": TURBO}
        self.cache, meta = setup_cache.entry(f"x-provisioned-{ssid}", inputs, self._onboard)
        self.mock = MockTrmnl()
        self.mock.display = {"image": "default", "refresh_rate": 300}
        self.host_ports = {meta["port"]: self.mock.port}

    def _onboard(self, out: Path) -> dict:
        shutil.copy(self.shipped.template, out / "flash.bin")
        with MockTrmnl() as mock:
            mock.display = {"image": "default", "refresh_rate": 300}
            with x_sim(flash=out / "flash.bin", name="x-provision") as s:
                onboard(s, mock, self.ssid)
                s.save_point(out / "asleep.trmnlsave", label="onboarded, asleep")
            return {"port": mock.port}

    def boot(self, **kw) -> Simulator:
        flash = self.shipped.copy("provisioned")
        shutil.copy(self.cache / "flash.bin", flash)
        return x_sim(flash=flash, host_ports=self.host_ports, **kw)

    def boot_asleep(self, **kw) -> Simulator:
        return self.restore(self.cache / "asleep.trmnlsave", **kw)

    def restore(self, path, **kw) -> Simulator:
        return x_sim(restore=path, host_ports=self.host_ports, **kw)

    def close(self):
        self.mock.close()


__all__ = ["X_BUILD", "X_MAC", "SSID_24", "SSID_5", "x_sim", "ShippedX", "ProvisionedX", "onboard"]
