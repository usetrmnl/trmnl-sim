"""Shared tests for the BYOD boards (the firmware's `device_list[]` rows other than the
TRMNL OG, BWRY and X). A test module subclasses `ByodBoard` once per board:

    class XteinkX4(ByodBoard, unittest.TestCase):
        ENV = "xteink_x4"
        NAME = "Xteink X4"
        MODEL = "xteink_x4"
        SIZE = (800, 480)
        BATTERY_V = 0.0

Each class onboards its board through the captive portal (cached, see setup_cache), then
checks what the device reports and what it shows. Classes whose build is missing skip.
"""

import unittest

from support import MockTrmnl, ProvisionedDevice, build_of
from trmnl_mock import (big_number, checkerboard, png_image, expected_gray, spectra_bars, color_bars)


class PanelSizedDefault(ProvisionedDevice):
    """A provisioned device whose server's default screen is a PNG the panel's size and
    kind instead of the 800x480 BMP (see ByodBoard.PNG_DEFAULT)."""

    NAME = "provisioned-png-default"

    def __init__(self, build, size: tuple[int, int], inks: str):
        self.size, self.inks = size, inks
        super().__init__(build)

    def new_mock(self) -> MockTrmnl:
        mock = super().new_mock()
        w, h = self.size
        if self.inks == "spectra6":
            mock.set_spectra6_png("default", spectra_bars, w, h)
        elif self.inks == "bwry":
            mock.set_color_png("default", color_bars, w, h)
        else:
            number = big_number("0", scale=max(4, h // 30))
            mock.images["default.png"] = png_image(lambda x, y: 0 if number(x, y) else 1, w, h, bits=1)
            mock._stamp("default")
        return mock


class ByodBoard:
    """Mixin for `unittest.TestCase`. Set on the subclass:

    ENV        PlatformIO environment (the build directory's name)
    NAME       the simulator's board name (status()["board"]["name"])
    MODEL      the firmware's DEVICE_MODEL (sent as the Model header)
    SIZE       the panel's (width, height) as the device reports it
    BATTERY_V  expected Battery-Voltage header at the default 4.1 V battery (None: don't check)
    INKS       "mono", "bwry" or "spectra6": what the panel shows
    PNG_DEFAULT  serve a panel-sized PNG as the server's default screen (onboarding and
               setUp) instead of the 800x480 1-bit BMP, for panels whose firmware can't
               take that BMP
    """

    ENV: str
    NAME: str
    MODEL: str
    SIZE: tuple[int, int] = (800, 480)
    BATTERY_V: float | None = 4.1
    INKS = "mono"
    PNG_DEFAULT = False

    dev: ProvisionedDevice

    @classmethod
    def setUpClass(cls):
        build = build_of(cls.ENV)
        if not (build / "firmware.elf").exists():
            raise unittest.SkipTest(f"no {cls.ENV} build at {build} (pio run -e {cls.ENV})")
        cls.dev = PanelSizedDefault(build, cls.SIZE, cls.INKS) if cls.PNG_DEFAULT else ProvisionedDevice(build)

    @classmethod
    def tearDownClass(cls):
        cls.dev.close()

    def setUp(self):
        m = self.dev.mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def refresh(self, s, path: str) -> dict:
        """Wake the sleeping device; wait for it to fetch `path`, finish the refresh and sleep."""
        n = len(self.dev.mock.requests)
        s.wake()
        self.dev.mock.wait_for_request(path, after=n, timeout_s=60)
        return s.wait(state="deep_sleep", display_idle=True, timeout_s=90)["status"]

    def serve_test_image(self, name: str = "test") -> bytes:
        """Serve a test image for this panel as the current screen; returns the screenshot it
        should give."""
        w, h = self.SIZE
        m = self.dev.mock
        if self.INKS == "spectra6":
            expected = m.set_spectra6_png(name, spectra_bars, w, h)
        elif self.INKS == "bwry":
            expected = m.set_color_png(name, color_bars, w, h)
        else:
            number, board = big_number("42", scale=max(4, h // 30)), checkerboard(max(8, w // 20))

            def level(x, y):
                ink = number(x, y) if y < h // 2 else board(x, y)
                return 0 if ink else 1

            m.images[name + ".png"] = png_image(level, w, h, bits=1)
            m._stamp(name)
            expected = expected_gray(level, w, h, bits=1)
        m.display = {"image": name, "refresh_rate": 300}
        return expected

    # ---- the tests every board gets ---------------------------------------------------------

    def test_identifies_itself(self):
        with self.dev.boot_asleep() as s:
            self.assertEqual(s.status()["board"]["name"], self.NAME)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            n = len(self.dev.mock.requests)
            s.wake()
            req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
            self.assertEqual(req.headers["Model"], self.MODEL)
            self.assertEqual((int(req.headers["Width"]), int(req.headers["Height"])), self.SIZE)
            if self.BATTERY_V is not None:
                self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), self.BATTERY_V, delta=0.06)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)

    def test_shows_the_served_image(self):
        expected = self.serve_test_image()
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/test.png")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0.001)
            self.assertTrue(result["match"], result)
