"""Image formats and bad images on the device under test (see devices.py; the TRMNL X has
its own, test_images_x): PNG in every depth and color type, JPEG, BMP variants, and images
the device must refuse. Images are the panel's size; what should be on screen is the same
picture, reduced to what the panel shows."""

import hashlib
import struct
import time
import unittest

from support import BWRY_BUILD, DEVICE, HERE, ProvisionedDevice, close_fixtures, fixture, panel_number
from trmnl_mock import (big_number, expected_bwry, expected_gray, expected_spectra6, png_image, png_palette,
                        png_rgb)

from devices import ANY, DEVICES

ENV = ANY  # general tests: they run on the device under test (see devices.py)

# Firmware bugs, by the device they show on (see run.py's DeviceLoader).
REDUCE_BPP_4BIT = (
    "ReduceBpp (display.cpp:945) builds a 4-bit gray PNG's odd pixels as (s[0] & 0xf) | (s[0] << 4) "
    "without masking to 8 bits: white is 0xfff, and g >> 7 ORs 5 bits into the output byte, so most "
    "of the image comes out black (png_draw's path for black-and-white SPI panels)")
CROWPANEL_1BIT_PNG = (
    "png_to_epd (display.cpp:1764) calls bbep.setPanelType(dpList[...].OneBit) for 1-bit PNGs, but the "
    "CrowPanel's dpList row holds bb_epaper product numbers for bbep.begin() (its device_list[] row has "
    "no pins): EPD_CROWPANEL42 = 8 is taken as panel type EP295_128x296_4GRAY, and the picture never "
    "shows (see test_byod_ssd.CrowPanel42)")
BWRY_TRUECOLOR = (
    "trmnl_4clr doesn't set PNG_MAX_BUFFERED_PIXELS (platformio.ini [env:trmnl_4clr]; trmnl does), so "
    "PNGdec's default row buffer is too small for 800 px truecolor rows: the decode corrupts the heap "
    "and the device crashes and restarts (see BwryPng.test_truecolor_png)")
BWRY_WIDE_PNG = (
    "png_draw_4clr (display.cpp:1325) writes every decoded row whole, (iWidth + 3) / 4 bytes, without "
    "cropping it to the panel like png_draw does: a PNG wider than 800 px wraps into the next rows")
COLOR_JPEG = (
    "jpeg_to_epd decodes to 1 bit and jpeg_draw (display.cpp:1608) sends the rows with "
    "startWrite(PLANE_0) as a 1-bpp plane, which on the color panels' UC81xx controllers is command "
    "0x13; they take their pixels through 0x10 (2 bpp on BWRY, bbepWriteImage2bpp; 4 bpp on "
    "Spectra 6, bbepWriteImage4bpp), so the JPEG never shows")
FASTEPD_WIDE_PNG = (
    "FastEPD's png_draw (display.cpp:1470) takes a 1-bit PNG wider than the panel for a portrait one "
    "and draws it rotated: for x up to the image's width it steps a row up from the bottom (d -= "
    "iPitch), running off the top of the framebuffer (StoreProhibited in png_draw, display.cpp:1485), "
    "and the device crashes and restarts, instead of cropping it as png_to_epd announces")

BWR_4GRAY = (
    "png_to_epd sends PNGs of more than two colors or 2 bits (and 4/8-bit gray, truecolor) down the "
    "4-gray path (display.cpp:1789), whose two gray bit planes land in this panel's black/white (DTM1) "
    "and red (DTM2) planes: white comes out black and black red (see test_byod_uc81xx.DiyKitBwr)")
BWR_2BIT_RED = (
    "png_draw's PNG_2_BIT_INVERTED case (display.cpp:1378) writes the inverted picture into the "
    "second plane for 2-bit two-color PNGs, which on this 3-color panel is the red plane (the 1-bit "
    "case clears it for BBEP_3COLOR, display.cpp:1366): the black comes out red")
STICKY_4GRAY = (
    "bb_epaper (the Sticky's pinned 0395f30) starts EP397_800x480_4GRAY refreshes with 0x22 0xD7, "
    "whose load-LUT bit replaces the custom 4-gray LUT its init sequence wrote with the built-in "
    "one, which shows the two gray planes as black and white: PNGs on the 4-gray path come out "
    "inverted (see test_byod_ssd.SeeedSticky.test_shows_a_4_gray_image)")
SSD1677_WINDOW = (
    "jpeg_draw (display.cpp:1607) sets an address window for every 8-row block, and this env's pinned "
    "bb_epaper programs it ascending (bbepSetAddrWindow: X start < end, counter at the start; bytes, "
    "not pixels, for EP397), while SET_ORIENTATION in the panel's init sequence (bbepSetFlip180) "
    "puts the SSD1677 in data entry mode 0x02, X counting down from 799: the blocks run off the left "
    "edge (M5Paper) or scatter into thin lines (Sticky). (bb_epaper 2.1.9 skips the window for "
    "EP426/EP397, so the X4 and 4.26\" kit are fine)")
EP397_ROW_SHIFT = (
    "bb_epaper 2.1.9's EP397_800x480 init sequences make the RAM Y address count down from 479 but "
    "start the counter at 0: the picture is one row too high, its top row at the bottom (see "
    "test_byod_ssd.Waveshare397)")
M5_1BIT_PNG = (
    "png_to_epd (display.cpp:1764) passes the M5Paper Mono's dpList product number (EPD_M5_PAPER_MONO "
    "= 30) to bbep.setPanelType for 1-bit PNGs: panel type EP266YR_184x360, a UC81xx 4-color panel, so "
    "the image goes out with UC81xx commands the SSD1677 doesn't understand and never shows (see "
    "test_byod_m5.M5PaperMono)")
E1004_PNG_BUFFER = (
    "PNG_MAX_BUFFERED_PIXELS=6432 (platformio.ini [env:seeed_reTerminal_E1004]) is sized for 800 px "
    "rows, and PNGdec keeps two rows in that buffer while refusing only a row that alone doesn't fit "
    "(png.inl:643): a 1200 px truecolor row (3601 bytes, 7202 for two) runs past it and the picture "
    "comes out wrong (bands of garbage)")
GEN2_4CLR = (
    "the trmnl_gen2_4clr env defines BOARD_TRMNL_GEN2 but not BOARD_TRMNL_4CLR, which png_to_epd's "
    "4-color path (png_draw_4clr, display.cpp:1752) is compiled under: images go out as two 1-bit "
    "planes the BWRY panel reads as 2 bits per pixel, so only part of the screen changes, in the wrong "
    "inks (see test_og_gen2)")

EP397_SHIFTED_TESTS = [
    "Png.test_1bit_png", "Png.test_2bit_png_with_two_colors_is_drawn_as_1bit", "Png.test_8bit_gray_png_is_reduced",
    "Png.test_palette_png_is_reduced", "Png.test_truecolor_png_is_reduced", "Png.test_larger_png_is_cropped",
    "Png.test_new_version_of_a_plugin_image_replaces_the_cached_one", "Png.test_long_filenames_are_shortened",
    "Png.test_temperature_profile_is_saved", "Png.test_maximum_compatibility_forces_full_refreshes",
    "Png.test_long_refresh_rates_use_fast_instead_of_partial_refreshes",
]
GEN2_4CLR_TESTS = EP397_SHIFTED_TESTS + [
    "Png.test_2bit_png_uses_4_gray_levels", "Png.test_4bit_gray_png_is_reduced"]

CROWPANEL_1BIT_TESTS = [
    "Png.test_1bit_png", "Png.test_2bit_png_with_two_colors_is_drawn_as_1bit", "Png.test_palette_png_is_reduced",
    "Png.test_larger_png_is_cropped", "Png.test_new_version_of_a_plugin_image_replaces_the_cached_one",
    "Png.test_long_filenames_are_shortened", "Png.test_temperature_profile_is_saved",
    "Png.test_maximum_compatibility_forces_full_refreshes",
    "Png.test_long_refresh_rates_use_fast_instead_of_partial_refreshes",
]

KNOWN_FAILURES: dict[str, dict[str, str]] = {
    env: {"Png.test_4bit_gray_png_is_reduced": REDUCE_BPP_4BIT} for env, d in DEVICES.items() if d.inks == "mono"
}
for env, d in DEVICES.items():
    if d.inks in ("bwry", "spectra6"):
        KNOWN_FAILURES.setdefault(env, {})["Jpeg.test_jpeg_is_dithered_to_1bit"] = COLOR_JPEG
    if d.inks == "gray16":
        KNOWN_FAILURES.setdefault(env, {})["Png.test_larger_png_is_cropped"] = FASTEPD_WIDE_PNG
KNOWN_FAILURES["TRMNL_7inch5_OG_DIY_Kit_3CLR"] = {
    **dict.fromkeys(["Png.test_2bit_png_uses_4_gray_levels", "Png.test_4bit_gray_png_is_reduced",
                     "Png.test_8bit_gray_png_is_reduced", "Png.test_truecolor_png_is_reduced"], BWR_4GRAY),
    **dict.fromkeys(["Png.test_2bit_png_with_two_colors_is_drawn_as_1bit", "Png.test_palette_png_is_reduced"],
                    BWR_2BIT_RED),
}
KNOWN_FAILURES["seeed_sticky"].update({
    **dict.fromkeys(["Png.test_2bit_png_uses_4_gray_levels", "Png.test_8bit_gray_png_is_reduced",
                     "Png.test_truecolor_png_is_reduced"], STICKY_4GRAY),
    "Jpeg.test_jpeg_is_dithered_to_1bit": SSD1677_WINDOW,
})
KNOWN_FAILURES["WAVESHARE_397"].update(dict.fromkeys(EP397_SHIFTED_TESTS, EP397_ROW_SHIFT))
KNOWN_FAILURES["m5_paper_mono"].update({**dict.fromkeys(CROWPANEL_1BIT_TESTS, M5_1BIT_PNG),
                                        "Jpeg.test_jpeg_is_dithered_to_1bit": SSD1677_WINDOW})
KNOWN_FAILURES["seeed_reTerminal_E1004"]["Png.test_truecolor_png_is_reduced"] = E1004_PNG_BUFFER
KNOWN_FAILURES["trmnl_gen2_4clr"].update(dict.fromkeys(GEN2_4CLR_TESTS, GEN2_4CLR))
KNOWN_FAILURES["CrowPanel42"].update(dict.fromkeys(CROWPANEL_1BIT_TESTS, CROWPANEL_1BIT_PNG))
KNOWN_FAILURES["trmnl_4clr"].update({
    "Png.test_truecolor_png_is_reduced": BWRY_TRUECOLOR,
    "Png.test_larger_png_is_cropped": BWRY_WIDE_PNG,
})

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

DATA = HERE / "data"
dev = fixture(ProvisionedDevice)
bwry = fixture(lambda: ProvisionedDevice(BWRY_BUILD))

# The panel of the device under test: the general classes' images are its size.
W, H = DEVICE.size

seven = panel_number("7")


def digit(level_black: int, level_white: int, text: str = "7"):
    px = panel_number(text)
    return lambda x, y: level_black if px(x, y) else level_white


def expected(level, bits: int) -> bytes:
    """The screenshot a `bits`-deep gray PNG of `level` should give on the device under test's
    panel. The color panels' PNG decoders (png_draw_4clr / png_draw_6clr in display.cpp) take
    a gray sample's raw bits as its RGB value (2-bit 0x00/0x40/0x80/0xC0, 1-bit 0x00/0x80, 4-bit
    doubled) and pick the nearest ink; the others show the grays."""
    if DEVICE.inks in ("bwry", "spectra6"):
        quantized = expected_bwry if DEVICE.inks == "bwry" else expected_spectra6
        widen = {1: lambda v: v << 7, 2: lambda v: v << 6, 4: lambda v: v * 17, 8: lambda v: v}[bits]
        return quantized(lambda x, y: (widen(level(x, y)),) * 3, W, H)
    return expected_gray(level, W, H, bits)


# The TRMNL BWRY's (BwryPng) 800x480 panel.
og_seven = big_number("7")


def og_digit(level_black: int, level_white: int, text: str = "7"):
    px = big_number(text)
    return lambda x, y: level_black if px(x, y) else level_white


def bmp(width: int = W, height: int = H, palette: bytes = bytes([0, 0, 0, 0, 255, 255, 255, 0]),
        offset: int | None = None, magic: bytes = b"BM", black=None) -> bytes:
    """A 1-bit bottom-up BMP, blank (index 0) unless `black(x, y)` says which pixels are
    index 0 (the rest are index 1)."""
    row = ((width + 31) // 32) * 4
    data = bytearray(row * height)
    if black is not None:
        for y in range(height):
            base = (height - 1 - y) * row
            for x in range(width):
                if not black(x, y):
                    data[base + x // 8] |= 0x80 >> (x % 8)
    data = bytes(data)
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
            return s.wait(state="deep_sleep", display_idle=True, timeout_s=30, settle_ms=200)["status"]
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
        with self.show("one.png", png_image(level, W, H, 1), "image/png") as s:
            self.assert_shows(s, expected(level, 1))

    def test_2bit_png_with_two_colors_is_drawn_as_1bit(self):
        level = digit(0, 3)
        with self.show("two-colors.png", png_image(level, W, H, 2), "image/png") as s:
            self.assert_shows(s, expected(level, 2))

    def test_2bit_png_uses_4_gray_levels(self):
        def level(x, y):
            return 0 if seven(x, y) else min(3, x * 4 // W)

        with self.show("gray4.png", png_image(level, W, H, 2), "image/png") as s:
            self.assert_shows(s, expected(level, 2), tolerance=48, max_ratio=0.01)

    def test_8bit_gray_png_is_reduced(self):
        level = digit(0, 255)
        with self.show("gray8.png", png_image(level, W, H, 8), "image/png") as s:
            self.assert_shows(s, expected(level, 8))

    def test_4bit_gray_png_is_reduced(self):
        # a known failure on the black-and-white panels (REDUCE_BPP_4BIT)
        level = digit(0, 15)
        with self.show("gray16.png", png_image(level, W, H, 4), "image/png") as s:
            self.assert_shows(s, expected(level, 4))

    def test_palette_png_is_reduced(self):
        colors = [(0, 0, 0), (255, 255, 255)]
        data = png_palette(lambda x, y: colors[0] if seven(x, y) else colors[1], colors, W, H)
        with self.show("palette.png", data, "image/png") as s:
            self.assert_shows(s, expected(digit(0, 1), 1))

    def test_truecolor_png_is_reduced(self):
        data = png_rgb(lambda x, y: (0, 0, 0) if seven(x, y) else (255, 255, 255), W, H)
        with self.show("rgb.png", data, "image/png") as s:
            self.assert_shows(s, expected(digit(0, 1), 1))

    def test_portrait_png_is_rotated(self):
        px = panel_number("7")
        level = lambda x, y: 0 if px(y, H - 1 - x) else 1  # noqa: E731  (the digit, turned)
        with self.show("portrait.png", png_image(level, H, W, 1), "image/png") as s:
            screen = s.screenshot()
            self.assertGreater(len(screen), 0)

    def test_larger_png_is_cropped(self):
        px = panel_number("7")
        level = lambda x, y: 0 if x < W and y < H and px(x, y) else 1  # noqa: E731
        with self.show("large.png", png_image(level, W + 200, H + 120, 1), "image/png") as s:
            self.assert_shows(s, expected(digit(0, 1), 1))

    def test_corrupt_png_is_not_drawn(self):
        data = png_image(digit(0, 1), W, H, 1)
        with self.show("corrupt.png", data[:40] + bytes(len(data) - 40), "image/png") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_same_png_again_is_not_downloaded_or_redrawn(self):
        level = digit(0, 1)
        with self.show("again.png", png_image(level, W, H, 1), "image/png") as s:
            refreshes = s.status()["display_refreshes"]
            n = len(self.device().mock.requests)
            s.wake()
            self.device().mock.wait_for_request("/api/display", after=n, timeout_s=90)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90, settle_ms=200)
            self.assertEqual([r.path for r in self.device().mock.requests[n:]], ["/api/display"])
            self.assertEqual(s.status()["display_refreshes"], refreshes)

    def test_new_version_of_a_plugin_image_replaces_the_cached_one(self):
        m = self.device().mock
        old, new = png_image(digit(0, 1, "1"), W, H, 1), png_image(digit(0, 1, "2"), W, H, 1)
        m.set_file("/img/v1.png", "image/png", old)
        m.set_file("/img/v2.png", "image/png", new)
        m.display_queue = [{"image_url": m.device_url + "/img/v1.png", "filename": "plugin-abc123-1000", "refresh_rate": 300}]
        m.display = {"image_url": m.device_url + "/img/v2.png", "filename": "plugin-abc123-2000", "refresh_rate": 300}
        with self.device().boot() as s:
            m.wait_for_request("/img/v1.png", timeout_s=15)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            c = s.status()["console_total"]
            s.wake()
            s.wait(console=r"Deleting older version of plugin image", since=c, timeout_s=15)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.assert_shows(s, expected(digit(0, 1, "2"), 1))

    def test_long_filenames_are_shortened(self):
        level = digit(0, 1)
        path = self.serve("long.png", png_image(level, W, H, 1), "image/png")
        self.device().mock.display["filename"] = "mashup-066cc3-weather-and-calendar-1771674964"
        with self.device().boot() as s:
            self.device().mock.wait_for_request(path, timeout_s=15)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.assert_shows(s, expected(level, 1))

    def test_temperature_profile_is_saved(self):
        level = digit(0, 1)
        data = png_image(level, W, H, 1)
        with self.show("temp.png", data, "image/png", temperature_profile="a") as s:
            self.assert_shows(s, expected(level, 1))
            self.serve("temp2.png", png_image(digit(0, 1, "8"), W, H, 1), "image/png", temperature_profile="b")
            n = len(self.device().mock.requests)
            s.wake()
            self.device().mock.wait_for_request("/img/temp2.png", after=n, timeout_s=90)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
            self.assert_shows(s, expected(digit(0, 1, "8"), 1))

    def test_maximum_compatibility_forces_full_refreshes(self):
        level = digit(0, 1)
        with self.show("compat.png", png_image(level, W, H, 1), "image/png", maximum_compatibility=True) as s:
            self.assert_shows(s, expected(level, 1))

    def test_long_refresh_rates_use_fast_instead_of_partial_refreshes(self):
        with self.show("slow1.png", png_image(digit(0, 1), W, H, 1), "image/png", refresh_rate=3600) as s:
            eight = digit(0, 1, "8")
            self.serve("slow2.png", png_image(eight, W, H, 1), "image/png", refresh_rate=3600)
            n = len(self.device().mock.requests)
            s.wake()
            self.device().mock.wait_for_request("/img/slow2.png", after=n, timeout_s=90)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
            self.assert_shows(s, expected(eight, 1))


class Jpeg(Case):
    def test_jpeg_is_dithered_to_1bit(self):
        with self.show("five.jpg", (DATA / f"five_{W}x{H}.jpg").read_bytes(), "image/jpeg") as s:
            # dithering edges: up to 2% of an 800x480 screen, as many pixels on other panels
            # (a share of a big panel would let a blank screen pass)
            self.assert_shows(s, expected(digit(0, 1, "5"), 1), tolerance=64, max_ratio=min(0.02, 7680 / (W * H)))

    def test_jpeg_of_the_wrong_size_is_refused(self):
        with self.show("small.jpg", (DATA / "five_640x480.jpg").read_bytes(), "image/jpeg") as s:  # no panel is 640x480
            self.assertNotEqual(s.status()["state"], "halted")


class Bmp(Case):
    @unittest.expectedFailure
    def test_bmp_with_an_inverted_palette(self):
        # parseBMPHeader recognizes the white/black palette ("Color scheme reversed") and
        # sets image_reverse, but nothing uses it (display_show_image's inversion is under
        # #ifdef FUTURE): the image is shown inverted.
        level = digit(0, 1)
        # index 0 = white, 1 = black
        data = bmp(palette=bytes([255, 255, 255, 0, 0, 0, 0, 0]), black=lambda x, y: not seven(x, y))
        with self.show("inverted.bmp", bytes(data), "image/bmp") as s:
            self.assert_shows(s, expected(level, 1))

    def test_not_a_bmp(self):
        with self.show("garbage.bmp", b"XX" + bytes(len(bmp()) - 2), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_bmp_of_the_wrong_size(self):
        with self.show("small.bmp", bmp(W // 2, H // 2), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_bmp_with_a_color_palette(self):
        with self.show("red.bmp", bmp(palette=bytes([0, 0, 255, 0, 255, 255, 255, 0])), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_bmp_with_a_bad_data_offset(self):
        with self.show("offset.bmp", bmp(offset=100), "image/bmp") as s:
            self.assertNotEqual(s.status()["state"], "halted")


class Refused(Case):
    def test_image_too_large_to_download(self):
        path = self.serve("huge.png", bytes(DEVICE.max_image + 5000), "image/png")
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
    ENV = "trmnl_4clr"
    device = staticmethod(bwry)

    def setUp(self):
        if not (BWRY_BUILD / "firmware.elf").exists():
            self.skipTest(f"no TRMNL BWRY build at {BWRY_BUILD}")
        super().setUp()

    def test_1bit_png(self):
        level = og_digit(0, 1)
        with self.show("one.png", png_image(level, 800, 480, 1), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 1))

    def test_8bit_gray_png(self):
        level = og_digit(0, 255)
        with self.show("gray8.png", png_image(level, 800, 480, 8), "image/png") as s:
            self.assert_shows(s, expected_gray(level, 800, 480, 8))

    @unittest.expectedFailure
    def test_truecolor_png(self):
        # trmnl_4clr doesn't set PNG_MAX_BUFFERED_PIXELS (trmnl does), so PNGdec's default
        # row buffer is too small for 800 px truecolor rows: the decode corrupts the heap
        # (store fault in tlsf_free), and the device crashes, restarts, downloads the same
        # image and crashes again, until the server sends something else.
        data = png_rgb(lambda x, y: (255, 0, 0) if og_seven(x, y) else (255, 255, 255))
        with self.show("rgb.png", data, "image/png") as s:
            self.assertNotEqual(s.status()["state"], "halted")

    def test_jpeg(self):
        with self.show("five.jpg", (DATA / "five_800x480.jpg").read_bytes(), "image/jpeg") as s:
            self.assertNotEqual(s.status()["state"], "halted")


def tearDownModule():
    close_fixtures()


if __name__ == "__main__":
    unittest.main()
