"""The simulator's built-in mock TRMNL server (`/mock/...` control API) instead of trmnl_mock.py:
onboarding against it, converted images on screen, switching images, OTA files."""

import unittest
import urllib.parse
import urllib.request

from support import BUILD, BWRY_BUILD, sim, big_number
from trmnl_mock import color_bars, expected_bwry, png_gray, png_rgb

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)


def onboard(s, refresh_rate: int = 300) -> str:
    """Start the built-in server and onboard the fresh device `s` against it."""
    url = s.mock.start()
    s.mock.display(refresh_rate=refresh_rate)
    s.wait(portal=True, timeout_s=90)
    s.portal_connect("TRMNL-Sim", "password", server=url)
    return url


class BuiltinServerOg(unittest.TestCase):
    def test_onboards_and_shows_uploaded_images(self):
        with sim(erase=True, extra_args=("--offline",)) as s:
            # An 8-bit gray PNG: converted to the OG's 1-bit BMP without changing a pixel.
            reference = png_gray(big_number("7"))
            info = s.mock.add_image("seven", reference, current=True)
            self.assertTrue(info["filename"].startswith("plugin-"), info)
            url = onboard(s)
            self.assertRegex(url, r"^http://10\.0\.2\.2:\d+$")

            setup = s.mock.wait_for_request("/api/setup", timeout_s=120)
            self.assertEqual(setup["headers"]["ID"], "7C:DF:A1:00:00:01")
            req = s.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req["headers"]["Access-Token"], "sim-test-api-key")
            s.mock.wait_for_request("/images/seven.bmp", timeout_s=120)
            st = s.wait(state="deep_sleep", timeout_s=120)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 300, delta=15)
            self.assertTrue(s.compare_screen(s.mock.expected("seven"), tolerance=64, max_ratio=0)["match"])
            self.assertTrue(s.compare_screen(reference, tolerance=64, max_ratio=0)["match"])

            # Switch the image and wake the device: the next request fetches it.
            cursor = s.mock.state()["total_requests"]
            s.mock.add_image("eight", png_gray(big_number("8")))
            s.mock.display(image="eight")
            s.wake()
            s.mock.wait_for_request("/images/eight.bmp", after=cursor, timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
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
