"""Image formats on the TRMNL X (1872x1404, 16 gray levels): PNG depths, portrait images,
JPEG, BMP, and the image cache."""

import unittest

from support import HERE, close_fixtures, fixture
from support_x import ProvisionedX, ShippedX, X_BUILD
from trmnl_mock import expected_gray, png_image
from test_trmnl_x import digits

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

DATA = HERE / "data"
shipped = fixture(ShippedX)
dev = fixture(lambda: ProvisionedX(shipped()))
W, H = 1872, 1404


def setUpModule():
    if not (X_BUILD / "firmware.elf").exists():
        raise unittest.SkipTest(f"no TRMNL_X build at {X_BUILD} (set TRMNL_X_BUILD)")


def tearDownModule():
    close_fixtures()


def levels(black: int, white: int, text: str = "7"):
    px = digits(text)
    return lambda x, y: black if px(x, y) == 0 else white


class Case(unittest.TestCase):
    def setUp(self):
        m = dev().mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def show(self, name: str, data: bytes, content_type: str):
        m = dev().mock
        url = m.set_file(f"/img/{name}", content_type, data)
        m.display = {"image_url": url, "filename": f"plugin-{abs(hash(name)) % 999999:06d}-1000", "refresh_rate": 300}
        s = dev().boot_asleep()
        try:
            s.wait(state="deep_sleep", timeout_s=15)
            s.wake()
            m.wait_for_request(f"/img/{name}", timeout_s=15)
            st = s.wait(state="deep_sleep", timeout_s=15, settle_ms=300)["status"]
            self.assertNotEqual(st["state"], "halted")
        except BaseException:
            s.close()
            raise
        return s


class XPng(Case):
    def test_2bit_png(self):
        level = levels(0, 3)
        with self.show("gray4.png", png_image(level, W, H, 2), "image/png") as s:
            self.assertTrue(s.compare_screen(expected_gray(level, W, H, 2), tolerance=64, max_ratio=0.001)["match"])

    def test_8bit_png(self):
        level = levels(0, 255)
        with self.show("gray8.png", png_image(level, W, H, 8), "image/png") as s:
            self.assertTrue(s.compare_screen(expected_gray(level, W, H, 8), tolerance=64, max_ratio=0.001)["match"])

    def test_portrait_1bit_png(self):
        px = digits("7")
        level = lambda x, y: 0 if px(y, W - 1 - x) == 0 else 1  # noqa: E731
        with self.show("portrait1.png", png_image(level, H, W, 1), "image/png"):
            pass

    def test_portrait_4bit_png(self):
        px = digits("7")
        level = lambda x, y: 0 if px(y, W - 1 - x) == 0 else 15  # noqa: E731
        with self.show("portrait4.png", png_image(level, H, W, 4), "image/png"):
            pass


class XJpeg(Case):
    def test_jpeg_is_dithered_to_16_grays(self):
        with self.show("five.jpg", (DATA / "five_1872x1404.jpg").read_bytes(), "image/jpeg") as s:
            result = s.compare_screen(expected_gray(levels(0, 1, "5"), W, H, 1), tolerance=96, max_ratio=0.05)
            self.assertTrue(result["match"], result)


class XCache(Case):
    def test_same_image_again_is_not_downloaded_or_redrawn(self):
        m = dev().mock
        m.set_png("one", digits("1"))
        m.display = {"image": "one", "refresh_rate": 300}
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            s.wake()
            m.wait_for_request("/images/one.png", timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=300)
            refreshes = s.status()["display_refreshes"]
            n = len(m.requests)
            s.wake()
            m.wait_for_request("/api/display", after=n, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=300)
            self.assertEqual([r.path for r in m.requests[n:]], ["/api/display"])
            self.assertEqual(s.status()["display_refreshes"], refreshes)


if __name__ == "__main__":
    unittest.main()
