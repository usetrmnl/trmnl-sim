"""TRMNL BWRY (`trmnl_4clr`): the OG board with a 4-color black/white/yellow/red panel."""

import unittest

from support import BWRY_BUILD, ProvisionedDevice
from trmnl_mock import BWRY_RGB, color_bars

dev: ProvisionedDevice


def setUpModule():
    global dev
    if not (BWRY_BUILD / "firmware.elf").exists():
        raise unittest.SkipTest(f"no trmnl_4clr build at {BWRY_BUILD} (set TRMNL_BWRY_BUILD)")
    dev = ProvisionedDevice(BWRY_BUILD)


def tearDownModule():
    dev.close()


class Bwry(unittest.TestCase):
    def setUp(self):
        dev.mock.requests.clear()
        dev.mock.display_queue.clear()
        dev.mock.display = {"image": "default", "refresh_rate": 300}

    def test_identifies_as_the_4_color_model(self):
        with dev.boot() as s:
            self.assertEqual(s.status()["board"]["name"], "TRMNL BWRY")
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["Model"], "og_4clr")
            self.assertEqual((req.headers["Width"], req.headers["Height"]), ("800", "480"))
            s.wait(state="deep_sleep", timeout_s=120)

    def test_color_png_is_rendered_in_four_colors(self):
        expected = dev.mock.set_color_png("bars", color_bars)
        dev.mock.display = {"image": "bars", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request("/images/bars.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            result = s.compare_screen(expected, tolerance=16, max_ratio=0)
            self.assertTrue(result["match"], result)

    def test_screenshot_is_rgb(self):
        dev.mock.set_color_png("red", lambda x, y: BWRY_RGB["red"])
        dev.mock.display = {"image": "red", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request("/images/red.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            png = s.screenshot(region=(0, 0, 8, 8))
            self.assertEqual(png[25], 2)  # IHDR color type 2: truecolor

    def test_refresh_takes_the_panel_s_long_update(self):
        dev.mock.set_color_png("bars", color_bars)
        dev.mock.display = {"image": "bars", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request("/images/bars.png", timeout_s=120)
            t0 = s.status()["sim_time_s"]
            st = s.wait(state="deep_sleep", timeout_s=120)["status"]
            self.assertGreater(st["sim_time_s"] - t0, 15)  # the 4-color update alone is ~16 s

if __name__ == "__main__":
    unittest.main()
