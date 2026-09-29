"""The simulator's built-in mock TRMNL server (`/mock/...` control API) instead of trmnl_mock.py:
onboarding against it, converted images on screen, switching images, OTA files."""

import time
import unittest
import urllib.parse
import urllib.request

from support import BUILD, BWRY_BUILD, DEVICE, panel_number, sim
from trmnl_sim import SimError
from trmnl_mock import color_bars, expected_bwry, expected_gray, png_image, png_rgb

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

# Firmware bugs, by the device they show on (see run.py's DeviceLoader).
SHOWS = "BuiltinServerOg.test_onboards_and_shows_uploaded_images"
KNOWN_FAILURES = {
    # the built-in server serves 800x480 black-and-white panels the OG's 1-bit BMP
    **{env: {SHOWS: "a 1-bit BMP after the setup screens' full refresh shows only scraps on SSD16xx panels "
                    "(see test_http.SSD16XX_BMP)"}
       for env in ("xteink_x4", "TRMNL_4inch26_DIY_Kit", "seeed_sticky", "WAVESHARE_397")},
    "CrowPanel42": {SHOWS: "1-bit PNGs never show on the CrowPanel (see test_images.CROWPANEL_1BIT_PNG)"},
    "trmnl_gen2_4clr": {SHOWS: "images don't take the 4-color path without BOARD_TRMNL_4CLR "
                               "(see test_images.GEN2_4CLR)"},
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

    def assertMatch(self, result: dict):
        self.assertTrue(result["match"], result)

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
            self.assertMatch(s.compare_screen(s.mock.expected("seven"), tolerance=64, max_ratio=0))
            self.assertMatch(s.compare_screen(reference, tolerance=64, max_ratio=0))

            # Switch the image and wake the device: the next request fetches it.
            cursor = s.mock.state()["total_requests"]
            info = s.mock.add_image("eight", black_and_white("8")[0])
            s.mock.display(image="eight")
            s.wake()
            s.mock.wait_for_request(info["path"], after=cursor, timeout_s=120)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            self.assertMatch(s.compare_screen(s.mock.expected("eight"), tolerance=64, max_ratio=0))

    def test_http_faults_are_survived(self):
        # scripts/mock_server.py's failures, one wake each: the device gets the fault, sleeps
        # without crashing, and shows the image once the server is healthy again.
        faults = [("display", "500"), ("display", "reset"), ("display", "close"), ("display", "bad-json"),
                  ("display", "timeout=2"), ("image", "truncate"), ("image", "garbage"), ("image", "empty"),
                  ("image", "reset"), ("image", "slow=1024,20")]
        data, reference = black_and_white("7")
        with sim(erase=True, extra_args=("--offline",)) as s:
            info = s.mock.add_image("seven", data, current=True)
            onboard(s)
            s.mock.wait_for_request(info["path"], timeout_s=120)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            for route, spec in faults:
                with self.subTest(route=route, fault=spec):
                    # a new version of the image, so the device downloads it again
                    s.mock.add_image("seven", data, current=True)
                    self.assertEqual(s.mock.faults(**{route: [f"{spec}:1"]})[route], [f"{spec}:1"])
                    cursor = s.mock.state()["total_requests"]
                    boots = s.status()["boot_count"]
                    s.wake()
                    hit = self.wait_for_fault(s, cursor)
                    self.assertTrue(hit["summary"].startswith(f"fault {spec}"), hit)
                    st = s.wait(state="deep_sleep", display_idle=True, timeout_s=120)["status"]
                    # waking from deep sleep is one boot; a crash would be another
                    self.assertEqual(st["boot_count"], boots + 1, f"the device restarted after {route} {spec}")
                    self.assertEqual(s.mock.state()["faults"][route], [], "used up")
            s.mock.faults(display=["503"])
            s.mock.clear_faults()
            self.assertEqual(s.mock.state()["faults"], {"display": [], "image": []})
            info = s.mock.add_image("seven", data, current=True)
            cursor = s.mock.state()["total_requests"]
            s.wake()
            s.mock.wait_for_request(info["path"], after=cursor, timeout_s=120)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            self.assertMatch(s.compare_screen(reference, tolerance=64, max_ratio=0))

    def wait_for_fault(self, s, after: int, timeout_s: float = 120) -> dict:
        """The first request from index `after` on that a fault answered."""
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            hit = next((r for r in s.mock.requests(after) if r["summary"].startswith("fault ")), None)
            if hit:
                return hit
            time.sleep(0.2)
        self.fail(f"no request met the fault: {[(r['path'], r['summary']) for r in s.mock.requests(after)]}")

    def test_bad_fault_specs_are_refused(self):
        with sim(extra_args=("--offline",)) as s:
            for body in ({"display": ["truncate"]}, {"image": ["600"]}, {"display": ["500:0"]}, {"nope": ["500"]},
                         {"display": ["500", "wat"]}):
                with self.subTest(body=body), self.assertRaises(SimError):
                    s._post("/mock/faults", body)
            self.assertEqual(s.mock.state()["faults"], {"display": [], "image": []}, "nothing added")

    def test_serves_this_build_s_firmware_for_ota(self):
        with sim(extra_args=("--offline",)) as s:
            port = urllib.parse.urlparse(s.mock.start()).port
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/firmware.bin", timeout=30) as r:
                self.assertEqual(r.read(), (BUILD / "firmware.bin").read_bytes())
            url = s.mock.set_file("/blob.bin", b"x" * 100_000)
            self.assertEqual(url, f"http://10.0.2.2:{port}/blob.bin")


class BuiltinServerBwry(unittest.TestCase):
    ENV = "trmnl_4clr"
    assertMatch = BuiltinServerOg.assertMatch
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
            self.assertMatch(s.compare_screen(s.mock.expected("bars"), tolerance=16, max_ratio=0))
            self.assertMatch(s.compare_screen(expected_bwry(color_bars), tolerance=16, max_ratio=0))


if __name__ == "__main__":
    unittest.main()
