"""Fault injection on the TRMNL X: a missing fuel gauge or one that loses its configuration,
a panel whose PMIC never powers up, a modem that stops answering, downloads cut on the
5 GHz (modem) path, and power loss while the firmware writes NVS."""

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

    def test_fuel_gauge_loses_its_configuration(self):
        # A power-on reset (battery disconnected) puts the gauge back on factory data memory
        # with ITPOR set; the next wake has to write the golden file again.
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            s.set_faults(gauge_reset=True)
            s.set_faults(gauge_reset=None)
            n = len(dev.mock.requests)
            s.wake()
            h = dev.mock.wait_for_request("/api/display", after=n, timeout_s=30).headers
            s.wait(state="deep_sleep", timeout_s=30)
        # Readable again (an unconfigured gauge is reported as -1), with the golden file's
        # 6000 mAh design capacity rather than the factory 1340 mAh.
        self.assertEqual((h["Gauge-SOC"], h["Gauge-Capacity"]), ("83", "4980/6000"))
        # The factory calibration inverts the current; the firmware flips CC Gain's sign back.
        self.assertEqual(h["Battery-Current"], "-50")

    def touch_bar_fault(self, kind: str):
        """Asleep, the touch controller starts misbehaving; wake by a tap, then by the timer
        (with the fault cleared). Returns the console."""
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            s.set_faults(touch_bar=kind)
            n = len(dev.mock.requests)
            s.touch("center", 150)
            dev.mock.wait_for_request("/api/display", after=n, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=300)
            s.set_faults(touch_bar=None)
            n = len(dev.mock.requests)
            s.wake()
            dev.mock.wait_for_request("/api/display", after=n, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)
            return "\n".join(s.console(0))

    def test_touch_controller_reset_while_asleep(self):
        # The wake stub reads SHOW_RESET; the IQS323 task reinitializes it (its "IQS323 Task:"
        # lines go to Serial, which production X builds don't mirror to the log).
        self.assertIn("wakeup_stub_iqs_status.status: 0x80", self.touch_bar_fault("reset"))

    def test_touch_controller_i2c_lockup(self):
        self.touch_bar_fault("lockup")

    def test_touch_controller_ati_error(self):
        self.touch_bar_fault("ati_error")

    def modem_errors(self, *prefixes) -> dict:
        """Asleep on 5 GHz, the modem starts answering ERROR to these commands; the device
        must still get back to sleep after a timer wake. Returns the status."""
        with dev.boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            s.set_faults(modem_at_errors=list(prefixes))
            s.wake()
            st = s.wait(state="deep_sleep", timeout_s=30, settle_ms=300)["status"]
            self.assertNotEqual(st["state"], "halted")
            return st

    def test_modem_station_mode_fails(self):
        self.modem_errors("AT+CWMODE")

    def test_modem_auto_connect_setting_fails(self):
        self.modem_errors("AT+CWAUTOCONN")

    def test_modem_baud_rate_change_fails(self):
        self.modem_errors("AT+UART_CUR")

    def test_modem_cannot_join(self):
        self.modem_errors("AT+CWJAP=")

    def test_modem_http_headers_rejected(self):
        self.modem_errors("AT+HTTPCHEAD")

    def test_modem_time_sync_fails(self):
        self.modem_errors("AT+CIPSNTPCFG")

    def test_modem_time_query_fails(self):
        self.modem_errors("AT+CIPSNTPTIME")

    def test_modem_mac_and_signal_queries_fail(self):
        self.modem_errors("AT+CIPSTAMAC", "AT+CWJAP?")

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
