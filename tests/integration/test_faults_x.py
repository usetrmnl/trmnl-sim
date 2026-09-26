"""Fault injection on the TRMNL X: a missing fuel gauge, a panel whose PMIC never powers up, a
modem that stops answering, downloads cut on the 5 GHz (modem) path, and power loss while
the firmware writes NVS."""

import unittest

from support import big_number
from support_x import ProvisionedX, ShippedX, X_BUILD

shipped: ShippedX
dev: ProvisionedX

FUEL_GAUGE = 0x55  # BQ27427
TOUCH_BAR = 0x44  # IQS323


def setUpModule():
    global shipped, dev
    if not (X_BUILD / "firmware.elf").exists():
        raise unittest.SkipTest(f"no TRMNL_X build at {X_BUILD} (set TRMNL_X_BUILD)")
    shipped = ShippedX()
    dev = ProvisionedX(shipped)  # on the 5 GHz network: all HTTP goes through the modem


def tearDownModule():
    dev.close()
    shipped.close()


def digits(text: str):
    pixel = big_number(text)
    return lambda x, y: 0 if pixel(x * 800 // 1872, y * 480 // 1404) else 1


class FaultsX(unittest.TestCase):
    def setUp(self):
        dev.mock.requests.clear()
        dev.mock.display_queue.clear()
        dev.mock.clear_faults()
        self.expected = dev.mock.set_png("five", digits("5"))
        dev.mock.display = {"image": "five", "refresh_rate": 300}

    def shows_image(self, s) -> bool:
        return s.compare_screen(self.expected, tolerance=64, max_ratio=0)["match"]

    def test_fuel_gauge_absent(self):
        with dev.boot(faults={"i2c_absent": [FUEL_GAUGE]}) as s:
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            # The firmware reports -1 when it can't read the gauge, and carries on.
            self.assertEqual(float(req.headers["Battery-Voltage"]), -1.0)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=300)
            self.assertTrue(self.shows_image(s))

    @unittest.expectedFailure
    def test_touch_controller_absent(self):
        # bl_init() restarts the device when the IQS323 task fails to initialize, on every
        # boot: a device whose touch controller died boot-loops (never reaching the server,
        # draining the battery) instead of carrying on without its touch bar.
        with dev.boot(faults={"i2c_absent": [TOUCH_BAR]}) as s:
            dev.mock.wait_for_request("/api/display", timeout_s=15)
            self.assertLess(s.status()["boot_count"], 3)

    def test_panel_power_failure(self):
        # The PMIC's power good never comes: the panel gets no drive voltages.
        with dev.boot(faults={"panel_busy_stuck": True}) as s:
            dev.mock.wait_for_request("/images/five.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=180)
            self.assertFalse(self.shows_image(s))

    def test_unresponsive_modem(self):
        with dev.boot(faults={"modem_unresponsive": True}) as s:
            s.wait(console=r"Connection failed", timeout_s=180)
            s.wait(state="deep_sleep", timeout_s=180)
            self.assertEqual(dev.mock.requests, [])
            s.clear_faults()
            s.wake()
            dev.mock.wait_for_request("/images/five.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=300)
            self.assertTrue(self.shows_image(s))

    def test_download_cut_on_the_modem_path(self):
        # /api/display (about 230 bytes) gets through; the PNG (about 1.8 kB) does not.
        with dev.boot(faults={"net": {"tcp_cut": {"after_bytes": 1_000}}}) as s:
            s.wait(console=r"Image download failed", timeout_s=180)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertEqual(dev.mock.count("/images/five.png"), 5, "the firmware retries 5 times")
            self.assertFalse(self.shows_image(s))
            s.clear_faults()
            n = len(dev.mock.requests)
            s.wake()
            dev.mock.wait_for_request("/images/five.png", after=n, timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=300)
            self.assertTrue(self.shows_image(s))

    def test_power_loss_during_nvs_write_then_boots(self):
        # The X writes NVS early on every boot; tear the first page program.
        with dev.boot(faults={"power_loss": {"partition": "nvs", "op": "program", "cut": "torn"}}) as s:
            s.wait(console=r"\[sim\] power lost: program #1 .* in partition nvs", timeout_s=120)
            dev.mock.wait_for_request("/images/five.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=300)
            self.assertTrue(self.shows_image(s))
            self.assertEqual(dev.mock.count("/api/setup"), 0, "must not lose its registration")


if __name__ == "__main__":
    unittest.main()
