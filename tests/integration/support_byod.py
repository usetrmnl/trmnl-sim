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

from support import ProvisionedDevice, build_of
from trmnl_mock import (big_number, checkerboard, png_image, expected_gray, spectra_bars, color_bars)


class ByodBoard:
    """Mixin for `unittest.TestCase`. Set on the subclass:

    ENV        PlatformIO environment (the build directory's name)
    NAME       the simulator's board name (status()["board"]["name"])
    MODEL      the firmware's DEVICE_MODEL (sent as the Model header)
    SIZE       the panel's (width, height) as the device reports it
    BATTERY_V  expected Battery-Voltage header at the default 4.1 V battery (None: don't check)
    INKS       "mono", "bwry", "spectra6" or "gray16" (a 16-gray parallel panel, served
               4-bit PNGs like the TRMNL X): what the panel shows
    """

    ENV: str
    NAME: str
    MODEL: str
    SIZE: tuple[int, int] = (800, 480)
    BATTERY_V: float | None = 4.1
    INKS = "mono"

    dev: ProvisionedDevice

    @classmethod
    def setUpClass(cls):
        build = build_of(cls.ENV)
        if not (build / "firmware.elf").exists():
            raise unittest.SkipTest(f"no {cls.ENV} build at {build} (pio run -e {cls.ENV})")
        cls.dev = ProvisionedDevice(build)

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

            bits = 4 if self.INKS == "gray16" else 1

            def level(x, y):
                ink = number(x, y) if y < h // 2 else board(x, y)
                return 0 if ink else (1 << bits) - 1

            m.images[name + ".png"] = png_image(level, w, h, bits=bits)
            m._stamp(name)
            expected = expected_gray(level, w, h, bits=bits)
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
