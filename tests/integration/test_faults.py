"""Fault injection on the TRMNL OG: server errors, broken downloads, bad networks (DNS failure,
an access point without internet, slow and lossy links), power loss in the middle of flash
writes, and a stuck panel. The device must cope: sleep, retry later, and keep booting."""

import unittest

from support import BUILD, ProvisionedDevice, big_number, close_fixtures, fixture

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker


class NamedServerDevice(ProvisionedDevice):
    """Like ProvisionedDevice, but onboarded with the server's host name instead of its IP,
    so every request needs a DNS lookup (answered by the simulator via --dns)."""

    NAME = "named-server"
    HOST = "trmnl-mock.test"
    DEVICE_HOST = HOST
    SIM_ARGS = ("--offline", "--dns", f"{HOST}=10.0.2.2")


dev = fixture(ProvisionedDevice)
named = fixture(NamedServerDevice)


def tearDownModule():
    close_fixtures()


class FaultCase(unittest.TestCase):
    def setUp(self):
        dev().mock.requests.clear()
        dev().mock.display_queue.clear()
        dev().mock.clear_faults()
        self.expected = dev().mock.set_image("one", big_number("1"))
        dev().mock.display = {"image": "one", "refresh_rate": 300}

    def assert_shows_image_next_time(self, s, mock=None):
        """After the fault is gone, the next wake fetches and shows the image."""
        mock = mock or dev().mock
        mock.clear_faults()
        s.set_faults(net=None)
        n = len(mock.requests)
        s.wake()
        mock.wait_for_request("/images/one.bmp", after=n, timeout_s=90)
        s.wait(state="deep_sleep", timeout_s=90, settle_ms=300)
        self.assertTrue(s.compare_screen(self.expected, tolerance=64)["match"])


class ServerErrors(FaultCase):
    def test_http_500_from_api_display_is_retried_then_sleeps(self):
        dev().mock.set_fault("/api/display", status=500)
        with dev().boot() as s:
            st = s.wait(state="deep_sleep", timeout_s=120)["status"]
            self.assertEqual(dev().mock.count("/api/display"), 5, "the firmware retries 5 times")
            self.assertEqual(dev().mock.count("/api/log"), 1, "and reports the failure")
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 300, delta=15)
            self.assertFalse(s.compare_screen(self.expected, tolerance=64)["match"])
            self.assert_shows_image_next_time(s)

    def test_malformed_json_is_rejected(self):
        dev().mock.set_fault("/api/display", body='{"status": 0, "image_url": ')
        with dev().boot() as s:
            s.wait(console=r"JSON deserialization error", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertEqual(dev().mock.count("/images/one.bmp"), 0)
            self.assert_shows_image_next_time(s)


class BrokenDownloads(FaultCase):
    def test_truncated_image_is_not_shown(self):
        dev().mock.set_fault("/images/*", truncate=10_000)
        with dev().boot() as s:
            s.wait(console=r"incomplete download", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertFalse(s.compare_screen(self.expected, tolerance=64)["match"])
            self.assert_shows_image_next_time(s)

    def test_connection_reset_mid_download(self):
        with dev().boot(faults={"net": {"tcp_cut": {"after_bytes": 20_000}}}) as s:
            dev().mock.wait_for_request("/images/one.bmp", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertFalse(s.compare_screen(self.expected, tolerance=64)["match"])
            self.assert_shows_image_next_time(s)

    def test_stalled_download_times_out(self):
        with dev().boot(faults={"net": {"tcp_cut": {"after_bytes": 20_000, "stall": True}}}) as s:
            dev().mock.wait_for_request("/images/one.bmp", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=180)
            self.assertFalse(s.compare_screen(self.expected, tolerance=64)["match"])

    def test_slow_high_latency_link_still_works(self):
        with dev().boot(faults={"net": {"latency_ms": 250, "bandwidth_bps": 16_000}}) as s:
            s.wait(state="deep_sleep", timeout_s=180, settle_ms=300)
            self.assertTrue(s.compare_screen(self.expected, tolerance=64)["match"])

    def test_lossy_link_still_works(self):
        with dev().boot(faults={"net": {"loss": 0.05}}) as s:
            s.wait(state="deep_sleep", timeout_s=180, settle_ms=300)
            self.assertTrue(s.compare_screen(self.expected, tolerance=64)["match"])


class BadNetworks(FaultCase):
    def test_access_point_without_internet(self):
        with dev().boot(faults={"net": {"no_internet": True}}) as s:
            st = s.wait(wifi_connected=True, timeout_s=60)["status"]
            self.assertEqual(st["ip"], "10.0.2.15", "DHCP still works")
            s.wait(state="deep_sleep", timeout_s=240)
            self.assertEqual(dev().mock.requests, [])
            self.assert_shows_image_next_time(s)

    def test_dns_failure(self):
        named().mock.requests.clear()
        named().mock.display_queue.clear()
        named().mock.clear_faults()
        named().mock.set_image("one", big_number("1"))
        named().mock.display = {"image": "one", "refresh_rate": 300}
        for fault in ("servfail", "timeout"):
            with self.subTest(fault), named().boot(faults={"net": {"dns": fault}}) as s:
                s.wait(state="deep_sleep", timeout_s=240)
                self.assertEqual(named().mock.requests, [])
                self.assert_shows_image_next_time(s, named().mock)
                named().mock.requests.clear()


class PowerLoss(FaultCase):
    def test_power_loss_during_nvs_writes_then_boots(self):
        # A failing /api/display makes the firmware write NVS (its retry counter and the
        # log it stores for /api/log). Cut the power in the middle of those writes,
        # leaving a torn page, at a few different points.
        for nth in (1, 4, 9):
            with self.subTest(nth=nth):
                dev().mock.requests.clear()
                dev().mock.set_fault("/api/display", status=500)
                with dev().boot(faults={"power_loss": {"partition": "nvs", "op": "program", "nth": nth,
                                                     "cut": "torn"}}) as s:
                    s.wait(console=r"\[sim\] power lost: program", timeout_s=120)
                    dev().mock.clear_faults()
                    n = len(dev().mock.requests)
                    dev().mock.wait_for_request("/images/one.bmp", after=n, timeout_s=120)
                    st = s.wait(state="deep_sleep", timeout_s=120, settle_ms=300)["status"]
                    self.assertEqual(st["power_losses"], 1)
                    self.assertTrue(s.compare_screen(self.expected, tolerance=64)["match"])
                    self.assertEqual(dev().mock.count("/api/setup"), 0, "must not lose its registration")

    def test_power_loss_while_the_bootloader_writes_otadata(self):
        # The fresh flash has blank otadata, which the bootloader initialises on first boot.
        with dev().boot(faults={"power_loss": {"partition": "otadata", "cut": "torn"}}) as s:
            s.wait(console=r"\[sim\] power lost: erase", timeout_s=60)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=300)
            self.assertTrue(s.compare_screen(self.expected, tolerance=64)["match"])

    def test_power_loss_during_ota_keeps_the_old_firmware(self):
        firmware = (BUILD / "firmware.bin").read_bytes()
        url = dev().mock.set_file("/firmware.bin", "application/octet-stream", firmware)
        dev().mock.display_queue = [{"image": "one", "update_firmware": True, "firmware_url": url}]
        with dev().boot(faults={"power_loss": {"partition": "ota_1", "op": "program", "nth": 200,
                                             "cut": "torn"}}) as s:
            dev().mock.wait_for_request("/firmware.bin", timeout_s=120)
            c = s.wait_for_console(r"\[sim\] power lost: program #200", timeout_s=120)
            s.wait(console=r"Loaded app from partition at offset 0x10000", timeout_s=60)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertIn("app1", c)


class Peripherals(FaultCase):
    def test_panel_busy_stuck_does_not_hang_the_device(self):
        with dev().boot(faults={"panel_busy_stuck": True}) as s:
            dev().mock.wait_for_request("/images/one.bmp", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=180)


if __name__ == "__main__":
    unittest.main()
