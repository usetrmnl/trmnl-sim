"""A provisioned device talking to a mock TRMNL server: images, sleep, wake sources, headers."""

import unittest

from support import BUILD, DEVICE, ProvisionedDevice, device_image, device_number, needs

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

# Firmware bug on the SSD16xx boards: display_show_image() (display.cpp:1947) sends a 1-bit
# BMP with bbep.writePlane(), which without a second plane writes only the new-image RAM
# (0x24), and asks for a partial refresh, which drives only the pixels differing from the
# "old" RAM (0x26); that holds an earlier picture (or its inverse after a fast refresh), so
# the BMP comes out as a mix of the two. See test_byod_ssd.SsdBoard.test_bmp_after_a_fast_refresh.
SSD_BMP = "SSD16xx: a 1-bit BMP is refreshed partially against a stale old-image RAM (display.cpp:1947)"
CROWPANEL_PNG = ("CrowPanel: png_to_epd() calls bbep.setPanelType(dpList[...].OneBit) (display.cpp:1764) with the "
                 "bb_epaper product number the panel was begun with, selecting a 2.9\" 128x296 panel: 1-bit PNGs "
                 "never show (see test_byod_ssd.CrowPanel42)")
X4_BATTERY = ("device_list[] (display.cpp:51) gives the X4 batt_pin 0xff though its divider is on GPIO0 "
              "(config.h:117), so it always reports 0 V")

KNOWN_FAILURES = {
    "xteink_x4": {
        "RefreshCycle.test_image_is_rendered_exactly": SSD_BMP,
        "RefreshCycle.test_timer_wake_fetches_next_image": SSD_BMP,
        "RefreshCycle.test_reports_battery_voltage": X4_BATTERY,
    },
    "CrowPanel42": {
        "RefreshCycle.test_image_is_rendered_exactly": CROWPANEL_PNG,
        "RefreshCycle.test_timer_wake_fetches_next_image": CROWPANEL_PNG,
    },
}

dev: ProvisionedDevice


def setUpModule():
    global dev
    dev = ProvisionedDevice()


def tearDownModule():
    dev.close()


class RefreshCycle(unittest.TestCase):
    def setUp(self):
        dev.mock.requests.clear()
        dev.mock.display_queue.clear()
        dev.mock.display = {"image": "default", "refresh_rate": 300}

    def test_image_is_rendered_exactly(self):
        path, expected = device_image(dev.mock, "one", device_number("1"))
        dev.mock.display = {"image": "one", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request(path, timeout_s=90)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            result = s.compare_screen(expected, tolerance=64, max_ratio=0)
            self.assertTrue(result["match"], result)

    def test_sleeps_for_refresh_rate(self):
        dev.mock.display = {"image": "default", "refresh_rate": 600}
        with dev.boot() as s:
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 600, delta=15)

    def test_timer_wake_fetches_next_image(self):
        _, expected = device_image(dev.mock, "two", device_number("2"))
        dev.mock.display = {"image": "two", "refresh_rate": 300}
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev.mock.requests)
            s.wake()
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
            self.assertEqual(req.headers["Update-Source"], "timer")
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            self.assertTrue(s.compare_screen(expected, tolerance=64)["match"])

    @needs("button")
    def test_button_press_wakes_and_refreshes(self):
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev.mock.requests)
            s.press(150)
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
            self.assertEqual(req.headers["Update-Source"], DEVICE.button_source)
            s.wait(state="deep_sleep", timeout_s=90)

    def test_reports_battery_voltage(self):
        with dev.boot_asleep() as s:
            s.set_battery(3700)
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev.mock.requests)
            s.wake()
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
            # boards that can't measure it report a fixed value (see Device.battery_tracks)
            expected = 3.70 if DEVICE.battery_tracks else DEVICE.battery_v
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), expected, delta=0.05)

    def test_reports_device_identity(self):
        with dev.boot() as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=90)
            for h in ("ID", "FW-Version", "Model", "RSSI", "Width", "Height"):
                self.assertIn(h, req.headers)
            self.assertEqual(req.headers["Model"], DEVICE.model)
            self.assertEqual((int(req.headers["Width"]), int(req.headers["Height"])), DEVICE.size)
            self.assertEqual(req.headers["RSSI"], "-54")
            if DEVICE.panel_rev:
                # read from the UC8179 by bit-banging its REV command (the simulator's default)
                self.assertEqual(req.headers["Panel-Rev"], "0a0c1b2c")
            else:
                self.assertNotIn("Panel-Rev", req.headers)
            s.wait(state="deep_sleep", timeout_s=90)

    def test_reports_the_panel_revision_it_reads(self):
        with dev.boot(extra_args=("--panel-rev", "0x00c0ffee")) as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=90)
            if DEVICE.panel_rev:
                self.assertEqual(req.headers["Panel-Rev"], "00c0ffee")
            else:  # the firmware only asks 7.5" UC8179 panels whose pins it knows
                self.assertNotIn("Panel-Rev", req.headers)
            s.wait(state="deep_sleep", timeout_s=90)

    def test_omits_the_panel_revision_when_it_reads_zero(self):
        with dev.boot(extra_args=("--panel-rev", "0")) as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=90)
            self.assertNotIn("Panel-Rev", req.headers)
            s.wait(state="deep_sleep", timeout_s=90)

    def test_credentials_survive_power_cycle(self):
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.power_cycle()
            n = len(dev.mock.requests)
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
            self.assertEqual(req.headers["Update-Source"], "powercycle")
            self.assertEqual(dev.mock.count("/api/setup"), 0, "should not re-register")

    @needs("button")
    def test_long_press_resets_wifi(self):
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.press(6000)
            s.wait(portal=True, timeout_s=120)

    @needs("button")
    def test_portal_soft_reset_forgets_the_device(self):
        # A long press only forgets WiFi; the portal's Soft Reset also clears the API key,
        # so the next onboarding has to register with /api/setup again.
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.press(6000)
            boots = s.wait(portal=True, timeout_s=120)["status"]["boot_count"]
            try:
                s.portal_request("/soft-reset", retry_s=0)
            except (ConnectionError, OSError):
                pass  # the device may restart before it answers
            s.wait(min_boots=boots + 1, portal=True, timeout_s=120)
            s.portal_connect("TRMNL-Sim", "password", server=dev.mock.device_url)
            dev.mock.wait_for_request("/api/setup", timeout_s=120)
            dev.mock.wait_for_request("/api/display", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)

    def test_wifi_out_of_range_keeps_running(self):
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.set_wifi(False)
            n = len(dev.mock.requests)
            s.wake()
            # The device must give up gracefully and sleep again without reaching the server.
            s.wait(state="deep_sleep", timeout_s=240)
            self.assertEqual(len(dev.mock.requests), n)


def ota_1_offset(build) -> int:
    """Where the build's partition table puts the second app slot (ota_1)."""
    table = (build / "partitions.bin").read_bytes()
    for i in range(0, len(table), 32):
        e = table[i:i + 32]
        if e[:2] != b"\xaa\x50":
            break
        if (e[2], e[3]) == (0, 0x11):
            return int.from_bytes(e[4:8], "little")
    raise AssertionError(f"{build}/partitions.bin has no ota_1 partition")


class FirmwareUpdate(unittest.TestCase):
    def test_ota_update_installs_and_boots_other_slot(self):
        firmware = (BUILD / "firmware.bin").read_bytes()
        url = dev.mock.set_file("/firmware.bin", "application/octet-stream", firmware)
        dev.mock.requests.clear()
        dev.mock.display_queue = [{"image": "default", "update_firmware": True, "firmware_url": url}]
        dev.mock.display = {"image": "default", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request("/firmware.bin", timeout_s=120)
            # The bootloader picks the freshly written slot after the restart.
            s.wait_for_console(rf"booting the app in the other slot, at {ota_1_offset(BUILD):#x}", timeout_s=300)
            cursor = len(dev.mock.requests)  # the new app can't have reached the network yet
            req = dev.mock.wait_for_request("/api/display", after=cursor, timeout_s=120)
            self.assertEqual(req.headers["FW-Version"], "1.8.16")
            s.wait(state="deep_sleep", timeout_s=120)


if __name__ == "__main__":
    unittest.main()
