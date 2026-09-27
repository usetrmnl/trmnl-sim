"""Fault injection on the device under test: server errors, broken downloads, bad networks
(DNS failure, an access point without internet, slow and lossy links), power loss in the
middle of flash writes, and a stuck panel. The device must cope: sleep, retry later, and keep
booting. The image it is served is the one its server would send (see support.device_image)."""

import unittest

from support import (BUILD, DEVICE, GEN2_NTP_HANG_BUG, ONE_BIT_PNG_PANEL_TYPE_BUG, SSD16XX_BMP_BUG,
                     ProvisionedDevice, boot_slot, close_fixtures, device_image, fixture, image_size, ota_slot_label,
                     panel_number, partition_table)

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker


class NamedServerDevice(ProvisionedDevice):
    """Like ProvisionedDevice, but onboarded with the server's host name instead of its IP,
    so every request needs a DNS lookup (answered by the simulator via --dns)."""

    NAME = "named-server"
    HOST = "trmnl-mock.test"
    DEVICE_HOST = HOST
    SIM_ARGS = ("--offline", "--dns", f"{HOST}=10.0.2.2")


# The tests that check the device shows the served image once the fault is gone.
SHOWS_THE_IMAGE = [
    "ServerErrors.test_http_500_from_api_display_is_retried_then_sleeps",
    "ServerErrors.test_malformed_json_is_rejected",
    "BrokenDownloads.test_truncated_image_is_not_shown",
    "BrokenDownloads.test_connection_reset_mid_download",
    "BrokenDownloads.test_slow_high_latency_link_still_works",
    "BrokenDownloads.test_lossy_link_still_works",
    "BadNetworks.test_access_point_without_internet",
    "BadNetworks.test_dns_failure",
    "PowerLoss.test_power_loss_during_nvs_writes_then_boots",
    "PowerLoss.test_power_loss_while_the_bootloader_writes_otadata",
]
NO_INTERNET = ["BadNetworks.test_access_point_without_internet", "BadNetworks.test_dns_failure"]

KNOWN_FAILURES = {
    "xteink_x4": dict.fromkeys(SHOWS_THE_IMAGE, SSD16XX_BMP_BUG),
    "CrowPanel42": dict.fromkeys(SHOWS_THE_IMAGE, ONE_BIT_PNG_PANEL_TYPE_BUG),
    "trmnl_gen2": dict.fromkeys(NO_INTERNET, GEN2_NTP_HANG_BUG),
    "trmnl_gen2_4clr": dict.fromkeys(NO_INTERNET, GEN2_NTP_HANG_BUG),
}

dev = fixture(ProvisionedDevice)
named = fixture(NamedServerDevice)


def tearDownModule():
    close_fixtures()


def one_with_noise():
    """The test image: a big "1" over a strip of noise along the bottom, so that even as a PNG
    (which compresses the rest to nothing) the file is a few KB: download faults cut it part
    of the way through, after the /api/display response."""
    w, h = DEVICE.size
    number, strip = panel_number("1"), h - h // 6

    def pixel(x, y):
        if y >= strip:
            n = (x * 374761393 + y * 668265263) & 0xFFFFFFFF
            n = (n ^ n >> 13) * 1274126177 & 0xFFFFFFFF
            return (n ^ n >> 16) & 1 == 1
        return number(x, y)

    return pixel


def serve_one(mock) -> tuple[str, bytes]:
    """Serve the test image ("1") as the current screen; returns its path and the screenshot
    it should give."""
    path, expected = device_image(mock, "one", one_with_noise())
    mock.display = {"image": "one", "refresh_rate": 300}
    return path, expected


class FaultCase(unittest.TestCase):
    def setUp(self):
        dev().mock.requests.clear()
        dev().mock.display_queue.clear()
        dev().mock.clear_faults()
        self.path, self.expected = serve_one(dev().mock)
        # the image file's size: download faults cut it part of the way through (the OG's
        # 48 KB BMP at 10 and 20 KB)
        self.size = image_size(dev().mock, self.path)

    def assert_shows_image_next_time(self, s, mock=None):
        """After the fault is gone, the next wake fetches and shows the image."""
        mock = mock or dev().mock
        mock.clear_faults()
        s.set_faults(net=None)
        n = len(mock.requests)
        s.wake()
        mock.wait_for_request(self.path, after=n, timeout_s=90)
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
            self.assertEqual(dev().mock.count(self.path), 0)
            self.assert_shows_image_next_time(s)


class BrokenDownloads(FaultCase):
    def test_truncated_image_is_not_shown(self):
        dev().mock.set_fault("/images/*", truncate=min(10_000, self.size // 4))
        with dev().boot() as s:
            s.wait(console=r"incomplete download", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertFalse(s.compare_screen(self.expected, tolerance=64)["match"])
            self.assert_shows_image_next_time(s)

    def test_connection_reset_mid_download(self):
        with dev().boot(faults={"net": {"tcp_cut": {"after_bytes": min(20_000, self.size // 2)}}}) as s:
            dev().mock.wait_for_request(self.path, timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertFalse(s.compare_screen(self.expected, tolerance=64)["match"])
            self.assert_shows_image_next_time(s)

    def test_stalled_download_times_out(self):
        with dev().boot(faults={"net": {"tcp_cut": {"after_bytes": min(20_000, self.size // 2), "stall": True}}}) as s:
            dev().mock.wait_for_request(self.path, timeout_s=120)
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
            s.wait(state="deep_sleep", timeout_s=90)
            self.assertEqual(dev().mock.requests, [])
            self.assert_shows_image_next_time(s)

    def test_dns_failure(self):
        named().mock.display_queue.clear()
        serve_one(named().mock)
        for fault in ("servfail", "timeout"):
            named().mock.requests.clear()
            named().mock.clear_faults()
            with self.subTest(fault), named().boot(faults={"net": {"dns": fault}}) as s:
                s.wait(state="deep_sleep", timeout_s=90)
                self.assertEqual(named().mock.requests, [])
                self.assert_shows_image_next_time(s, named().mock)


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
                    dev().mock.wait_for_request(self.path, after=n, timeout_s=120)
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
            flash = s.flash.read_bytes()
            self.assertIn(f"partition {ota_slot_label(flash, 1)}", c)
            if s.wait(console=r"2nd stage bootloader|BL init success", timeout_s=60)["line"]["text"].count("bootloader"):
                # the bootloader logs (not in the ESP32-S3's Arduino 2 builds): from the old slot
                old = next(p for p in partition_table(flash) if p["label"] == ota_slot_label(flash, 0))
                s.wait(console=rf"Loaded app from partition at offset {old['offset']:#x}\b", timeout_s=60)
            s.wait(state="deep_sleep", timeout_s=120)
        self.assertEqual(boot_slot(s.flash.read_bytes()), ota_slot_label(flash, 0), "otadata still boots the old slot")


class Peripherals(FaultCase):
    def test_panel_busy_stuck_does_not_hang_the_device(self):
        with dev().boot(faults={"panel_busy_stuck": True}) as s:
            dev().mock.wait_for_request(self.path, timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=180)


if __name__ == "__main__":
    unittest.main()
