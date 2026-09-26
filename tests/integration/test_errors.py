"""Error handling on the TRMNL OG: the error screens (and their weak-WiFi variants), quiet
retries on timer wakes, /api/setup failures during onboarding, and factory QA."""

import time
import unittest

from support import MockTrmnl, ProvisionedDevice, close_fixtures, fixture, sim

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

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
            dev().mock.wait_for_request("/images/default.bmp", after=n, timeout_s=90)
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


class FactoryQa(unittest.TestCase):
    @unittest.expectedFailure
    def test_qa_runs_when_the_qa_network_is_in_range(self):
        # startQA() calls pins_init() before display_init(); pins_init reads
        # pDevice->interrupt_pin while pDevice is still NULL (hw_config_init hasn't run), so
        # a fresh OG near a "TRMNL_QA" network crashes, and keeps crashing on every boot since
        # the test never gets marked as passed.
        nets = [{"ssid": "TRMNL_QA", "rssi": -40}, {"ssid": "TRMNL-Sim"}]
        with sim(erase=True, networks=nets, extra_args=("--offline",)) as s:
            deadline = time.time() + 240
            while time.time() < deadline:
                st = s.status()
                self.assertEqual(st["boot_count"], 1, "the device crashed")
                if any("QA Test Passed" in line for line in s.console(max(0, st["console_total"] - 50))):
                    break
                time.sleep(0.5)
            else:
                self.fail("QA never finished")
            s.press(200)  # "Long press the button to continue" (it takes a short one)
            s.wait(portal=True, timeout_s=120)


if __name__ == "__main__":
    unittest.main()
