"""A factory-fresh device: WiFi setup screen, captive portal, onboarding."""

import unittest

from support import DEVICE, TEST_MAC, MockTrmnl, assert_golden, sim

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)


# Firmware layout bug on panels smaller than 800x480: display_show_msg() puts the screens'
# texts at the OG's fixed coordinates (the version line at (40, 48) next to the QR code at
# width - 106, the portal texts at y 386 and 340) and centres lines wider than the panel.
SMALL_PANEL = ("display_show_msg() lays the screen out for 800x480 (display.cpp:2796-2813, 2559, 2225-2243): "
               "texts off the bottom, cut off at the sides, the version line under the QR code")
# ...and on the parallel (FastEPD) boards smaller than the TRMNL X: the X's Inter_18 font
# (display.cpp:2759-2760) and 2x logo, with the texts at X offsets from the bottom.
PARALLEL_SMALL_PANEL = ("display_show_msg() uses the TRMNL X's Inter_18 font and big logo on every parallel panel "
                        "(display.cpp:2759-2760, 2805, 2561, 2232): at 960x540 lines run off the sides and over the logo")
# Firmware bug on the ESP32-C5: WifiCaptive::getAPSSID() names the setup network after bytes 3-5
# of ESP.getEfuseMac(), but on chips with 802.15.4 esp_efuse_mac_get_default() returns the
# 8-byte EUI-64 OUI:FF:FE:NIC, so those bytes are FF FE and the NIC's first byte.
C5_AP_NAME = ("WifiCaptive.cpp:27-30 takes the AP name from bytes 3-5 of the C5's EUI-64 factory MAC (FF FE + one "
              "NIC byte): every gen-2 device's setup network is TRMNL-FFFExx instead of the MAC's last three bytes")

# Firmware bug on the XIAO ESP32-C3: bl_init() waits 2 s (bl.cpp:731-733) before it reads the
# button, so a wake press is over by then, and classify_button_presses() (button.cpp:66-71)
# then waits for another press with no timeout: the device stays awake until pressed again.
XIAO_C3_BUTTON = ("XIAO C3: after bl.cpp:731-733's 2 s delay the wake press is over, and button.cpp:66-71 waits "
                  "for a new press forever (no timeout): a button wake never reaches the server")
# Firmware (bb_epaper) bug on the Waveshare 3.97": EP397_800x480 starts the RAM Y counter at 0
# instead of 479, so every picture lands one row too high, its top row at the bottom (see
# test_byod_ssd.Waveshare397); its BMPs also hit the SSD16xx partial refresh bug.
WAVESHARE_ROW = ("bb_epaper EP397_800x480 init sets the RAM Y counter to 0 while counting down from 479: "
                 "every screen is one row too high (see test_byod_ssd.Waveshare397)")
# Firmware bug on the gen-2 BWRY: its env defines BOARD_TRMNL_GEN2 but not BOARD_TRMNL_4CLR, so
# images take the 1-bit two-plane path the BWRY panel misreads (see test_og_gen2.OgGen2Bwry).
GEN2_BWRY_IMAGES = ("trmnl_gen2_4clr lacks BOARD_TRMNL_4CLR, so display.cpp's 4-color image path isn't compiled: "
                    "images come out half drawn in the wrong inks (see test_og_gen2.OgGen2Bwry)")
KNOWN_FAILURES: dict[str, dict[str, str]] = {
    "CrowPanel42": {
        "FreshDevice.test_setup_screen": SMALL_PANEL,
        "FreshDevice.test_setup_screen_names_the_access_point": SMALL_PANEL,
        "PortalTimeout.test_unattended_portal_times_out_and_sleeps": SMALL_PANEL,
        "Onboarding.test_unknown_network_shows_wifi_error_and_sleeps": SMALL_PANEL,
        "Onboarding.test_wrong_password_shows_wifi_error_and_sleeps": SMALL_PANEL,
    },
    "TRMNL_X_PAPERS3": {
        "FreshDevice.test_setup_screen": PARALLEL_SMALL_PANEL,
        "FreshDevice.test_setup_screen_names_the_access_point": PARALLEL_SMALL_PANEL,
        "PortalTimeout.test_unattended_portal_times_out_and_sleeps": PARALLEL_SMALL_PANEL,
        "Onboarding.test_unknown_network_shows_wifi_error_and_sleeps": PARALLEL_SMALL_PANEL,
        "Onboarding.test_wrong_password_shows_wifi_error_and_sleeps": PARALLEL_SMALL_PANEL,
    },
    "trmnl_gen2": {
        "FreshDevice.test_setup_screen": C5_AP_NAME,
        "FreshDevice.test_setup_screen_names_the_access_point": C5_AP_NAME,
    },
    "trmnl_gen2_4clr": {
        "FreshDevice.test_setup_screen": C5_AP_NAME,
        "FreshDevice.test_setup_screen_names_the_access_point": C5_AP_NAME,
    },
    "seeed_xiao_esp32c3": {
        "PortalTimeout.test_unattended_portal_times_out_and_sleeps": XIAO_C3_BUTTON,
    },
    "WAVESHARE_397": {
        "FreshDevice.test_setup_screen": WAVESHARE_ROW,
        "FreshDevice.test_setup_screen_names_the_access_point": WAVESHARE_ROW,
        "PortalTimeout.test_unattended_portal_times_out_and_sleeps": WAVESHARE_ROW,
        "Onboarding.test_unknown_network_shows_wifi_error_and_sleeps": WAVESHARE_ROW,
        "Onboarding.test_wrong_password_shows_wifi_error_and_sleeps": WAVESHARE_ROW,
    },
}


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
            # (a slow panel is still showing the message when the chip sleeps)
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=120)["status"]
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
            s.wait(state="deep_sleep", display_idle=True, timeout_s=180)
            # "Can't establish WiFi connection. Will keep trying..." (the missing
            # apostrophe is a known firmware font bug; update the golden once fixed)
            assert_golden(s, "wifi_failed_message.png", region=REGIONS["wifi_failed"])

    def test_unknown_network_shows_wifi_error_and_sleeps(self):
        self._join_and_expect_wifi_error("No Such Network", "x")

    def test_wrong_password_shows_wifi_error_and_sleeps(self):
        self._join_and_expect_wifi_error("Neighbors WiFi", "not-the-password")

if __name__ == "__main__":
    unittest.main()
