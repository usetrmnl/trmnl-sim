"""TRMNL X: factory flow, shipment mode and the dock, onboarding over 2.4 GHz (S3 WiFi) and
5 GHz (ESP32-C5 modem), the 1872x1404 parallel panel, the touch bar, and charging headers."""

import unittest

from support import MockTrmnl, big_number
from trmnl_mock import expected_gray, png_image
from support_x import SSID_24, ProvisionedX, ShippedX, X_BUILD, onboard

shipped: ShippedX
dev: ProvisionedX


def setUpModule():
    global shipped, dev
    if not (X_BUILD / "firmware.elf").exists():
        raise unittest.SkipTest(f"no TRMNL_X build at {X_BUILD} (set TRMNL_X_BUILD)")
    shipped = ShippedX()
    dev = ProvisionedX(shipped)  # onboarded on the 5 GHz network, through the modem


def tearDownModule():
    dev.close()
    shipped.close()


def digits(text: str):
    """big_number() scaled up to the X panel, as a 1-bit level function (0 = black)."""
    pixel = big_number(text)
    return lambda x, y: 0 if pixel(x * 800 // 1872, y * 480 // 1404) else 1


def ramp16(x: int, y: int) -> int:
    return min(15, x * 16 // 1872)


class FactoryAndDock(unittest.TestCase):
    def test_factory_flow_flashes_modem_then_ships(self):
        log = "\n".join(shipped.factory_console)
        self.assertIn("[MODEM] FLASH COMPLETE!", log)
        self.assertIn("Entering shipment mode light sleep loop", log)

    def test_shipment_mode_waits_for_the_dock(self):
        with shipped.boot() as s:
            s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
            st = s.wait(state="light_sleep", timeout_s=60)["status"]
            self.assertFalse(st["docked"])
            self.assertIsNone(st["portal_url"])
            s.dock(True)
            st = s.wait(portal=True, timeout_s=180)["status"]
            self.assertTrue(st["docked"])
            self.assertGreaterEqual(st["boot_count"], 2)  # restarted out of shipment mode

    def test_portal_soft_reset_restarts_from_core_0(self):
        # /soft-reset calls ESP.restart() on async_tcp (core 0), which resets the APP core
        # first and must not see it come back before both cores restart.
        with shipped.boot() as s:
            s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
            s.dock(True)
            boots = s.wait(portal=True, timeout_s=180)["status"]["boot_count"]
            try:
                s.portal_request("/soft-reset", retry_s=0)
            except (ConnectionError, OSError):
                pass  # the device may restart before it answers
            st = s.wait(min_boots=boots + 1, portal=True, timeout_s=180)["status"]
            self.assertNotEqual(st["state"], "halted")

    def test_onboarding_on_2_4_ghz_uses_the_s3_radio(self):
        with MockTrmnl() as mock, shipped.boot() as s:
            onboard(s, mock, SSID_24)
            req = mock.wait_for_request("/api/display")
            self.assertEqual(req.headers["RSSI"], "-54")  # the 2.4 GHz AP, seen by the S3


class Provisioned(unittest.TestCase):
    def setUp(self):
        dev.mock.requests.clear()
        dev.mock.display_queue.clear()
        dev.mock.display = {"image": "default", "refresh_rate": 300}

    def test_5_ghz_requests_go_through_the_modem(self):
        with dev.boot() as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["RSSI"], "-48")  # the 5 GHz AP, seen by the modem
            for h in ("ID", "FW-Version", "Model", "Battery-Voltage", "Width", "Height"):
                self.assertIn(h, req.headers)
            self.assertEqual((req.headers["Width"], req.headers["Height"]), ("1872", "1404"))
            self.assertEqual(req.headers["Model"], "x")
            s.wait(state="deep_sleep", timeout_s=120)

    def test_1bit_png_is_rendered_exactly(self):
        expected = dev.mock.set_png("seven", digits("7"))
        dev.mock.display = {"image": "seven", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request("/images/seven.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            result = s.compare_screen(expected, tolerance=64, max_ratio=0)
            self.assertTrue(result["match"], result)

    def test_4bit_png_shows_16_gray_levels(self):
        expected = dev.mock.set_png("ramp", ramp16, bits=4)
        dev.mock.display = {"image": "ramp", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request("/images/ramp.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            result = s.compare_screen(expected, tolerance=40, max_ratio=0.001)
            self.assertTrue(result["match"], result)

    def test_sleeps_for_refresh_rate(self):
        dev.mock.display = {"image": "default", "refresh_rate": 600}
        with dev.boot() as s:
            st = s.wait(state="deep_sleep", timeout_s=120)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 600, delta=15)

    def test_center_tap_wakes_and_refreshes(self):
        with dev.boot() as s:
            s.wait(state="deep_sleep", timeout_s=120)
            n = len(dev.mock.requests)
            s.touch("center", 150)
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=120)
            self.assertEqual(req.headers["Update-Source"], "EXT0")
            s.wait(state="deep_sleep", timeout_s=120)

    def test_left_tap_shows_the_previous_image_offline(self):
        one = dev.mock.set_png("one", digits("1"))
        two = dev.mock.set_png("two", digits("2"))
        dev.mock.display_queue = [{"image": "one", "refresh_rate": 300}]
        dev.mock.display = {"image": "two", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request("/images/one.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            s.wake()
            dev.mock.wait_for_request("/images/two.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=500)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])
            n, c = len(dev.mock.requests), s.status()["console_total"]
            s.touch("left", 150)
            s.wait(console=r"Playlist browse", since=c, timeout_s=60)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])
            self.assertEqual([r.path for r in dev.mock.requests[n:]], [])  # no network needed

    def test_dock_reports_usb_power_and_charging(self):
        with dev.boot() as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual((req.headers["USB-Connected"], req.headers["Battery-Charging"]), ("false", "0"))
            s.wait(state="deep_sleep", timeout_s=120)
            s.dock(True)
            self.assertTrue(s.status()["charging"])
            n = len(dev.mock.requests)
            s.wake()
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=120)
            self.assertEqual((req.headers["USB-Connected"], req.headers["Battery-Charging"]), ("true", "1"))
            s.wait(state="deep_sleep", timeout_s=120)


class BuiltinServer(unittest.TestCase):
    def test_onboards_against_the_built_in_server(self):
        with shipped.boot() as s:
            s.mock.start()
            s.mock.add_image("seven", png_image(digits("7"), 1872, 1404), current=True)
            s.mock.display(refresh_rate=300)
            onboard(s, s.mock, SSID_24)
            self.assertEqual(s.mock.count("/images/seven.png"), 1)
            expected = expected_gray(digits("7"), 1872, 1404)
            self.assertTrue(s.compare_screen(expected, tolerance=64, max_ratio=0)["match"])
            self.assertTrue(s.compare_screen(s.mock.expected("seven"), tolerance=64, max_ratio=0)["match"])


if __name__ == "__main__":
    unittest.main()
