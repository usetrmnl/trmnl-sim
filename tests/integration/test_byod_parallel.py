"""BYOD boards with a 16-gray parallel e-paper panel driven by FastEPD (the firmware's
PARALLEL_EPD builds other than the TRMNL X): the M5Stack PaperS3 and the LilyGo T5 4.7" S3
Pro, both with the 4.7" ED047TC1 960x540 panel."""

import unittest

from support import sim
from support_byod import ByodBoard
from trmnl_mock import expected_gray, png_image

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

W, H = 960, 540


def ramp16(x: int, y: int) -> int:
    """16 vertical bands, black at the left."""
    return min(15, x * 16 // W)


class ParallelBoard(ByodBoard):
    SIZE = (W, H)
    INKS = "gray16"

    def test_shows_16_grays(self):
        m = self.dev.mock
        m.images["ramp.png"] = png_image(ramp16, W, H, bits=4)
        m._stamp("ramp")
        m.display = {"image": "ramp", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/ramp.png")
            result = s.compare_screen(expected_gray(ramp16, W, H, bits=4), tolerance=40, max_ratio=0.001)
            self.assertTrue(result["match"], result)

    # Firmware bug: display_show_msg2(WIFI_CONNECT) uses the TRMNL X's big font on every
    # PARALLEL_EPD board and centres 'Connect your phone or computer to "TRMNL-XXXXXX" Wi-Fi'
    # with (width - text width) / 2, which goes negative on a 960 px panel: the line starts
    # at the left edge and is cut off after "Wi" at the right.
    @unittest.expectedFailure
    def test_setup_screen_text_fits_the_panel(self):
        flash = self.dev.dir / "setup-screen.bin"
        with sim(self.dev.build, flash=flash, erase=True, extra_args=("--offline",)) as s:
            s.wait(portal=True, timeout_s=90)
            s.wait(display_idle=True, timeout_s=30)
            paper = expected_gray(lambda x, y: 1, 4, 120, bits=1)
            for x in (0, W - 4):  # 4 px margins on both sides of the instructions
                result = s.compare_screen(paper, region=(x, 350, 4, 120), tolerance=16, max_ratio=0)
                self.assertTrue(result["match"], (x, result))

    def test_reports_the_battery_it_measures(self):
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            s.set_battery(3700)
            n = len(self.dev.mock.requests)
            s.wake()
            req = self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
            self.assertAlmostEqual(float(req.headers["Battery-Voltage"]), 3.7, delta=0.06)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)


class M5PaperS3(ParallelBoard, unittest.TestCase):
    ENV = "TRMNL_X_PAPERS3"
    NAME = "M5Stack PaperS3"
    MODEL = "m5_papers3"

    def test_has_no_wake_button(self):
        # device_list[] has interrupt_pin 0xff: only the timer wakes it
        with self.dev.boot_asleep() as s:
            self.assertFalse(s.status()["board"]["has_button"])
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.assertEqual(st["status"]["state"], "deep_sleep")


class LilyGoT5Pro(ParallelBoard, unittest.TestCase):
    ENV = "TRMNL_X_LILYGO_T5PRO"
    NAME = "LilyGo T5 4.7\" S3 Pro"
    MODEL = "lilygo_t5pro"

    def test_button_wakes_it(self):
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            n = len(self.dev.mock.requests)
            s.press(200)
            s.wait_for_console(r"woken by button", timeout_s=30)
            self.dev.mock.wait_for_request("/api/display", after=n, timeout_s=60)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)


if __name__ == "__main__":
    unittest.main()
