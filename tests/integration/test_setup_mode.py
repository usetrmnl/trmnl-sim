"""A factory-fresh device: WiFi setup screen, captive portal, onboarding."""

import unittest

from support import DEVICE, TEST_MAC, MockTrmnl, assert_golden, sim

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)



def regions(w: int, h: int) -> dict:
    """Where the screens' texts are (display_show_msg in the firmware's display.cpp): the
    version line at (40, 48) left of the top-right QR code, the portal's texts at fixed
    heights, the WiFi error 192 px above the bottom; centred. On the OG's 800x480 panel:
    body (0, 56, 800, 424), top_right (320, 0, 480, 56), ssid_line (140, 368, 520, 44),
    timed_out (260, 320, 280, 48), wifi_failed (100, 288, 600, 44). Other panels compare
    with their own goldens (golden/<env>/, see support.golden)."""
    return {
        "body": (0, 56, w, h - 56),
        "top_right": (320, 0, w - 320, 56),
        "ssid_line": ((w - 520) // 2, 368, 520, 44),
        "timed_out": ((w - 280) // 2, 320, 280, 48),
        "wifi_failed": ((w - 600) // 2, h - 192, 600, 44),
    }


REGIONS = regions(*DEVICE.size)


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
        assert_golden(self.sim, "setup_screen_body.png", region=REGIONS["body"])
        assert_golden(self.sim, "setup_screen_top_right.png", region=REGIONS["top_right"])

    def test_setup_screen_names_the_access_point(self):
        # Just the "Connect ... to TRMNL-XXXXXX" line, so version bumps don't break it.
        assert_golden(self.sim, "setup_ssid_line.png", region=REGIONS["ssid_line"])

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
            assert_golden(s, "portal_timed_out.png", region=REGIONS["timed_out"])
            if DEVICE.button:
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
            self.assertEqual(setup.headers["Model"], DEVICE.model)
            if DEVICE.panel_rev:
                self.assertEqual(setup.headers["Panel-Rev"], "0a0c1b2c")
            else:
                self.assertNotIn("Panel-Rev", setup.headers)
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
            assert_golden(s, "wifi_failed_message.png", region=REGIONS["wifi_failed"])

    def test_unknown_network_shows_wifi_error_and_sleeps(self):
        self._join_and_expect_wifi_error("No Such Network", "x")

    def test_wrong_password_shows_wifi_error_and_sleeps(self):
        self._join_and_expect_wifi_error("Neighbors WiFi", "not-the-password")

if __name__ == "__main__":
    unittest.main()
