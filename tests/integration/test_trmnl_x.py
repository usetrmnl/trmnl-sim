"""TRMNL X: factory flow, shipment mode and the dock, onboarding over 2.4 GHz (S3 WiFi) and
5 GHz (ESP32-C5 modem), the 1872x1404 parallel panel, the touch bar, and charging headers."""

import json
import tempfile
import time
import unittest
from pathlib import Path

from support import MockTrmnl, big_number, close_fixtures, fixture
from trmnl_mock import expected_gray, png_image
from support_x import SSID_5, SSID_24, ProvisionedX, ShippedX, X_BUILD, onboard, x_sim

ENV = "TRMNL_X"  # the PlatformIO environment these tests run (bin/spec TRMNL_X)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

shipped = fixture(ShippedX)
dev = fixture(lambda: ProvisionedX(shipped()))  # onboarded on the 5 GHz network, through the modem


def setUpModule():
    if not (X_BUILD / "firmware.elf").exists():
        raise unittest.SkipTest(f"no TRMNL_X build at {X_BUILD} (set TRMNL_X_BUILD)")


def tearDownModule():
    close_fixtures()


def digits(text: str):
    """big_number() scaled up to the X panel, as a 1-bit level function (0 = black)."""
    pixel = big_number(text)
    return lambda x, y: 0 if pixel(x * 800 // 1872, y * 480 // 1404) else 1


def ramp16(x: int, y: int) -> int:
    return min(15, x * 16 // 1872)


class FactoryAndDock(unittest.TestCase):
    def test_factory_flow_flashes_modem_then_ships(self):
        log = "\n".join(shipped().factory_console)
        self.assertIn("[MODEM] FLASH COMPLETE!", log)
        self.assertIn("Entering shipment mode light sleep loop", log)

    def test_factory_flow_waits_until_taken_off_the_dock_to_ship(self):
        # Still on USB power when QA is done: "ready to ship" until it comes off the dock.
        with x_sim(erase=True) as s:
            s.dock(True)
            s.wait_for_console(r"USB power still detected", timeout_s=30)
            s.dock(False)
            s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=30)

    def test_shipment_mode_waits_for_the_dock(self):
        with shipped().boot() as s:
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
        with shipped().boot() as s:
            s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
            s.dock(True)
            boots = s.wait(portal=True, timeout_s=180)["status"]["boot_count"]
            try:
                s.portal_request("/soft-reset", retry_s=0)
            except (ConnectionError, OSError):
                pass  # the device may restart before it answers
            st = s.wait(min_boots=boots + 1, portal=True, timeout_s=180)["status"]
            self.assertNotEqual(st["state"], "halted")


class ProvisionedCase(unittest.TestCase):
    def setUp(self):
        dev().mock.requests.clear()
        dev().mock.display_queue.clear()
        dev().mock.display = {"image": "default", "refresh_rate": 300}


class Provisioned(ProvisionedCase):
    def test_5_ghz_requests_go_through_the_modem(self):
        with dev().boot() as s:
            req = dev().mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["RSSI"], "-48")  # the 5 GHz AP, seen by the modem
            for h in ("ID", "FW-Version", "Model", "Battery-Voltage", "Width", "Height"):
                self.assertIn(h, req.headers)
            self.assertEqual((req.headers["Width"], req.headers["Height"]), ("1872", "1404"))
            self.assertEqual(req.headers["Model"], "x")
            self.assertNotIn("Panel-Rev", req.headers)  # FastEPD panels aren't read
            s.wait(state="deep_sleep", timeout_s=120)

    def test_1bit_png_is_rendered_exactly(self):
        expected = dev().mock.set_png("seven", digits("7"))
        dev().mock.display = {"image": "seven", "refresh_rate": 300}
        with dev().boot() as s:
            dev().mock.wait_for_request("/images/seven.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            result = s.compare_screen(expected, tolerance=64, max_ratio=0)
            self.assertTrue(result["match"], result)

    def test_4bit_png_shows_16_gray_levels(self):
        expected = dev().mock.set_png("ramp", ramp16, bits=4)
        dev().mock.display = {"image": "ramp", "refresh_rate": 300}
        with dev().boot() as s:
            dev().mock.wait_for_request("/images/ramp.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            result = s.compare_screen(expected, tolerance=40, max_ratio=0.001)
            self.assertTrue(result["match"], result)

    def test_sleeps_for_refresh_rate(self):
        dev().mock.display = {"image": "default", "refresh_rate": 600}
        with dev().boot() as s:
            st = s.wait(state="deep_sleep", timeout_s=120)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 600, delta=15)

    def test_restored_x_shows_the_same_screen_and_wakes_by_touch(self):
        seven = dev().mock.set_png("seven", digits("7"))
        dev().mock.display = {"image": "seven", "refresh_rate": 300}
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.trmnlsave"
            with dev().boot() as s:
                dev().mock.wait_for_request("/images/seven.png", timeout_s=120)
                s.wait(state="deep_sleep", timeout_s=120)
                s.dock(True)
                saved = s.save_point(path)
                screen = s.screenshot()
                boots = s.status()["boot_count"]
            self.assertTrue(saved["deep_sleep"])
            eight = dev().mock.set_png("eight", digits("8"))
            dev().mock.display = {"image": "eight", "refresh_rate": 300}
            dev().mock.requests.clear()
            with dev().restore(path) as s:
                st = s.wait(state="deep_sleep", timeout_s=30)["status"]
                self.assertEqual((st["boot_count"], st["docked"], st["charging"]), (boots, True, True))
                self.assertTrue(s.compare_screen(screen, tolerance=0, max_ratio=0)["match"])
                self.assertTrue(s.compare_screen(seven, tolerance=64, max_ratio=0)["match"])
                # Touch wake needs the IQS323 configuration and RTC state from before the save.
                s.touch("center", 150)
                req = dev().mock.wait_for_request("/api/display", timeout_s=120)
                self.assertEqual(req.headers["Update-Source"], "EXT0")
                self.assertEqual(req.headers["RSSI"], "-48")  # still on 5 GHz, through the modem
                self.assertEqual(req.headers["USB-Connected"], "true")
                dev().mock.wait_for_request("/images/eight.png", timeout_s=120)
                s.wait(state="deep_sleep", timeout_s=120)
                self.assertTrue(s.compare_screen(eight, tolerance=64, max_ratio=0)["match"])
                self.assertNotIn("/api/setup", [r.path for r in dev().mock.requests])


class TouchAndDock(ProvisionedCase):
    def test_center_tap_wakes_and_refreshes(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=120)
            n = len(dev().mock.requests)
            s.touch("center", 150)
            req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=120)
            self.assertEqual(req.headers["Update-Source"], "EXT0")
            s.wait(state="deep_sleep", timeout_s=120)

    def test_left_tap_shows_the_previous_image_offline(self):
        one = dev().mock.set_png("one", digits("1"))
        two = dev().mock.set_png("two", digits("2"))
        dev().mock.display_queue = [{"image": "one", "refresh_rate": 300}]
        dev().mock.display = {"image": "two", "refresh_rate": 300}
        with dev().boot() as s:
            dev().mock.wait_for_request("/images/one.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            s.wake()
            dev().mock.wait_for_request("/images/two.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=500)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])
            n, c = len(dev().mock.requests), s.status()["console_total"]
            s.touch("left", 150)
            s.wait(console=r"Playlist browse", since=c, timeout_s=60)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])
            self.assertEqual([r.path for r in dev().mock.requests[n:]], [])  # no network needed

    def test_dock_reports_usb_power_and_charging(self):
        with dev().boot() as s:
            req = dev().mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual((req.headers["USB-Connected"], req.headers["Battery-Charging"]), ("false", "0"))
            s.wait(state="deep_sleep", timeout_s=120)
            s.dock(True)
            self.assertTrue(s.status()["charging"])
            n = len(dev().mock.requests)
            s.wake()
            req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=120)
            self.assertEqual((req.headers["USB-Connected"], req.headers["Battery-Charging"]), ("true", "1"))
            s.wait(state="deep_sleep", timeout_s=120)

    def test_reports_the_fuel_gauge_next_to_its_voltage_estimate(self):
        # Production X builds estimate the charge from the voltage (BYPASS_BQ27427_SOC) and
        # send the gauge's own readings in the Gauge-* headers for comparison. At 4.06 V the
        # estimate sits on its 100 % plateau; the simulated gauge says 88 %.
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=120)
            s.set_battery(4060)
            n = len(dev().mock.requests)
            s.wake()
            h = dev().mock.wait_for_request("/api/display", after=n, timeout_s=120).headers
            s.wait(state="deep_sleep", timeout_s=120)
        self.assertAlmostEqual(float(h["Battery-Voltage"]), 4.06, delta=0.05)
        self.assertEqual(h["Battery-Count"], "1")
        # the voltage estimate, with a nominal 6000 mAh per cell and no state of health
        self.assertEqual((h["Percent-Charged"], h["Battery-Capacity"], h["Battery-Health"]),
                         ("100", "6000/6000", "-1"))
        # the gauge's own view
        self.assertEqual((h["Gauge-SOC"], h["Gauge-Capacity"], h["Gauge-Health"]), ("88", "5280/6000", "100"))
        self.assertEqual((h["Battery-Current"], h["Battery-Temp"]), ("-50", "25.00"))  # discharging


class PortalTimeout(unittest.TestCase):
    """Nobody joins the setup portal: after 15 minutes the X goes back to shipment mode."""

    def time_out(self, s, docked: bool = False):
        s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=30)
        s.dock(True)
        s.wait(portal=True, timeout_s=30)
        s.dock(docked)
        s.set_portal_client(False)  # so turbo can run to the timeout
        # Serial isn't running when enter_shipment_sleep() announces itself on a production
        # build, so go by the clock: the portal is gone once 15 minutes have passed.
        t0 = s.status()["sim_time_s"]
        deadline = time.time() + 150  # about 40 s alone, longer next to other tests
        while (st := s.status())["sim_time_s"] < t0 + 15 * 60 + 5:
            self.assertLess(time.time(), deadline, "the portal did not time out")
            time.sleep(0.5)
        self.assertIsNone(st["portal_url"])
        return st["console_total"]

    def test_unattended_portal_goes_back_to_shipment_mode(self):
        with shipped().boot() as s:
            # Still docked: "ready to ship" until it comes off USB power, then shipment mode.
            self.time_out(s, docked=True)
            g = s.status()["display_generation"]
            s.dock(False)  # checked every 2 s: then the shipping screen
            # (wall-clock limits allow for a machine busy with other simulators)
            deadline = time.time() + 90
            while s.status()["display_generation"] == g:
                self.assertLess(time.time(), deadline, "the shipping screen was not drawn")
                time.sleep(0.2)
            s.wait(display_idle=True, timeout_s=90)
            c = s.status()["console_total"]
            s.dock(True)  # the charger ends shipment mode
            s.wait(console=r"CHARGER DETECTED - Exiting shipment mode", since=c, timeout_s=90)

    @unittest.expectedFailure
    def test_shipment_mode_after_the_timeout_stays_asleep(self):
        # The setup screen (WIFI_CONNECT) ends with display_sleep(1000), which arms a 1 s
        # light-sleep timer that is never disarmed. enter_shipment_sleep() only adds the
        # charger's GPIO wakeup, so the device wakes about every 1.2 s ("Unexpected wakeup
        # cause: 4") instead of sleeping until it is docked, draining the battery in the box.
        with shipped().boot() as s:
            c = self.time_out(s)
            t0 = s.status()["sim_time_s"]
            while s.status()["sim_time_s"] < t0 + 10:
                time.sleep(0.2)
            wakes = [line for line in s.console(c) if "WAKEUP from light sleep" in line]
            self.assertEqual(wakes, [])


class Onboarding(unittest.TestCase):
    def test_portal_rescans_both_radios_and_joins_the_chosen_band(self):
        with MockTrmnl() as mock, shipped().boot() as s:
            s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=30)
            s.dock(True)
            s.wait(portal=True, timeout_s=30)
            s.dock(False)
            c = s.status()["console_total"]
            s.portal_request("/scan?force=1")
            s.wait(console=r"Modem re-scan found", since=c, timeout_s=30)
            deadline = time.time() + 30
            while (code := s.portal_request("/scan")[0]) != 200 and time.time() < deadline:
                time.sleep(0.5)
            code, body = s.portal_request("/scan")
            if code == 200:
                scan = json.loads(body)
                self.assertIn("mac_5ghz", scan)
            body = {"ssid": SSID_24, "pswd": "password", "server": mock.device_url, "band": "2.4GHz"}
            self.assertEqual(s.portal_request("/connect", body)[0], 200)
            req = mock.wait_for_request("/api/display", timeout_s=30)
            self.assertEqual(req.headers["RSSI"], "-54")  # the S3's own radio

    def test_onboarding_on_2_4_ghz_uses_the_s3_radio(self):
        with MockTrmnl() as mock, shipped().boot() as s:
            onboard(s, mock, SSID_24)
            req = mock.wait_for_request("/api/display")
            self.assertEqual(req.headers["RSSI"], "-54")  # the 2.4 GHz AP, seen by the S3

    def test_onboards_against_the_built_in_server(self):
        with shipped().boot() as s:
            s.mock.start()
            s.mock.add_image("seven", png_image(digits("7"), 1872, 1404), current=True)
            s.mock.display(refresh_rate=300)
            onboard(s, s.mock, SSID_24)
            self.assertEqual(s.mock.count("/images/seven.png"), 1)
            expected = expected_gray(digits("7"), 1872, 1404)
            self.assertTrue(s.compare_screen(expected, tolerance=64, max_ratio=0)["match"])
            self.assertTrue(s.compare_screen(s.mock.expected("seven"), tolerance=64, max_ratio=0)["match"])


# Firmware bug: when joining the network entered in the portal fails, startPortal() breaks
# out of its loop (WifiCaptive.cpp:196-199) and bl.cpp:1097-1107 shows WIFI_FAILED and
# deep-sleeps: the portal is gone and the phone is left waiting. After a failed modem (5 GHz)
# join it first retries the 5 GHz network on the S3 radio, which can't see it
# (WifiCaptive.cpp:209-225).
class FailedJoin(unittest.TestCase):
    """A join that fails leaves the portal up, so the password can be corrected."""

    def portal(self, s):
        s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
        s.dock(True)
        s.wait(portal=True, timeout_s=180)
        s.dock(False)
        s.set_networks([{"ssid": SSID_24, "password": "password", "rssi": -54, "channel": 6},
                        {"ssid": SSID_5, "password": "password", "rssi": -48, "channel": 36}])

    def join(self, s, ssid: str, band: str) -> int:
        c = s.status()["console_total"]
        body = {"ssid": ssid, "pswd": "wrong", "server": "http://x", "band": band}
        code, data = s.portal_request("/connect", body)
        self.assertEqual(code, 200, data)
        return c

    def assert_portal_stays(self, s):
        """For a minute of simulated time the device stays in setup mode (whether its AP
        stayed up or it brought the portal back), and the portal answers."""
        t0 = s.status()["sim_time_s"]
        deadline = time.time() + 120
        while (st := s.status())["sim_time_s"] < t0 + 60:
            self.assertNotEqual(st["state"], "deep_sleep", "went to sleep instead of keeping the portal")
            self.assertLess(time.time(), deadline, "the simulation stalled")
            time.sleep(0.5)
        s.wait(portal=True, timeout_s=30)
        self.assertEqual(s.portal_request("/")[0], 200)

    @unittest.expectedFailure  # WifiCaptive.cpp:196-199, bl.cpp:1097-1107 (above)
    def test_wrong_password_on_2_4_ghz_keeps_the_portal(self):
        with shipped().boot() as s:
            self.portal(s)
            c = self.join(s, SSID_24, "2.4GHz")
            s.wait(console=r"connect attempt failed", since=c, timeout_s=60)
            self.assert_portal_stays(s)

    @unittest.expectedFailure  # WifiCaptive.cpp:196-199 and 209-225, bl.cpp:1097-1107 (above)
    def test_wrong_password_on_5_ghz_keeps_the_portal(self):
        with shipped().boot() as s:
            self.portal(s)
            self.join(s, SSID_5, "5GHz")  # the modem's join fails silently on a production build
            self.assert_portal_stays(s)


if __name__ == "__main__":
    unittest.main()
