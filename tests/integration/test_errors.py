"""Error handling on the device under test: the error screens (and their weak-WiFi variants),
quiet retries on timer wakes, /api/setup failures during onboarding, and factory QA."""

import json
import time
import unittest

from support import (ONE_BIT_PNG_PANEL_TYPE_BUG, MockTrmnl, ProvisionedDevice, assert_text, close_fixtures, fixture, needs,
                     sim, skip_if)

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

# Firmware bugs (see KNOWN_FAILURES in run.py)
MESSAGE_BELOW_PANEL = (
    "firmware: display_show_msg() draws the error text at fixed rows from 340 down "
    "(src/display.cpp:2319 for API_UNABLE_TO_CONNECT), below the bottom of a 300-row panel: "
    "the error screen is the bare logo")
QA_RESULTS_BELOW_PANEL = (
    "firmware: display_show_msg_qa() draws the voltages, temperatures and verdict at rows "
    "340/370/400 (src/display.cpp:2651, 2655, 2663), below the bottom of a 300-row panel")
QA_1BPP_BUFFER_ON_COLOR_PANEL = (
    "firmware: display_show_msg_qa() copies startQA()'s white 1-bit buffer (src/qa.cpp:298-299) "
    "into the frame buffer with a 1-bpp size (src/display.cpp:2596, 2622), but a color panel's "
    "buffer has 2 (4-color) or 4 (Spectra 6) bits per pixel: the 0xFF bytes cover the top half "
    "(in red) or quarter of the screen, and the rest keeps the QA start screen (logo, "
    "'Starting QA test') under the results")
QA_SECOND_DISPLAY_INIT = (
    "firmware: startQA() calls display_init() again before showing the results "
    "(src/qa.cpp:353, after qa.cpp:301); on FastEPD boards display_init() calls "
    "bbep.initPanel() (src/display.cpp:295), whose second esp_lcd_new_i80_bus() fails "
    "(ESP_ERR_NOT_FOUND: the S3's one i80 bus is taken) and ESP_ERROR_CHECK aborts: the device "
    "reboots into QA again")
QA_PANEL_REV_BREAKS_SPI = (
    "firmware: startQA() calls display_init() again before showing the results "
    "(src/qa.cpp:353); display_init() reads the panel ID by bit-banging the SPI pins "
    "(get_panel_rev(), src/display.cpp:197), and with Arduino 3 the peripheral manager then "
    "detaches the pins from the SPI bus and stops it, while SPIClass::begin() returns early "
    "(its bus is still set): nothing reaches the panel and the QA results never show")

QA_SCREENS = ["FactoryQa.test_qa_passes_and_the_button_continues_to_setup",
              "FactoryQa.test_qa_fails_when_the_chip_heats_up"]

KNOWN_FAILURES = {
    "CrowPanel42": {
        "ErrorScreens.test_image_url_that_can_t_be_fetched": MESSAGE_BELOW_PANEL,
        "Retries.test_timer_wakes_retry_quietly_then_show_the_error": MESSAGE_BELOW_PANEL
        + "; the screen before it is the logo too (" + ONE_BIT_PNG_PANEL_TYPE_BUG + ")",
        **dict.fromkeys(QA_SCREENS, QA_RESULTS_BELOW_PANEL),
    },
    "trmnl_4clr": dict.fromkeys(QA_SCREENS, QA_1BPP_BUFFER_ON_COLOR_PANEL),
    "seeed_reTerminal_E1002": dict.fromkeys(QA_SCREENS, QA_1BPP_BUFFER_ON_COLOR_PANEL),
    "trmnl_gen2": dict.fromkeys(QA_SCREENS, QA_PANEL_REV_BREAKS_SPI),
    "TRMNL_X_PAPERS3": {
        "FactoryQa.test_qa_fails_when_the_chip_heats_up": QA_SECOND_DISPLAY_INIT,
    },
}

dev = fixture(ProvisionedDevice)

# WIFI_CONNECTION_RSSI is -100: at or below it the device blames the WiFi signal.
WEAK = [{"ssid": "TRMNL-Sim", "rssi": -100}]


def tearDownModule():
    close_fixtures()


def settle(s, timeout_s: float = 120) -> dict:
    """Wait until the device sleeps or restarts (a crash); returns its status."""
    deadline = time.time() + timeout_s
    while True:
        st = s.status()
        if st["boot_count"] > 1 or st["state"] == "halted":
            return st
        if st["state"] == "deep_sleep":
            return s.wait(state="deep_sleep", timeout_s=30, settle_ms=200)["status"]
        if time.time() > deadline:
            raise TimeoutError(f"device neither slept nor restarted: {st}")
        time.sleep(0.2)


class Case(unittest.TestCase):
    def setUp(self):
        m = dev().mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def boot_with_error(self, **kw):
        """Power on (an error is shown right away) and wait for the error screen."""
        s = dev().boot(**kw)
        try:
            st = settle(s)
            self.assertEqual((st["boot_count"], st["state"]), (1, "deep_sleep"))
        except BaseException:
            s.close()
            raise
        return s


class ErrorScreens(Case):
    def test_api_unreachable(self):
        dev().mock.set_fault("/api/display", close=True)
        with self.boot_with_error() as s:
            self.assertGreater(s.status()["display_refreshes"], 1)

    def test_api_unreachable_on_weak_wifi(self):
        dev().mock.set_fault("/api/display", close=True)
        with self.boot_with_error(networks=WEAK):
            pass

    def test_image_cut_short_on_weak_wifi(self):
        dev().mock.set_fault("/images/*", truncate=1000)
        with self.boot_with_error(networks=WEAK):
            pass

    def test_image_host_unreachable(self):
        dev().mock.set_fault("/images/*", close=True)
        with self.boot_with_error():
            pass

    def test_image_url_that_can_t_be_fetched(self):
        # HTTPClient refuses the URL, so the request never starts: HTTPS_UNABLE_TO_CONNECT.
        # The screen blames the API although it answered (only HTTP errors from the image
        # host get the "image download failed" screen).
        dev().mock.display = {"image_url": "ftp://10.0.2.2/image.bmp", "filename": "plugin-bbbbbb-1",
                              "refresh_rate": 300}
        with self.boot_with_error() as s:
            # "WiFi connected, unable connect to API." and how to retry (on the 960x540 parallel
            # panels the logo, centred, runs into the text's fixed rows)
            assert_text(s, "api_unable_to_connect.png", (200, 320, 400, 64))
        log = dev().mock.wait_for_request("/api/log", timeout_s=10)
        self.assertIn("HTTPS_UNABLE_TO_CONNECT - Unable to create WiFiClient", log.body.decode())

    def test_image_too_large(self):
        dev().mock.set_file("/huge.bmp", "image/bmp", bytes(100_000))
        dev().mock.display = {"image_url": dev().mock.device_url + "/huge.bmp", "filename": "plugin-aaaaaa-1",
                              "refresh_rate": 300}
        with self.boot_with_error():
            pass


class Retries(Case):
    def test_timer_wakes_retry_quietly_then_show_the_error(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            screen = s.screenshot()
            dev().mock.set_fault("/api/display", status=500)
            for attempt in range(5):
                n = len(dev().mock.requests)
                s.wake()
                dev().mock.wait_for_request("/api/display", after=n, timeout_s=90)
                s.wait(state="deep_sleep", timeout_s=180, settle_ms=200)
                quiet = s.compare_screen(screen, tolerance=0, max_ratio=0)["match"]
                if not quiet:
                    break
            self.assertFalse(quiet, "the error was never shown")
            self.assertGreaterEqual(attempt, 2, "retried quietly first")
            dev().mock.clear_faults()
            n = len(dev().mock.requests)
            s.wake()
            dev().mock.wait_for_request(dev().mock.image_path("default"), after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)


class SetupErrors(unittest.TestCase):
    """Onboarding against a server whose /api/setup misbehaves."""

    def onboard(self, mock: MockTrmnl, **kw):
        s = sim(erase=True, extra_args=("--offline",), **kw)
        try:
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
            mock.wait_for_request("/api/setup", timeout_s=120)
        except BaseException:
            s.close()
            raise
        return s

    def test_unregistered_mac_shows_the_signup_message(self):
        with MockTrmnl() as mock:
            mock.setup = None  # 404 "MAC Address not registered"
            with self.onboard(mock) as s:
                st = settle(s)
                self.assertEqual((st["boot_count"], st["state"]), (1, "deep_sleep"))
                self.assertEqual(mock.count("/api/display"), 0)

    def test_long_unregistered_message_is_wrapped(self):
        message = ("Your device " + "is not yet registered with any account, " * 3 +
                   "visit https://usetrmnl.com/signup/with-a-very-long-link-that-cannot-be-wrapped-anywhere-at-all "
                   "and enter Device ID SIMTST")
        with MockTrmnl() as mock:
            mock.setup = None
            mock.set_fault("/api/setup", body=json.dumps({"status": 404, "api_key": None, "friendly_id": None,
                                                          "image_url": None, "message": message}))
            with self.onboard(mock) as s:
                st = settle(s, timeout_s=30)
                self.assertEqual((st["boot_count"], st["state"]), (1, "deep_sleep"))

    def test_setup_server_error(self):
        with MockTrmnl() as mock:
            mock.set_fault("/api/setup", status=500)
            with self.onboard(mock) as s:
                st = settle(s)
                self.assertEqual(st["boot_count"], 1)

    def test_setup_server_error_on_weak_wifi(self):
        with MockTrmnl() as mock:
            mock.set_fault("/api/setup", status=500)
            with self.onboard(mock, networks=WEAK) as s:
                st = settle(s)
                self.assertEqual(st["boot_count"], 1)

    def test_setup_malformed_json(self):
        with MockTrmnl() as mock:
            mock.set_fault("/api/setup", body='{"status": 200, "api_key": ')
            with self.onboard(mock) as s:
                st = settle(s)
                self.assertEqual(st["boot_count"], 1)

    def test_setup_logo_missing(self):
        with MockTrmnl() as mock:
            mock.set_fault("/images/*", status=404)
            with self.onboard(mock) as s:
                mock.wait_for_request("/images/default.bmp", timeout_s=120)
                st = settle(s)
                self.assertEqual(st["boot_count"], 1)

    def test_setup_logo_not_an_image(self):
        with MockTrmnl() as mock:
            mock.set_fault("/images/*", body=b"not an image at all")
            with self.onboard(mock) as s:
                mock.wait_for_request("/images/default.bmp", timeout_s=120)
                st = settle(s)
                self.assertEqual(st["boot_count"], 1)

    def test_setup_logo_host_unreachable(self):
        with MockTrmnl() as mock:
            mock.set_fault("/images/*", close=True)
            with self.onboard(mock) as s:
                mock.wait_for_request("/images/default.bmp", timeout_s=120)
                st = settle(s)
                self.assertEqual(st["boot_count"], 1)


@skip_if("shipment", why="its factory flow flashes the modem and ships; it runs no QA test (main.cpp)")
class FactoryQa(unittest.TestCase):
    """A fresh device near a "TRMNL_QA" network runs the factory test: 7 s of CPU and radio
    load, comparing the chip temperature (and battery voltage) before and after. Every build
    but the TRMNL X's has it (setup() in src/main.cpp calls startQA() until it passed once;
    the X has its own factory flow, see test_trmnl_x)."""

    NETS = [{"ssid": "TRMNL_QA", "rssi": -40}, {"ssid": "TRMNL-Sim"}]

    def start_qa(self, **kw):
        s = sim(erase=True, networks=self.NETS, extra_args=("--offline",), **kw)
        try:
            s.wait(console=r"Stress test started", timeout_s=30)
        except BaseException:
            s.close()
            raise
        return s

    def finish_qa(self, s):
        s.wait(console=r"QA Test Passed", timeout_s=30)
        s.wait(display_idle=True, timeout_s=30)
        self.assertEqual(s.status()["boot_count"], 1, "the device crashed")

    @needs("button")
    def test_qa_passes_and_the_button_continues_to_setup(self):
        with self.start_qa() as s:
            self.finish_qa(s)
            assert_text(s, "qa_pass.png", (300, 200, 200, 70))
            # "Initial temperature: 25.0000 C, Final temperature: 25.0000 C  Diff: 0.0000 C".
            # (The voltage line above it isn't compared: measureVoltageAverage() reads GPIO 3 on
            # every board, and on the OG scales the raw ADC value as if 4095 were 3.3 V (5.41 V
            # for a 4.1 V battery; at 11 dB the C3 tops out at 2.5 V).)
            assert_text(s, "qa_temperatures.png", (60, 352, 680, 26))
            s.press(200)  # "press button to clear screen" (a short press)
            s.wait(console=r"painting screen white", timeout_s=30)
            s.wait(portal=True, timeout_s=90)
            s.power_cycle()  # passed: not run again
            s.wait(portal=True, timeout_s=90)
            self.assertEqual(sum("Stress test started" in line for line in s.console(0)), 1)

    def test_qa_fails_when_the_chip_heats_up(self):
        with self.start_qa() as s:
            s.set_faults(chip_temp_c=30)  # 5 °C warmer after the load: 3 °C is the limit
            self.finish_qa(s)
            assert_text(s, "qa_fail.png", (300, 200, 200, 70))
            # "... Final temperature: 30.0000 C  Diff: 5.0000 C" and "QA failed, please use
            # another board and put in failure pile for investigation"
            assert_text(s, "qa_fail_details.png", (40, 352, 720, 58))

    @needs("button")
    def test_button_stops_qa(self):
        with self.start_qa() as s:
            s.press(200)
            s.wait(console=r"QA test stopped by user", timeout_s=30)
            s.wait(portal=True, timeout_s=90)  # carries on as a normal first boot


if __name__ == "__main__":
    unittest.main()
