"""BYOD boards whose panel and wiring are built into bb_epaper (`bbep.begin()`), in the
firmware's `#ifdef CMD_CS1_CS2` rows: the M5Paper Mono (3.97" 800x480 SSD1677, its supply
and RST on an M5IOE1 I/O expander at I2C 0x4f), the M5Paper Color (4" 400x600 Spectra 6,
powered through the board's PY32 at I2C 0x6e) and the Seeed reTerminal E1004 (13.3"
1200x1600 Spectra 6 driven by two controllers, one per half, on two chip selects)."""

import unittest

from support import KNOWN_MEMORY_BUGS
from support_byod import ByodBoard
from trmnl_mock import SPECTRA6_RGB, expected_gray, png_image

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

INKS = list(SPECTRA6_RGB)  # black, white, yellow, red, blue, green


class M5PaperMono(ByodBoard, unittest.TestCase):
    ENV = "m5_paper_mono"
    NAME = "M5Paper Mono"
    MODEL = "m5_paper_mono"
    BATTERY_V = 4.2  # BATT_NONE

    # Firmware bug: for a 1-bit PNG display_show_image() calls
    # bbep.setPanelType(dpList[panel_set][iTempProfile].OneBit), but for this board dpList
    # holds bb_epaper *product* IDs meant for bbep.begin() (EPD_M5_PAPER_MONO = 30), not
    # panel types. 30 is a valid panel index, EP266YR_184x360 (a UC81xx 4-color panel), so
    # the image is written and "refreshed" with UC81xx commands the SSD1677 doesn't
    # understand (its DRF 0x12 is the SSD1677's SW reset) and the old screen stays up.
    # 2-bit images take the bbep.begin() path and work (test_four_grays).
    @unittest.expectedFailure
    def test_shows_the_served_image(self):
        super().test_shows_the_served_image()

    def test_four_grays(self):
        """A 2-bit PNG: bb_epaper's 4-gray mode (EPD_M5_PAPER_MONO_4GRAY, a waveform in the
        LUT register) gives black, two grays and white."""
        w, h = self.SIZE

        def level(x, y):  # four vertical bands, and a checker of all four at the bottom
            if y >= 400:
                return (x // 40 + y // 40) % 4
            return x * 4 // w

        m = self.dev.mock
        m.images["grays.png"] = png_image(level, w, h, bits=2)
        m._stamp("grays")
        m.display = {"image": "grays", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/grays.png")
            result = s.compare_screen(expected_gray(level, w, h, bits=2), tolerance=16, max_ratio=0.001)
            self.assertTrue(result["match"], result)


class M5PaperColor(ByodBoard, unittest.TestCase):
    # USB CDC on boot and WAIT_FOR_SERIAL: the firmware waits up to 2 s for a host
    ENV = "m5_paper_color"
    NAME = "M5Paper Color"
    MODEL = "m5_paper_color"
    SIZE = (400, 600)  # portrait
    BATTERY_V = 4.2  # BATT_NONE
    INKS = "spectra6"

    def test_every_ink_in_portrait(self):
        """Six horizontal bands, one per ink, with a checker of all six at the bottom: rows
        and columns of the portrait panel land where the image has them."""
        w, h = self.SIZE

        def color(x, y):
            if y >= 540:
                return SPECTRA6_RGB[INKS[(x // 20 + y // 20) % 6]]
            return SPECTRA6_RGB[INKS[y // 90]]

        m = self.dev.mock
        expected = m.set_spectra6_png("bands", color, w, h)
        m.display = {"image": "bands", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            self.refresh(s, "/images/bands.png")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0)
            self.assertTrue(result["match"], result)


class ReTerminalE1004(ByodBoard, unittest.TestCase):
    ENV = "seeed_reTerminal_E1004"
    NAME = "reTerminal E1004"
    MODEL = "reterminal_e1004"
    SIZE = (1200, 1600)
    INKS = "spectra6"
    PNG_DEFAULT = True  # the mock's usual 800x480 BMP crashes it (test_takes_an_800x480_bmp)

    # Firmware bug: display_show_image() un-flips an uncompressed BMP in place with
    # flip_image(image_buffer + 62, bbep.width(), bbep.height()), i.e. assuming the BMP is
    # panel-sized, and then sends it as the panel's plane (writePlane reads a 4-bit
    # 1200x1600 plane, 960 KB, from it). An 800x480 1-bit BMP (48 KB) on this panel makes
    # flip_image write ~190 KB past the download buffer. Seen during onboarding with the
    # mock's default BMP: the heap's free-block headers get overwritten (with 0x11, white
    # Spectra pixels) and the device panics in HttpRetryRequest::releaseBody -> free(), then
    # reboots, fetches the same image and crashes again. Whether it crashes depends on the
    # heap layout, so this test catches the overflow with --memcheck. (The E1002 skips
    # writePlane for BMPs; flip_image is a known bug on the X too, see KNOWN_MEMORY_BUGS.)
    @unittest.expectedFailure
    def test_takes_an_800x480_bmp(self):
        m = self.dev.mock
        m.set_image("bmp", lambda x, y: (x // 40 + y // 40) % 2 == 0)
        m.display = {"image": "bmp", "refresh_rate": 300}
        others = tuple(f for f in KNOWN_MEMORY_BUGS if f != "_Z10flip_imagePhiib")
        with self.dev.boot_asleep(memcheck="log", memcheck_suppress=others) as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=60)
            n = len(m.requests)
            s.wake()
            m.wait_for_request("/images/bmp.bmp", after=n, timeout_s=60)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
            self.assertNotIn("Guru Meditation", s.log())
            self.assertEqual(s.memcheck()["violations"], [])

    def test_each_controller_shows_its_half(self):
        """Different content left and right of the seam at x = 600, and bands that cross it:
        the left controller (CS 10) shows the left half, the right one (CS 2) the right."""
        w, h = self.SIZE

        def color(x, y):
            if y < 400:  # six bars across the whole width
                return SPECTRA6_RGB[INKS[x * 6 // w]]
            if y < 800:  # one ink per half
                return SPECTRA6_RGB["red" if x < w // 2 else "blue"]
            if y < 1200:  # a checker straddling the seam
                return SPECTRA6_RGB[INKS[(x // 50 + y // 50) % 6]]
            return SPECTRA6_RGB["green" if x < 590 or x >= 610 else "black"]  # a thin seam line

        m = self.dev.mock
        expected = m.set_spectra6_png("halves", color, w, h)
        m.display = {"image": "halves", "refresh_rate": 300}
        with self.dev.boot_asleep() as s:
            before = s.wait(state="deep_sleep", display_idle=True, timeout_s=60)["status"]["display_refreshes"]
            status = self.refresh(s, "/images/halves.png")
            result = s.compare_screen(expected, tolerance=16, max_ratio=0)
            self.assertTrue(result["match"], result)
            # one refresh for the image: both controllers refresh together
            self.assertEqual(status["display_refreshes"] - before, 1)


if __name__ == "__main__":
    unittest.main()
