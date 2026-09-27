"""Shared fixtures for the TRMNL X integration tests (ESP32-S3, 1872x1404 panel, C5 modem),
and what makes the general tests (ENV = ANY) run on it (see `XSim` and `unboxed`)."""

import os
import shutil
import tempfile
import unittest
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


def ask_to_reset_wifi(s: Simulator, since: int) -> None:
    """Hold both edges of the touch bar, let go as the WiFi reset confirmation prompt
    appears, and wait until it is on the screen: the firmware draws the prompt (a few
    seconds on the X panel) before it reads the touch bar again, so an answer given during
    the refresh is missed."""
    s.pause(True)  # both fingers down at the same instant
    try:
        s.touch_down("left")
        s.touch_down("right")
    finally:
        s.pause(False)
    s.wait(console=r"Entering WiFi reset confirmation mode", since=since, timeout_s=15)
    s.touch_up("left")
    s.touch_up("right")
    # The display is still idle right after that line; wait for the prompt to be drawn.
    s.wait(console=r"display_show_msg end", since=since, timeout_s=15)
    s.wait(display_idle=True, settle_ms=100, timeout_s=15)


class XSim(Simulator):
    """A simulated TRMNL X for the general tests: the button actions they use are done with
    the touch bar, the X's equivalent in the firmware (bl.cpp, process_iqs323_data):

    - a short press (up to 1 s, which wakes the OG and refreshes) is a tap in the middle,
      which wakes the X (Update-Source: EXT0) and refreshes;
    - a long press (5-15 s: forget WiFi, open the setup portal) is the X's WiFi reset:
      hold both edges, then confirm the prompt with a 1.5 s hold in the middle
      (check_corners_gesture, handle_wifi_reset_confirmation);
    - the OG's double click or 1-5 s press (run the special function) and 15 s press (soft
      reset: forget the device too) have no touch bar equivalent: the X never reads the
      saved special function (bl_init's double_click branch is OG-only). A test using
      them is skipped (mark it @needs("double_click") / @needs("soft_reset_press")).
    """

    def __init__(self, *args, own_flash: Path | None = None, **kw):
        super().__init__(*args, **kw)
        self._own_flash = own_flash

    def press(self, ms: int = 100) -> None:
        if ms >= 15000:
            raise unittest.SkipTest("the TRMNL X has no soft-reset gesture (the OG's 15 s press)")
        if ms > 5000:
            self.reset_wifi_gesture()
        elif ms > 1000:
            raise unittest.SkipTest("the TRMNL X has no gesture that runs the special function (the OG's 1-5 s press)")
        else:
            self.touch("center", ms)

    def double_click(self, ms: int = 80, gap_ms: int = 150) -> None:
        raise unittest.SkipTest("the TRMNL X has no double click (the OG's special function gesture)")

    def button(self, down: bool) -> None:
        if down:
            self.touch_down("center")
        else:
            self.touch_up("center")

    def reset_wifi_gesture(self) -> None:
        """Forget WiFi and open the setup portal: both edges, then a middle hold. The X reads
        the gesture when it wakes, so it waits for the device to be asleep first."""
        if self.status()["state"] != "deep_sleep":
            self.wait(state="deep_sleep", timeout_s=120, settle_ms=200)
        ask_to_reset_wifi(self, self.status()["console_total"])
        self.touch("center", 1500)

    def close(self) -> None:
        super().close()
        if self._own_flash:
            self._own_flash.unlink(missing_ok=True)


def unboxed() -> Path:
    """The flash of an X a customer just unboxed: a shipped X (see ShippedX) taken out of
    shipment mode by its dock; it restarted into the setup portal and remembers being
    shipped, so booting this flash goes straight to the portal. The general tests' "fresh
    device" (sim(erase=True)) boots a copy of it, as the X never gets to the portal from an
    erased flash without the factory flow."""
    shipped = ShippedX()
    try:
        inputs = {"shipped": shipped.cache.name, "memcheck": MEMCHECK, "turbo": TURBO}
        cache, _ = setup_cache.entry("x-unboxed", inputs, lambda out: _unbox(shipped, out))
    finally:
        shipped.close()
    return cache / "flash.bin"


def _unbox(shipped: ShippedX, out: Path) -> dict:
    shutil.copy(shipped.template, out / "flash.bin")
    with x_sim(flash=out / "flash.bin", name="x-unbox") as s:
        s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
        s.dock(True)
        s.wait(portal=True, timeout_s=180)
    return {}


def general_sim(build: Path, **kw) -> XSim:
    """support.sim() for the X: an XSim; a factory-fresh device (`erase=True`, or no flash)
    boots a copy of the unboxed X instead of an erased flash (x_sim(erase=True) runs the
    factory flow)."""
    own = None
    erase = kw.pop("erase", False)
    if erase or (kw.get("flash") is None and kw.get("restore") is None):
        flash = kw.get("flash")
        if flash is None:
            fd, name = tempfile.mkstemp(prefix="trmnl-x-unboxed-", suffix=".bin")
            os.close(fd)
            flash = own = Path(name)
            kw["flash"] = flash
        shutil.copy(unboxed(), flash)
    return XSim(build, own_flash=own, **kw)


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


__all__ = ["X_BUILD", "X_MAC", "SSID_24", "SSID_5", "x_sim", "ShippedX", "ProvisionedX", "onboard", "XSim",
           "ask_to_reset_wifi", "unboxed", "general_sim"]
