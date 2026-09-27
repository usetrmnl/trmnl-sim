"""A provisioned device talking to a mock TRMNL server: images, sleep, wake sources, headers."""

import unittest

from support import BUILD, DEVICE, ProvisionedDevice, big_number, device_image, needs

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

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
        path, expected = device_image(dev.mock, "one", big_number("1"))
        dev.mock.display = {"image": "one", "refresh_rate": 300}
        with dev.boot() as s:
            dev.mock.wait_for_request(path, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            result = s.compare_screen(expected, tolerance=64, max_ratio=0)
            self.assertTrue(result["match"], result)

    def test_sleeps_for_refresh_rate(self):
        dev.mock.display = {"image": "default", "refresh_rate": 600}
        with dev.boot() as s:
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 600, delta=15)

    def test_timer_wake_fetches_next_image(self):
        _, expected = device_image(dev.mock, "two", big_number("2"))
        dev.mock.display = {"image": "two", "refresh_rate": 300}
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev.mock.requests)
            s.wake()
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
            self.assertEqual(req.headers["Update-Source"], "timer")
            s.wait(state="deep_sleep", timeout_s=90)
            self.assertTrue(s.compare_screen(expected, tolerance=64)["match"])

    def test_button_press_wakes_and_refreshes(self):
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev.mock.requests)
            s.press(150)
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
            self.assertEqual(req.headers["Update-Source"], DEVICE.press_source)
            s.wait(state="deep_sleep", timeout_s=90)

    def test_reports_battery_voltage(self):
        with dev.boot_asleep() as s:
            s.set_battery(3700)
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev.mock.requests)
            s.wake()
            req = dev.mock.wait_for_request("/api/display", after=n, timeout_s=90)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 3.70, delta=0.05)

    def test_reports_device_identity(self):
        with dev.boot() as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=90)
            for h in ("ID", "FW-Version", "Model", "RSSI", "Width", "Height"):
                self.assertIn(h, req.headers)
            self.assertEqual((req.headers["Width"], req.headers["Height"]), tuple(map(str, DEVICE.size)))
            self.assertEqual(req.headers["RSSI"], "-54")
            if DEVICE.panel_rev:
                # read from the UC8179 by bit-banging its REV command (the simulator's default)
                self.assertEqual(req.headers["Panel-Rev"], "0a0c1b2c")
            else:
                self.assertNotIn("Panel-Rev", req.headers)
            s.wait(state="deep_sleep", timeout_s=90)

    @needs("panel_rev")
    def test_reports_the_panel_revision_it_reads(self):
        with dev.boot(extra_args=("--panel-rev", "0x00c0ffee")) as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=90)
            self.assertEqual(req.headers["Panel-Rev"], "00c0ffee")
            s.wait(state="deep_sleep", timeout_s=90)

    @needs("panel_rev")
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

    def test_long_press_resets_wifi(self):
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.press(6000)
            s.wait(portal=True, timeout_s=120)

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
            s.wait_for_console(rf"Loaded app from partition at offset {DEVICE.ota_slot:#x}", timeout_s=300)
            cursor = len(dev.mock.requests)  # the new app can't have reached the network yet
            req = dev.mock.wait_for_request("/api/display", after=cursor, timeout_s=120)
            self.assertEqual(req.headers["FW-Version"], "1.8.16")
            s.wait(state="deep_sleep", timeout_s=120)


if __name__ == "__main__":
    unittest.main()
