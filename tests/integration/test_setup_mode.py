"""A factory-fresh device: WiFi setup screen, captive portal, onboarding."""

import unittest

from support import GOLDEN, TEST_MAC, MockTrmnl, sim

ENV = "trmnl"  # the PlatformIO environment these tests run (bin/spec trmnl)


class FreshDevice(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.sim = sim(erase=True, extra_args=("--offline",))
        cls.sim.wait(portal=True, timeout_s=90)
        cls.sim.wait(display_idle=True, min_refreshes=1, settle_ms=500, timeout_s=60)

    @classmethod
    def tearDownClass(cls):
        cls.sim.close()

    def test_setup_screen(self):
        # All but the "TRMNL firmware <version> (<git hash>)" line, which changes every commit.
        self.sim.assert_screen(GOLDEN / "setup_screen_body.png", region=(0, 56, 800, 424))
        self.sim.assert_screen(GOLDEN / "setup_screen_top_right.png", region=(320, 0, 480, 56))

    def test_setup_screen_names_the_access_point(self):
        # Just the "Connect ... to TRMNL-XXXXXX" line, so version bumps don't break it.
        self.sim.assert_screen(GOLDEN / "setup_ssid_line.png", region=(140, 368, 520, 44))

    def test_portal_lists_simulated_networks(self):
        scan = self.sim.portal_scan()
        self.assertIn("TRMNL-Sim", [n["name"] for n in scan["networks"]])
        self.assertEqual(scan["mac"], TEST_MAC)

    def test_status_reports_setup_mode(self):
        st = self.sim.status()
        self.assertEqual(st["state"], "running")
        self.assertFalse(st["wifi_connected"])
        self.assertIsNotNone(st["portal_url"])


class PortalTimeout(unittest.TestCase):
    def test_unattended_portal_times_out_and_sleeps(self):
        with sim(erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            s.set_portal_client(False)  # nobody joins, so turbo can run to the timeout
            st = s.wait(state="deep_sleep", timeout_s=40)["status"]
            self.assertGreaterEqual(st["sim_time_s"], 15 * 60)
            # "Wifi Captive Portal timed out" / "Press button to try again"
            s.assert_screen(GOLDEN / "portal_timed_out.png", region=(260, 320, 280, 48))
            s.set_portal_client(True)
            s.press(200)
            s.wait(portal=True, timeout_s=60)


class Onboarding(unittest.TestCase):
    def test_onboarding_registers_with_server(self):
        with MockTrmnl() as mock, sim(erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "any-password", server=mock.device_url)
            s.wait(wifi_connected=True, timeout_s=60)
            setup = mock.wait_for_request("/api/setup", timeout_s=60)
            self.assertEqual(setup.headers["ID"], TEST_MAC)
            self.assertEqual(setup.headers["Model"], "og")
            self.assertEqual(setup.headers["Panel-Rev"], "0a0c1b2c")
            display = mock.wait_for_request("/api/display", timeout_s=60)
            self.assertEqual(display.headers["Access-Token"], mock.api_key)
            self.assertEqual(display.headers["Update-Source"], "powercycle")
            s.wait(state="deep_sleep", timeout_s=90)

    def _join_and_expect_wifi_error(self, ssid, password):
        with sim(erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            s.portal_connect(ssid, password)
            s.wait_for_console(f'connecting to "{ssid}"', timeout_s=60)
            s.wait(portal=False, timeout_s=60)  # the setup AP goes away while joining
            s.wait(state="deep_sleep", timeout_s=180)
            # "Can't establish WiFi connection. Will keep trying..." (the missing
            # apostrophe is a known firmware font bug; update the golden once fixed)
            s.assert_screen(GOLDEN / "wifi_failed_message.png", region=(100, 288, 600, 44))

    def test_unknown_network_shows_wifi_error_and_sleeps(self):
        self._join_and_expect_wifi_error("No Such Network", "x")

    def test_wrong_password_shows_wifi_error_and_sleeps(self):
        self._join_and_expect_wifi_error("Neighbors WiFi", "not-the-password")

if __name__ == "__main__":
    unittest.main()
