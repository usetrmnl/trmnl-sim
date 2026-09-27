"""The gen-2 TRMNL OG on the ESP32-C5: `og_gen2` (env trmnl_gen2, the 7.5" black and white
UC8179 panel) and `og_gen2_4clr` (trmnl_gen2_4clr, the 4-color BWRY panel). Both have a
BQ27427 fuel gauge on I2C and a BQ25616 charger whose open-drain PG/STAT outputs are on
GPIO 25/24; the simulator's dock switch plugs the USB cable in. The C5's own radio does
2.4 and 5 GHz. Classes whose build is missing skip."""

import tempfile
import unittest
from pathlib import Path

from support import MockTrmnl, sim
from support_byod import ByodBoard
from trmnl_mock import BWRY_RGB, big_number, color_bars

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker


class Gen2Tests(ByodBoard):
    """What both gen-2 boards get on top of the BYOD tests."""

    # Served images show as they should (see OgGen2Bwry for the board where they don't).
    IMAGES_WORK = True

    def wake_for_display(self, s, wake=None):
        """Wake the sleeping device (timer, or `wake`); return its /api/display request once
        it sleeps again."""
        s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
        n = len(self.dev.mock.requests)
        (wake or s.wake)()
        req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
        s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
        return req

    def test_reports_the_battery_from_the_fuel_gauge(self):
        with self.dev.boot_asleep() as s:
            s.set_battery(3700)
            req = self.wake_for_display(s)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 3.70, delta=0.02)

    def test_usb_power_and_charging_come_from_the_charger_lines(self):
        with self.dev.boot_asleep() as s:
            h = self.wake_for_display(s).headers
            self.assertEqual((h["USB-Connected"], h["Battery-Charging"]), ("false", "0"))
            s.dock(True)
            self.assertTrue(s.status()["charging"])
            h = self.wake_for_display(s).headers
            self.assertEqual((h["USB-Connected"], h["Battery-Charging"]), ("true", "1"))
            # charged: USB still in, STAT high
            s.set_battery(4200)
            h = self.wake_for_display(s).headers
            self.assertEqual((h["USB-Connected"], h["Battery-Charging"]), ("true", "0"))

    def test_timer_wake(self):
        with self.dev.boot_asleep() as s:
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=60)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 300, delta=15)
            req = self.wake_for_display(s)
            self.assertEqual(req.headers["Update-Source"], "timer")
            st = s.status()
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 300, delta=15)

    def test_button_wakes_it(self):
        with self.dev.boot_asleep() as s:
            req = self.wake_for_display(s, wake=lambda: s.press(150))
            self.assertEqual(req.headers["Update-Source"], "button")

    def test_save_point_round_trip(self):
        m = self.dev.mock
        self.serve_test_image("first")
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "gen2.trmnlsave"
            with self.dev.boot_asleep() as s:
                self.refresh(s, "/images/first.png")
                s.dock(True)
                saved = s.save_point(path)
                screen = s.screenshot()
                boots = s.status()["boot_count"]
            self.assertTrue(saved["deep_sleep"])
            if self.INKS == "bwry":
                second = m.set_color_png("second", lambda x, y: BWRY_RGB["red" if x < 400 else "yellow"])
            else:
                second = m.set_image("second", big_number("7"))
            m.display = {"image": "second", "refresh_rate": 300}
            m.requests.clear()
            with self.dev.restore(path) as s:
                st = s.wait(state="deep_sleep", timeout_s=30)["status"]
                self.assertEqual((st["boot_count"], st["docked"], st["charging"]), (boots, True, True))
                self.assertTrue(s.compare_screen(screen, tolerance=0, max_ratio=0)["match"])
                self.assertFalse(s.compare_screen(second, tolerance=16, max_ratio=0.001)["match"])
                req = self.wake_for_display(s)
                self.assertEqual(req.headers["USB-Connected"], "true")
                self.assertNotIn("/api/setup", [r.path for r in m.requests])
                self.assertIn("/images/second.png" if self.INKS == "bwry" else "/images/second.bmp",
                              [r.path for r in m.requests])
                if self.IMAGES_WORK:
                    result = s.compare_screen(second, tolerance=16, max_ratio=0.001)
                    self.assertTrue(result["match"], result)

    def test_power_off_save_point_boots_again(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "off.trmnlsave"
            with self.dev.boot_asleep() as s:
                s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
                s.wake()
                s.wait(state="running", timeout_s=30)
                saved = s.save_point(path)
            self.assertFalse(saved["deep_sleep"])
            n = len(self.dev.mock.requests)
            with self.dev.restore(path) as s:
                req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
                self.assertEqual(req.headers["Model"], self.MODEL)
                s.wait(state="deep_sleep", display_idle=True, timeout_s=120)

    def test_onboards_on_5_ghz_with_its_own_radio(self):
        with MockTrmnl() as mock, sim(self.dev.build, erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            scan = {n["name"]: n for n in s.portal_scan()["networks"]}
            self.assertIn("TRMNL-Sim-5G", scan)
            self.assertIn("TRMNL-Sim", scan)
            s.portal_connect("TRMNL-Sim-5G", "password", server=mock.device_url)
            req = mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["RSSI"], "-48")
            self.assertEqual(req.headers["WiFi-Band"], "5")
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)

    def test_onboarding_on_2_4_ghz_reports_the_band(self):
        with self.dev.boot_asleep() as s:
            req = self.wake_for_display(s)
            self.assertEqual((req.headers["RSSI"], req.headers["WiFi-Band"]), ("-54", "2.4"))

    def test_https_uses_the_crypto_accelerators(self):
        # ECDHE-ECDSA with a P-384 certificate: the C5's ECC (point multiplication) and ECDSA
        # (signature verification) accelerators, SHA and AES-GCM over DMA
        with MockTrmnl(tls=True) as mock, sim(self.dev.build, erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
            req = mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["Model"], self.MODEL)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            self.assertIn("/api/setup", [r.path for r in mock.requests])


class OgGen2(Gen2Tests, unittest.TestCase):
    ENV = "trmnl_gen2"
    NAME = "TRMNL OG gen 2"
    MODEL = "og_gen2"

    def test_memcheck_onboarding_and_refresh_cycles(self):
        # The heap checker follows the C5's IDF 5.5 heap and its single-core FreeRTOS tasks
        # (known firmware bugs suppressed, see support.KNOWN_MEMORY_BUGS).
        with MockTrmnl() as mock, sim(self.dev.build, erase=True, memcheck="halt", extra_args=("--offline",)) as s:
            mock.set_image("one", big_number("1"))
            mock.display = {"image": "one", "refresh_rate": 300}
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
            mock.wait_for_request("/api/display", timeout_s=120)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            n = len(mock.requests)
            s.press(150)
            mock.wait_for_request("/api/display", after=n, timeout_s=90)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
            report = s.memcheck()
            self.assertEqual(report["violations"], [])
            self.assertGreater(report["heap"]["allocs"], 500)
            stacks = {t["task"]: t for t in report["stacks"]}
            self.assertEqual(stacks["loopTask"]["size"], 8192)
            self.assertGreaterEqual(stacks["loopTask"]["instances"], 2)  # one per boot

    def test_coverage_of_the_setup_boot(self):
        with tempfile.TemporaryDirectory() as tmp, \
                sim(self.dev.build, erase=True, coverage=Path(tmp) / "c5.info", extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            cov = s.write_coverage(Path(tmp) / "mid.info")
            self.assertGreater(cov["lines_hit"], 1000)
            self.assertLess(cov["lines_hit"], cov["lines_found"])
            info = (Path(tmp) / "mid.info").read_text()
            self.assertIn("SF:src/bl.cpp", info)


class OgGen2Bwry(Gen2Tests, unittest.TestCase):
    """FIRMWARE BUG: the trmnl_gen2_4clr env defines BOARD_TRMNL_GEN2 but not
    BOARD_TRMNL_4CLR, which display.cpp's 4-color image path (png_draw_4clr, 2 bits per pixel
    into DTM1) is compiled under. Images therefore take the generic path: two 1-bit planes of
    48000 bytes (DTM2, then DTM1). The BWRY panel reads DTM1 as 2 bits per pixel, so only the
    top half of the screen changes, in the wrong inks, and the rest keeps the old picture
    (color PNGs and 1-bit images alike). The setup and message screens, drawn through
    bb_epaper's 4-color buffer, are fine."""

    ENV = "trmnl_gen2_4clr"
    NAME = "TRMNL BWRY gen 2"
    MODEL = "og_gen2_4clr"
    INKS = "bwry"
    IMAGES_WORK = False

    @unittest.expectedFailure
    def test_shows_the_served_image(self):
        super().test_shows_the_served_image()

    @unittest.expectedFailure
    def test_color_png_is_rendered_in_four_colors(self):
        m = self.dev.mock
        expected = m.set_color_png("bars", color_bars)
        m.display = {"image": "bars", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/bars.png")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0)
            self.assertTrue(result["match"], result)
            self.assertEqual(s.screenshot(region=(0, 0, 8, 8))[25], 2)  # truecolor PNG

    def test_refresh_takes_the_panel_s_long_update(self):
        m = self.dev.mock
        m.set_color_png("red", lambda x, y: BWRY_RGB["red"])
        m.display = {"image": "red", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            n = len(m.requests)
            s.wake()
            m.wait_for_request("/images/red.png", after=n, timeout_s=60)
            t0 = s.status()["sim_time_s"]
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=120)["status"]
            self.assertGreater(st["sim_time_s"] - t0, 15)  # the 4-color update alone is ~16 s


del Gen2Tests  # not a test case on its own

if __name__ == "__main__":
    unittest.main()
