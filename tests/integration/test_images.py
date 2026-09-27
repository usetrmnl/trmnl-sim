"""Image formats and bad images on the SPI-panel devices (TRMNL OG and BWRY): PNG in every
depth and color type, JPEG, BMP variants, and images the device must refuse."""

import hashlib
import struct
import time
import unittest

from support import BWRY_BUILD, HERE, ProvisionedDevice, big_number, close_fixtures, fixture
from trmnl_mock import bmp_1bit, expected_gray, png_image, png_palette, png_rgb

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

DATA = HERE / "data"
dev = fixture(ProvisionedDevice)
bwry = fixture(lambda: ProvisionedDevice(BWRY_BUILD))

seven = big_number("7")


def digit(level_black: int, level_white: int, text: str = "7"):
    px = big_number(text)
    return lambda x, y: level_black if px(x, y) else level_white


def bmp(width: int = 800, height: int = 480, palette: bytes = bytes([0, 0, 0, 0, 255, 255, 255, 0]),
        offset: int | None = None, magic: bytes = b"BM") -> bytes:
    """A 1-bit BMP with a blank (index 0) image, for header checks."""
    row = ((width + 31) // 32) * 4
    data = bytes(row * height)
    off = 14 + 40 + len(palette) if offset is None else offset
    header = magic + struct.pack("<IHHI", 14 + 40 + len(palette) + len(data), 0, 0, off)
    info = struct.pack("<IiiHHIIiiII", 40, width, height, 1, 1, 0, len(data), 2835, 2835, 2, 2)
    return header + info + palette + data


def settle(s, timeout_s: float = 120) -> dict:
    """Wait until the device sleeps or restarts (a crash); returns its status."""
    deadline = time.time() + timeout_s
    while True:
        st = s.status()
        if st["boot_count"] > 1 or st["state"] == "halted":
            return st
        if st["state"] == "deep_sleep":
            return s.wait(state="deep_sleep", timeout_s=30, settle_ms=200)["status"]
        if time.time() > deadline:
            raise TimeoutError(f"device neither slept nor restarted: {st}")
        time.sleep(0.2)


class Case(unittest.TestCase):
    device = staticmethod(dev)

    def setUp(self):
        m = self.device().mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def serve(self, name: str, data: bytes, content_type: str, **fields) -> str:
        """Make the next /api/display answers point at `data`; returns its path."""
        m = self.device().mock
        path = f"/img/{name}"
        url = m.set_file(path, content_type, data)
        uid = hashlib.sha1(name.encode()).hexdigest()[:6]
        m.display = {"image_url": url, "filename": f"plugin-{uid}-{int(time.time())}", "refresh_rate": 300, **fields}
        return path

    def show(self, name: str, data: bytes, content_type: str, **fields):
        """Boot, get `data` shown, and return the simulator (asleep afterwards)."""
        path = self.serve(name, data, content_type, **fields)
        s = self.device().boot()
        try:
            self.device().mock.wait_for_request(path, timeout_s=90)
            st = settle(s)
            self.assertEqual((st["boot_count"], st["state"]), (1, "deep_sleep"), "the device crashed")
        except BaseException:
            s.close()
            raise
        return s

    def assert_shows(self, s, expected: bytes, tolerance: int = 64, max_ratio: float = 0):
        result = s.compare_screen(expected, tolerance=tolerance, max_ratio=max_ratio)
        self.assertTrue(result["match"], result)


class Png(Case):
    def test_1bit_png(self):
        level = digit(0, 1)
        with self.show("one.png", png_image(level, 800, 480, 1), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 1))

    def test_2bit_png_with_two_colors_is_drawn_as_1bit(self):
        level = digit(0, 3)
        with self.show("two-colors.png", png_image(level, 800, 480, 2), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 2))

    def test_2bit_png_uses_4_gray_levels(self):
        def level(x, y):
            return 0 if seven(x, y) else min(3, x * 4 // 800)

        with self.show("gray4.png", png_image(level, 800, 480, 2), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 2), tolerance=48, max_ratio=0.01)

    def test_8bit_gray_png_is_reduced(self):
        level = digit(0, 255)
        with self.show("gray8.png", png_image(level, 800, 480, 8), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 8))

    @unittest.expectedFailure
    def test_4bit_gray_png_is_reduced(self):
        # ReduceBpp's 4-bit grayscale case builds odd pixels as (s[0] & 0xf) | (s[0] << 4)
        # without masking to 8 bits: white is 0xfff, and g >> 7 ORs 5 bits into the output
        # byte, so most of the image comes out black.
        level = digit(0, 15)
        with self.show("gray16.png", png_image(level, 800, 480, 4), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 4))

    def test_palette_png_is_reduced(self):
        colors = [(0, 0, 0), (255, 255, 255)]
        data = png_palette(lambda x, y: colors[0] if seven(x, y) else colors[1], colors)
        with self.show("palette.png", data, "image/png") as s:
            self.assert_shows(s, expected_gray(digit(0, 1), 800, 480, 1))

    def test_truecolor_png_is_reduced(self):
        data = png_rgb(lambda x, y: (0, 0, 0) if seven(x, y) else (255, 255, 255))
        with self.show("rgb.png", data, "image/png") as s:
            self.assert_shows(s, expected_gray(digit(0, 1), 800, 480, 1))

    def test_portrait_png_is_rotated(self):
        px = big_number("7")
        level = lambda x, y: 0 if px(y, 479 - x) else 1  # noqa: E731  (the digit, turned)
        with self.show("portrait.png", png_image(level, 480, 800, 1), "image/png") as s:
            screen = s.screenshot()
            self.assertGreater(len(screen), 0)

    def test_larger_png_is_cropped(self):
        px = big_number("7")
        level = lambda x, y: 0 if x < 800 and y < 480 and px(x, y) else 1  # noqa: E731
        with self.show("large.png", png_image(level, 1000, 600, 1), "image/png") as s:
            self.assert_shows(s, expected_gray(digit(0, 1), 800, 480, 1))

    def test_corrupt_png_is_not_drawn(self):
        data = png_image(digit(0, 1), 800, 480, 1)
        with self.show("corrupt.png", data[:40] + bytes(len(data) - 40), "image/png") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_same_png_again_is_not_downloaded_or_redrawn(self):
        level = digit(0, 1)
        with self.show("again.png", png_image(level, 800, 480, 1), "image/png") as s:
            refreshes = s.status()["display_refreshes"]
            n = len(self.device().mock.requests)
            s.wake()
            self.device().mock.wait_for_request("/api/display", after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90, settle_ms=200)
            self.assertEqual([r.path for r in self.device().mock.requests[n:]], ["/api/display"])
            self.assertEqual(s.status()["display_refreshes"], refreshes)

    def test_new_version_of_a_plugin_image_replaces_the_cached_one(self):
        m = self.device().mock
        old, new = png_image(digit(0, 1, "1"), 800, 480, 1), png_image(digit(0, 1, "2"), 800, 480, 1)
        m.set_file("/img/v1.png", "image/png", old)
        m.set_file("/img/v2.png", "image/png", new)
        m.display_queue = [{"image_url": m.device_url + "/img/v1.png", "filename": "plugin-abc123-1000", "refresh_rate": 300}]
        m.display = {"image_url": m.device_url + "/img/v2.png", "filename": "plugin-abc123-2000", "refresh_rate": 300}
        with self.device().boot() as s:
            m.wait_for_request("/img/v1.png", timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)
            c = s.status()["console_total"]
            s.wake()
            s.wait(console=r"Deleting older version of plugin image", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)
            self.assert_shows(s, expected_gray(digit(0, 1, "2"), 800, 480, 1))

    def test_long_filenames_are_shortened(self):
        level = digit(0, 1)
        path = self.serve("long.png", png_image(level, 800, 480, 1), "image/png")
        self.device().mock.display["filename"] = "mashup-066cc3-weather-and-calendar-1771674964"
        with self.device().boot() as s:
            self.device().mock.wait_for_request(path, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)
            self.assert_shows(s, expected_gray(level, 800, 480, 1))

    def test_temperature_profile_is_saved(self):
        level = digit(0, 1)
        data = png_image(level, 800, 480, 1)
        with self.show("temp.png", data, "image/png", temperature_profile="a") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 1))
            self.serve("temp2.png", png_image(digit(0, 1, "8"), 800, 480, 1), "image/png", temperature_profile="b")
            n = len(self.device().mock.requests)
            s.wake()
            self.device().mock.wait_for_request("/img/temp2.png", after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            self.assert_shows(s, expected_gray(digit(0, 1, "8"), 800, 480, 1))

    def test_maximum_compatibility_forces_full_refreshes(self):
        level = digit(0, 1)
        with self.show("compat.png", png_image(level, 800, 480, 1), "image/png", maximum_compatibility=True) as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 1))

    def test_long_refresh_rates_use_fast_instead_of_partial_refreshes(self):
        with self.show("slow1.png", png_image(digit(0, 1), 800, 480, 1), "image/png", refresh_rate=3600) as s:
            eight = digit(0, 1, "8")
            self.serve("slow2.png", png_image(eight, 800, 480, 1), "image/png", refresh_rate=3600)
            n = len(self.device().mock.requests)
            s.wake()
            self.device().mock.wait_for_request("/img/slow2.png", after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            self.assert_shows(s, expected_gray(eight, 800, 480, 1))


class Jpeg(Case):
    def test_jpeg_is_dithered_to_1bit(self):
        with self.show("five.jpg", (DATA / "five_800x480.jpg").read_bytes(), "image/jpeg") as s:
            self.assert_shows(s, expected_gray(digit(0, 1, "5"), 800, 480, 1), tolerance=64, max_ratio=0.02)

    def test_jpeg_of_the_wrong_size_is_refused(self):
        with self.show("small.jpg", (DATA / "five_640x480.jpg").read_bytes(), "image/jpeg") as s:
            self.assertNotEqual(s.status()["state"], "halted")


class Bmp(Case):
    @unittest.expectedFailure
    def test_bmp_with_an_inverted_palette(self):
        # parseBMPHeader recognizes the white/black palette ("Color scheme reversed") and
        # sets image_reverse, but nothing uses it (display_show_image's inversion is under
        # #ifdef FUTURE): the image is shown inverted.
        level = digit(0, 1)
        data = bytearray(bmp_1bit(seven))
        data[54:62] = bytes([255, 255, 255, 0, 0, 0, 0, 0])  # index 0 = white, 1 = black
        for i in range(62, len(data)):
            data[i] ^= 0xFF
        with self.show("inverted.bmp", bytes(data), "image/bmp") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 1))

    def test_not_a_bmp(self):
        with self.show("garbage.bmp", b"XX" + bytes(48060), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_bmp_of_the_wrong_size(self):
        with self.show("small.bmp", bmp(400, 240), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_bmp_with_a_color_palette(self):
        with self.show("red.bmp", bmp(palette=bytes([0, 0, 255, 0, 255, 255, 255, 0])), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_bmp_with_a_bad_data_offset(self):
        with self.show("offset.bmp", bmp(offset=100), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")


class Refused(Case):
    def test_image_too_large_to_download(self):
        path = self.serve("huge.png", bytes(95000), "image/png")
        with self.device().boot() as s:
            self.device().mock.wait_for_request(path, timeout_s=90)
            st = settle(s)
            self.assertEqual((st["boot_count"], st["state"]), (1, "deep_sleep"))

    def test_empty_image(self):
        path = self.serve("empty.png", b"", "image/png")
        with self.device().boot() as s:
            self.device().mock.wait_for_request(path, timeout_s=90)
            st = settle(s)
            self.assertEqual((st["boot_count"], st["state"]), (1, "deep_sleep"))
        # Content-Length 0 takes the "no Content-Length" path (contentLength <= 0): if the
        # server's close has already arrived, writeToStream() reports the download as cut
        # short; otherwise it reads nothing and finishBody() says "No data received".
        log = self.device().mock.wait_for_request("/api/log", timeout_s=10)
        self.assertRegex(log.body.decode(), r"No data received|connection closed mid-download")

    def test_missing_image(self):
        self.device().mock.display = {"image_url": self.device().mock.device_url + "/img/missing.png",
                                      "filename": "plugin-000000-1", "refresh_rate": 300}
        with self.device().boot() as s:
            self.device().mock.wait_for_request("/img/missing.png", timeout_s=90)
            st = settle(s)
            self.assertEqual((st["boot_count"], st["state"]), (1, "deep_sleep"))
            self.assertLess(st["wake_at_s"] - st["sim_time_s"], 300)


class BwryPng(Case):
    device = staticmethod(bwry)

    def setUp(self):
        if not (BWRY_BUILD / "firmware.elf").exists():
            self.skipTest(f"no TRMNL BWRY build at {BWRY_BUILD}")
        super().setUp()

    def test_1bit_png(self):
        level = digit(0, 1)
        with self.show("one.png", png_image(level, 800, 480, 1), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 1))

    def test_8bit_gray_png(self):
        level = digit(0, 255)
        with self.show("gray8.png", png_image(level, 800, 480, 8), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 8))

    @unittest.expectedFailure
    def test_truecolor_png(self):
        # trmnl_4clr doesn't set PNG_MAX_BUFFERED_PIXELS (trmnl does), so PNGdec's default
        # row buffer is too small for 800 px truecolor rows: the decode corrupts the heap
        # (store fault in tlsf_free), and the device crashes, restarts, downloads the same
        # image and crashes again, until the server sends something else.
        data = png_rgb(lambda x, y: (255, 0, 0) if seven(x, y) else (255, 255, 255))
        with self.show("rgb.png", data, "image/png") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_jpeg(self):
        with self.show("five.jpg", (DATA / "five_800x480.jpg").read_bytes(), "image/jpeg") as s:
            self.assertNotEqual(s.status()["state"], "halted")


def tearDownModule():
    close_fixtures()


if __name__ == "__main__":
    unittest.main()
