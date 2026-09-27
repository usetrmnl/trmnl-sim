"""BYOD boards with other UltraChip UC81xx panels: the 7.5" black/white/red TRMNL DIY kit,
the TRMNL Steam (5.83" 648x480) and the Xteink X3 (3.68" 792x528, BQ27220 fuel gauge)."""

import unittest

from support_byod import ByodBoard
from trmnl_mock import bwr_bars, expected_gray, png_image

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker


class DiyKitBwr(ByodBoard, unittest.TestCase):
    # The firmware shows 1-bit images on this panel (black/white plane, red plane cleared).
    ENV = "TRMNL_7inch5_OG_DIY_Kit_3CLR"
    NAME = "TRMNL 7.5\" BWR DIY Kit"
    MODEL = "xiao_epaper_3clr"

    def test_refresh_takes_the_panel_s_long_color_update(self):
        self.serve_test_image()
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.assertTrue(s.status()["board"]["has_refresh_flashing"])
            n = len(self.dev.mock.requests)
            s.wake()
            self.dev.mock.wait_for_request("/images/test.png", after=n, timeout_s=60)
            t0 = s.status()["sim_time_s"]
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=90)["status"]
            self.assertGreater(st["sim_time_s"] - t0, 15)  # the OTP color update alone is ~16 s

    @unittest.expectedFailure
    def test_shows_red(self):
        # Firmware bug: display.cpp has no black/white/red image path (GetBWRPixel is never
        # called). A 3-color palette PNG has 2 bits per pixel and more than two colors, so
        # png_to_epd takes the 4-gray path and writes the gray bit planes into the panel's
        # black/white and red planes: white comes out black, black and red come out red.
        expected = self.dev.mock.set_bwr_png("bwr", bwr_bars)
        self.dev.mock.display = {"image": "bwr", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/bwr.png")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0.001)
            self.assertTrue(result["match"], result)

    @unittest.expectedFailure
    def test_4_gray_png_is_shown_in_black_and_white(self):
        # Firmware bug (as above): a 4-gray PNG goes down the 4-gray path, whose plane 0/1
        # split lands in DTM1 (black/white) and DTM2 (red) of this panel. The least it
        # should do is threshold the grays to black and white.
        def level(x, y):
            return min(3, x * 4 // 800)

        m = self.dev.mock
        m.images["gray4.png"] = png_image(level, 800, 480, bits=2)
        m._stamp("gray4")
        m.display = {"image": "gray4", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/gray4.png")
            result = s.compare_screen(expected_gray(lambda x, y: level(x, y) >> 1, 800, 480, 1),
                                      tolerance=16, max_ratio=0.001)
            self.assertTrue(result["match"], result)


class TrmnlSteam(ByodBoard, unittest.TestCase):
    ENV = "trmnl_steam"
    NAME = "TRMNL Steam"
    MODEL = "trmnl_steam"
    SIZE = (648, 480)


class XteinkX3(ByodBoard, unittest.TestCase):
    ENV = "xteink_x3"
    NAME = "Xteink X3"
    MODEL = "xteink_x3"
    SIZE = (792, 528)

    def test_battery_voltage_comes_from_the_fuel_gauge(self):
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            s.set_battery(3720)
            n = len(self.dev.mock.requests)
            s.wake()
            req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 3.72, delta=0.02)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)

    def test_2bit_png_uses_4_gray_levels(self):
        w, h = self.SIZE

        def level(x, y):
            return 0 if 200 <= y < 260 else min(3, x * 4 // w)

        m = self.dev.mock
        m.images["gray4.png"] = png_image(level, w, h, bits=2)
        m._stamp("gray4")
        m.display = {"image": "gray4", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/gray4.png")
            result = s.compare_screen(expected_gray(level, w, h, bits=2), tolerance=48, max_ratio=0.01)
            self.assertTrue(result["match"], result)

    @unittest.expectedFailure
    def test_survives_an_800x480_bmp(self):
        # Firmware bug: display_show_image flips an uncompressed BMP in place with the
        # display's size (flip_image(image_buffer+62, bbep.width(), bbep.height())) without
        # checking the BMP's. 792x528 needs 52272 bytes; an OG-sized 800x480 BMP has 48000,
        # so the flip writes past the download buffer, corrupting the heap: the device
        # panics (tlsf_free) and reboots, and repeats that on every wake.
        m = self.dev.mock
        m.set_image("og", lambda x, y: (x // 40 + y // 40) % 2 == 0)
        m.display = {"image": "og", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            boots = s.status()["boot_count"]
            self.refresh(s, "/images/og.bmp")
            self.assertEqual(s.status()["boot_count"], boots)


if __name__ == "__main__":
    unittest.main()
