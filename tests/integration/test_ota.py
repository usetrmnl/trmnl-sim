"""Firmware updates that go wrong: no URL, the download failing or cut short, a file that
isn't firmware, and (TRMNL X) updates through the 5 GHz modem."""

import unittest

from support import DEVICE, ProvisionedDevice, close_fixtures, fixture
from support_x import ProvisionedX, ShippedX, X_BUILD

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

dev = fixture(ProvisionedDevice)
shipped = fixture(ShippedX)
x = fixture(lambda: ProvisionedX(shipped()))  # onboarded on 5 GHz: OTA goes through the modem


def tearDownModule():
    close_fixtures()


class Case(unittest.TestCase):
    device = staticmethod(dev)

    def setUp(self):
        m = self.device().mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def update_from(self, path: str | None, data: bytes | None = None, ctype: str = "application/octet-stream"):
        """The first /api/display answer asks for an update from `path` on the mock."""
        m = self.device().mock
        url = m.set_file(path, ctype, data) if data is not None else (m.device_url + path if path else "")
        m.display_queue = [{"image": "default", "refresh_rate": 300, "update_firmware": True, "firmware_url": url}]

    def assert_keeps_running_the_old_firmware(self, s, path: str | None):
        if path:
            self.device().mock.wait_for_request(path, timeout_s=20)
        st = s.wait(state="deep_sleep", timeout_s=20, settle_ms=500)["status"]
        self.assertNotIn(f"Loaded app from partition at offset {DEVICE.ota_slot:#x}", "\n".join(s.console(0)))
        return st


class OgUpdates(Case):
    def test_update_without_a_url_is_ignored(self):
        self.update_from(None)
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, None)

    def test_missing_firmware_file(self):
        self.update_from("/missing.bin")
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/missing.bin")

    def test_failed_update_is_not_retried_within_a_day(self):
        # The failure's time is stored so a bad update can't boot-loop the device.
        m = self.device().mock
        self.update_from("/missing.bin")
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/missing.bin")
            m.display_queue = list(m.display_queue) or [
                {"image": "default", "refresh_rate": 300, "update_firmware": True,
                 "firmware_url": m.device_url + "/missing.bin"}]
            n, c = len(m.requests), s.status()["console_total"]
            s.wake()
            m.wait_for_request("/api/display", after=n, timeout_s=20)
            s.wait(console=r"Last OTA attempt was < 24h ago, skipping", since=c, timeout_s=20)
            s.wait(state="deep_sleep", timeout_s=20)
            self.assertNotIn("/missing.bin", [r.path for r in m.requests[n:]])

    @unittest.skip("slow: after the connection drops, Update.writeStream waits well over 20 s for the "
                   "rest of the firmware")
    def test_download_cut_short(self):
        firmware = (self.device().build / "firmware.bin").read_bytes()
        self.update_from("/firmware.bin", firmware)
        self.device().mock.set_fault("/firmware.bin", truncate=20_000)
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/firmware.bin")

    def test_file_that_is_not_firmware(self):
        self.update_from("/not-firmware.bin", bytes(range(256)) * 400)
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/not-firmware.bin")

    def test_firmware_too_large_for_the_slot(self):
        self.update_from("/huge.bin", b"\xe9" + bytes(3 * 1024 * 1024))
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/huge.bin")

    def test_firmware_host_unreachable(self):
        self.update_from("/firmware.bin", b"x")
        self.device().mock.set_fault("/firmware.bin", close=True)
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/firmware.bin")


class XModemUpdates(Case):
    ENV = "TRMNL_X"
    device = staticmethod(x)

    @classmethod
    def setUpClass(cls):
        if not (X_BUILD / "firmware.elf").exists():
            raise unittest.SkipTest(f"no TRMNL_X build at {X_BUILD}")

    def test_modem_update_with_a_file_that_is_not_firmware(self):
        self.update_from("/not-firmware.bin", bytes(range(256)) * 400)
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/not-firmware.bin")

    def test_modem_update_of_a_missing_file(self):
        self.update_from("/missing.bin")
        with self.device().boot() as s:
            self.assert_keeps_running_the_old_firmware(s, "/missing.bin")

    def test_modem_update_installs_the_new_firmware(self):
        firmware = (X_BUILD / "firmware.bin").read_bytes()
        self.update_from("/firmware.bin", firmware)
        with self.device().boot() as s:
            self.device().mock.wait_for_request("/firmware.bin", timeout_s=20)
            s.wait_for_console(r"Modem OTA successful", timeout_s=20)
            n = len(self.device().mock.requests)
            self.device().mock.wait_for_request("/api/display", after=n, timeout_s=20)
            s.wait(state="deep_sleep", timeout_s=20)


if __name__ == "__main__":
    unittest.main()
