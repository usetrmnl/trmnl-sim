"""BYOD boards with SSD16xx panels: the 4.26" 800x480 (Xteink X4, TRMNL 4.26" DIY kit), the
3.97" 800x480 (Waveshare ESP32-S3 3.97", Seeed Sticky) and the 4.2" 400x300 (CrowPanel)."""

import unittest

from support_byod import ByodBoard
from trmnl_mock import big_number, expected_gray, png_image

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker


class SsdBoard(ByodBoard):
    """What the SSD16xx boards have beyond the shared tests."""

    def gray_level(self, x, y):
        # four vertical bars, black to white, and a black/white strip along the bottom
        w, h = self.SIZE
        if y >= h - h // 8:
            return 0 if (x // 16) % 2 else 3
        return x * 4 // w

    def test_shows_a_4_gray_image(self):
        w, h = self.SIZE
        m = self.dev.mock
        m.images["gray.png"] = png_image(self.gray_level, w, h, bits=2)
        m._stamp("gray")
        m.display = {"image": "gray", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/gray.png")
            result = s.compare_screen(expected_gray(self.gray_level, w, h, bits=2), tolerance=16, max_ratio=0.001)
            self.assertTrue(result["match"], result)

    def test_partial_refresh_after_a_full_one(self):
        # 1-bit images refresh partially (differential) once the panel holds one; each
        # must end up exactly on screen.
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            for name in ("first", "second", "third"):
                expected = self.serve_test_image(name)
                self.refresh(s, f"/images/{name}.png")
                result = s.compare_screen(expected, tolerance=16, max_ratio=0.001)
                self.assertTrue(result["match"], (name, result))

    @unittest.expectedFailure
    def test_bmp_after_a_fast_refresh(self):
        # Firmware bug: display_show_image() writes 1-bit BMP (and Group5) images to the
        # SSD16xx's new-image RAM only (writePlane() = PLANE_BOTH without a second plane)
        # and asks for a partial refresh. SSD16xx partial refreshes are differential: they
        # drive only the pixels where the new image differs from the "old" RAM (0x26), which
        # the controller updates itself only after a partial refresh. After a fast or full
        # refresh of a PNG, which leaves the inverted PNG there (PLANE_FALSE_DIFF), every
        # pixel that should change counts as unchanged and the old picture stays up.
        # (PNGs are fine: png_to_epd() writes the inverted image to 0x26 every time.)
        w, h = self.SIZE
        if (w, h) != (800, 480):
            self.skipTest("the mock serves 800x480 BMPs")
        m = self.dev.mock
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.serve_test_image("png")
            m.display["refresh_rate"] = 3600  # 30 min or more: fast instead of partial refreshes
            self.refresh(s, "/images/png.png")
            expected = m.set_image("bmp", big_number("7", scale=20))
            m.display = {"image": "bmp", "refresh_rate": 300}
            self.refresh(s, "/images/bmp.bmp")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0.001)
            self.assertTrue(result["match"], result)

    def serve_test_image(self, name: str = "test") -> bytes:
        if name == "test":
            return super().serve_test_image(name)
        # a different picture per name, so every refresh changes pixels
        w, h = self.SIZE
        seed = sum(map(ord, name))
        size = 8 + seed % 24

        def level(x, y):
            return 0 if ((x + seed) // size + (y // size)) % 2 else 1

        m = self.dev.mock
        m.images[name + ".png"] = png_image(level, w, h, bits=1)
        m._stamp(name)
        m.display = {"image": name, "refresh_rate": 300}
        return expected_gray(level, w, h, bits=1)


class XteinkX4(SsdBoard, unittest.TestCase):
    ENV = "xteink_x4"
    NAME = "Xteink X4"
    MODEL = "xteink_x4"
    BATTERY_V = 0.0  # device_list[] has batt_pin 0xff

    @unittest.expectedFailure
    def test_reports_the_battery_voltage(self):
        # Firmware bug: the X4's battery divider is on GPIO0 (config.h: PIN_BATTERY 0 for
        # BOARD_XTEINK_X4), but its device_list[] row has batt_pin 0xff, so readVoltage()
        # reads no ADC pin and the X4 always reports 0 V.
        with self.dev.boot_asleep() as s:
            s.set_battery(3800)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            n = len(self.dev.mock.requests)
            s.wake()
            req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 3.8, delta=0.06)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)


class DiyKit426(SsdBoard, unittest.TestCase):
    ENV = "TRMNL_4inch26_DIY_Kit"
    NAME = "TRMNL 4.26\" DIY Kit"
    MODEL = "xiao_epaper_mini"


class Waveshare397(SsdBoard, unittest.TestCase):
    ENV = "WAVESHARE_397"
    NAME = "Waveshare ESP32-S3 3.97\""
    MODEL = "waveshare_397"

    def test_reports_the_battery_from_the_pmic(self):
        with self.dev.boot_asleep() as s:
            s.set_battery(3650)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            n = len(self.dev.mock.requests)
            s.wake()
            req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 3.65, delta=0.02)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)


class SeeedSticky(SsdBoard, unittest.TestCase):
    ENV = "seeed_sticky"
    NAME = "Seeed Sticky"
    MODEL = "seeed_sticky"

    def test_reports_the_battery_from_the_gauge(self):
        with self.dev.boot_asleep() as s:
            s.set_battery(3650)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            n = len(self.dev.mock.requests)
            s.wake()
            req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 3.65, delta=0.02)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)


class CrowPanel42(SsdBoard, unittest.TestCase):
    ENV = "CrowPanel42"
    NAME = "CrowPanel 4.2\""
    MODEL = "crowpanel42"
    SIZE = (400, 300)
    BATTERY_V = 4.2  # BATT_NONE: a fixed 4.2 V


if __name__ == "__main__":
    unittest.main()
