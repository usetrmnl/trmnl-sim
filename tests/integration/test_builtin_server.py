"""The simulator's built-in mock TRMNL server (`/mock/...` control API) instead of trmnl_mock.py:
onboarding against it, converted images on screen, switching images, OTA files."""

import unittest
import urllib.parse
import urllib.request

from support import BUILD, BWRY_BUILD, DEVICE, panel_number, sim
from trmnl_mock import color_bars, expected_bwry, expected_gray, png_image, png_rgb

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

# Firmware bugs, by the device they show on (see run.py's DeviceLoader).
KNOWN_FAILURES = {
    # the built-in server serves 800x480 black-and-white panels the OG's 1-bit BMP
    "xteink_x4": {"BuiltinServerOg.test_onboards_and_shows_uploaded_images":
                  "a 1-bit BMP after the setup screens' full refresh shows only scraps on SSD16xx panels "
                  "(see test_http.SSD16XX_BMP)"},
    "CrowPanel42": {"BuiltinServerOg.test_onboards_and_shows_uploaded_images":
                    "1-bit PNGs never show on the CrowPanel (see test_images.CROWPANEL_1BIT_PNG)"},
}


def onboard(s, refresh_rate: int = 300) -> str:
    """Start the built-in server and onboard the fresh device `s` against it."""
    url = s.mock.start()
    s.mock.display(refresh_rate=refresh_rate)
    s.wait(portal=True, timeout_s=90)
    s.portal_connect("TRMNL-Sim", "password", server=url)
    return url


def black_and_white(text: str) -> tuple[bytes, bytes]:
    """An 8-bit gray PNG of the panel's size with `text` in black on white, and the screenshot
    it should give (black and white are inks on every panel)."""
    px = panel_number(text)
    level = lambda x, y: 0 if px(x, y) else 255  # noqa: E731
    w, h = DEVICE.size
    return png_image(level, w, h, bits=8), expected_gray(level, w, h, bits=8)


class BuiltinServerOg(unittest.TestCase):
    """On the device under test (the class keeps the name it had when it ran on the OG only)."""

    def test_onboards_and_shows_uploaded_images(self):
        with sim(erase=True, extra_args=("--offline",)) as s:
            # A black-and-white 8-bit gray PNG of the panel's size: converted to what the panel
            # takes (the OG's 1-bit BMP, a 1-bit/4-bit gray or palette PNG) without changing a pixel.
            data, reference = black_and_white("7")
            info = s.mock.add_image("seven", data, current=True)
            self.assertTrue(info["filename"].startswith("plugin-"), info)
            url = onboard(s)
            self.assertRegex(url, r"^http://10\.0\.2\.2:\d+$")

            setup = s.mock.wait_for_request("/api/setup", timeout_s=120)
            self.assertEqual(setup["headers"]["ID"], "7C:DF:A1:00:00:01")
            req = s.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req["headers"]["Access-Token"], "sim-test-api-key")
            s.mock.wait_for_request(info["path"], timeout_s=120)
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=120)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 300, delta=15)
            self.assertTrue(s.compare_screen(s.mock.expected("seven"), tolerance=64, max_ratio=0)["match"])
            self.assertTrue(s.compare_screen(reference, tolerance=64, max_ratio=0)["match"])

            # Switch the image and wake the device: the next request fetches it.
            cursor = s.mock.state()["total_requests"]
            info = s.mock.add_image("eight", black_and_white("8")[0])
            s.mock.display(image="eight")
            s.wake()
            s.mock.wait_for_request(info["path"], after=cursor, timeout_s=120)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            self.assertTrue(s.compare_screen(s.mock.expected("eight"), tolerance=64, max_ratio=0)["match"])

    def test_serves_this_build_s_firmware_for_ota(self):
        with sim(extra_args=("--offline",)) as s:
            port = urllib.parse.urlparse(s.mock.start()).port
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/firmware.bin", timeout=30) as r:
                self.assertEqual(r.read(), (BUILD / "firmware.bin").read_bytes())
            url = s.mock.set_file("/blob.bin", b"x" * 100_000)
            self.assertEqual(url, f"http://10.0.2.2:{port}/blob.bin")


class BuiltinServerBwry(unittest.TestCase):
    ENV = "trmnl_4clr"
    @classmethod
    def setUpClass(cls):
        if not (BWRY_BUILD / "firmware.elf").exists():
            raise unittest.SkipTest(f"no trmnl_4clr build at {BWRY_BUILD} (set TRMNL_BWRY_BUILD)")

    def test_color_image_is_reduced_to_the_four_inks(self):
        with sim(BWRY_BUILD, erase=True, extra_args=("--offline",)) as s:
            # Without dithering, colors are classified like the firmware does.
            s.mock.add_image("bars", png_rgb(color_bars), current=True, dither=False)
            onboard(s)
            req = s.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req["headers"]["Model"], "og_4clr")
            s.mock.wait_for_request("/images/bars.png", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertTrue(s.compare_screen(s.mock.expected("bars"), tolerance=16, max_ratio=0)["match"])
            self.assertTrue(s.compare_screen(expected_bwry(color_bars), tolerance=16, max_ratio=0)["match"])


if __name__ == "__main__":
    unittest.main()
