"""Seeed reTerminal E1002 (`seeed_reTerminal_E1002`): an ESP32-S3 with a 7.3" Spectra 6
panel (black, white, yellow, red, blue, green), one button and a switched battery divider."""

import tempfile
import unittest
from pathlib import Path

from support import E1002_BUILD, GOLDEN, ProvisionedDevice, close_fixtures, fixture, sim
from trmnl_mock import (SPECTRA6_RGB, expected_spectra6, png_image, png_palette, png_rgb, png_rgba,
                        spectra_bars)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

dev = fixture(lambda: ProvisionedDevice(E1002_BUILD))


def setUpModule():
    if not (E1002_BUILD / "firmware.elf").exists():
        raise unittest.SkipTest(f"no seeed_reTerminal_E1002 build at {E1002_BUILD} (set TRMNL_E1002_BUILD)")


def tearDownModule():
    close_fixtures()


class Case(unittest.TestCase):
    def setUp(self):
        m = dev().mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def refresh(self, s, path: str):
        """Wake the sleeping device and wait for it to show `path` and sleep again."""
        n = len(dev().mock.requests)
        s.wake()
        dev().mock.wait_for_request(path, after=n, timeout_s=30)
        return s.wait(state="deep_sleep", timeout_s=60, settle_ms=300)["status"]


class Identity(Case):
    def test_reports_the_e1002_model_and_battery(self):
        with dev().boot_asleep() as s:
            self.assertEqual(s.status()["board"]["name"], "reTerminal E1002")
            s.wait(state="deep_sleep", timeout_s=30)
            s.wake()
            req = dev().mock.wait_for_request("/api/display", timeout_s=30)
            self.assertEqual(req.headers["Model"], "reterminal_e1002")
            self.assertNotIn("Panel-Rev", req.headers)  # not read on Spectra panels
            self.assertEqual((req.headers["Width"], req.headers["Height"]), ("800", "480"))
            # 4.1 V through the divider, read while the firmware switches it on (GPIO21)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 4.1, delta=0.05)
            s.wait(state="deep_sleep", timeout_s=60)


class Colors(Case):
    def test_palette_png_shows_the_six_inks(self):
        expected = dev().mock.set_spectra6_png("bars", spectra_bars)
        dev().mock.display = {"image": "bars", "refresh_rate": 300}
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            self.refresh(s, "/images/bars.png")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0)
            self.assertTrue(result["match"], result)

    def test_truecolor_png_is_reduced_to_the_inks(self):
        def color(x, y):  # the inks, and in-between colors the firmware maps to the nearest
            return SPECTRA6_RGB["blue"] if y < 240 else (x * 255 // 799, 128, 255 - x * 255 // 799)

        dev().mock.set_file("/img/rgb.png", "image/png", png_rgb(color))
        dev().mock.display = {"image_url": dev().mock.device_url + "/img/rgb.png", "filename": "plugin-rgb-1",
                              "refresh_rate": 300}
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            self.refresh(s, "/img/rgb.png")
            result = s.compare_screen(expected_spectra6(color), tolerance=16, max_ratio=0)
            self.assertTrue(result["match"], result)

    def test_refresh_takes_the_panel_s_long_update(self):
        dev().mock.set_spectra6_png("bars", spectra_bars)
        dev().mock.display = {"image": "bars", "refresh_rate": 300}
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            n = len(dev().mock.requests)
            s.wake()
            dev().mock.wait_for_request("/images/bars.png", after=n, timeout_s=30)
            t0 = s.status()["sim_time_s"]
            st = s.wait(state="deep_sleep", timeout_s=60)["status"]
            self.assertGreater(st["sim_time_s"] - t0, 18)  # the Spectra 6 update alone is ~19 s


def hues(x: int, y: int) -> tuple:
    """Colors between the inks: a red-to-blue sweep over the top half, green-to-yellow below."""
    t = x * 255 // 799
    return (t, 128, 255 - t) if y < 240 else (t, 200, 40)


class PngFormats(Case):
    """Every pixel format png_draw_6clr decodes, reduced to the nearest ink."""

    def show(self, name: str, data: bytes, expected: bytes):
        dev().mock.set_file(f"/img/{name}.png", "image/png", data)
        dev().mock.display = {"image_url": f"{dev().mock.device_url}/img/{name}.png",
                              "filename": f"plugin-{name}-1", "refresh_rate": 300}
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            self.refresh(s, f"/img/{name}.png")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0)
            self.assertTrue(result["match"], result)

    def gray(self, bits: int, expand):
        # the firmware widens gray samples its own way: 1-bit white is 0x80, 2-bit tops out at 0xc0
        top = (1 << bits) - 1

        def level(x, y):
            return (x * (top + 1) // 800 + y // 120) % (top + 1)

        self.show(f"gray{bits}", png_image(level, 800, 480, bits),
                  expected_spectra6(lambda x, y: (expand(level(x, y)),) * 3))

    def test_1bit_gray(self):
        self.gray(1, lambda v: v << 7)

    def test_2bit_gray(self):
        self.gray(2, lambda v: v << 6)

    def test_4bit_gray(self):
        self.gray(4, lambda v: v * 17)

    def test_8bit_gray(self):
        self.gray(8, lambda v: v)

    def indexed(self, bits: int):
        palette = [hues(x * 800 >> bits, (x & 1) * 240) for x in range(1 << bits)]
        palette = list(dict.fromkeys(palette))  # PLTE entries must be distinct for the index map

        def color(x, y):
            return palette[(x * len(palette) // 800 + y // 120) % len(palette)]

        self.show(f"pal{bits}", png_palette(color, palette, bits=bits),
                  expected_spectra6(color))

    def test_1bit_palette(self):
        self.indexed(1)

    def test_2bit_palette(self):
        self.indexed(2)

    def test_8bit_palette(self):
        self.indexed(8)

    def test_truecolor_with_alpha(self):
        self.show("rgba", png_rgba(hues), expected_spectra6(hues))


class Setup(unittest.TestCase):
    def test_setup_screen_matches_the_og_s(self):
        # Same 800x480 layout, access point name and QR code as the OG (all but the version line).
        with sim(E1002_BUILD, erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=30)
            s.wait(display_idle=True, min_refreshes=3, settle_ms=500, timeout_s=30)
            s.assert_screen(GOLDEN / "setup_screen_body.png", region=(0, 56, 800, 424))
            s.assert_screen(GOLDEN / "setup_screen_top_right.png", region=(320, 0, 480, 56))


class SavePoint(Case):
    def test_save_point_keeps_the_color_image(self):
        m = dev().mock
        bars = m.set_spectra6_png("bars", spectra_bars)
        m.display = {"image": "bars", "refresh_rate": 300}
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "e1002.trmnlsave"
            with dev().boot_asleep() as s:
                s.wait(state="deep_sleep", timeout_s=30)
                self.refresh(s, "/images/bars.png")
                s.save_point(path)
            m.set_spectra6_png("green", lambda x, y: SPECTRA6_RGB["green"])
            m.display = {"image": "green", "refresh_rate": 300}
            with dev().restore(path) as s:
                s.wait(state="deep_sleep", timeout_s=30)
                self.assertEqual(s.status()["board"]["name"], "reTerminal E1002")
                self.assertTrue(s.compare_screen(bars, tolerance=0, max_ratio=0)["match"])
                self.refresh(s, "/images/green.png")
                self.assertEqual(s.screenshot(region=(0, 0, 1, 1))[25], 2)  # truecolor screenshot
                green = expected_spectra6(lambda x, y: SPECTRA6_RGB["green"])
                self.assertTrue(s.compare_screen(green, tolerance=0, max_ratio=0)["match"])


class Button(Case):
    def test_button_press_wakes_and_refreshes(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            n = len(dev().mock.requests)
            s.press(100)
            dev().mock.wait_for_request("/api/display", after=n, timeout_s=30)
            s.wait(state="deep_sleep", timeout_s=60)


if __name__ == "__main__":
    unittest.main()
