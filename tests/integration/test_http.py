"""HTTP details on the device under test (see devices.py): redirects, bodies without a
Content-Length, and the error log (/api/log) when submitting fails. Images are served the
way the TRMNL server serves the device (support.device_image)."""

import unittest

from support import (ProvisionedDevice, close_fixtures, device_image, device_mock, fixture, panel_number,
                     served_path, sim)

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

# Firmware bugs, by the device they show on (see run.py's DeviceLoader).
SSD16XX_BMP = (
    "display_show_image (display.cpp:1947) writes a 1-bit BMP to the SSD16xx's new-image RAM only "
    "(writePlane() = PLANE_BOTH, no second plane) and asks for a partial refresh, which is differential "
    "against the old-image RAM; after the full refresh of the setup screens (display_show_msg2: "
    "PLANE_0 only, display.cpp:2835) that RAM doesn't hold the picture on screen, so only scraps of the "
    "BMP show (on the Sticky, whose panel supply is off in deep sleep, that RAM holds nothing; see "
    "test_byod_ssd.SsdBoard.test_bmp_after_a_fast_refresh)")
CROWPANEL_1BIT_PNG = (
    "png_to_epd (display.cpp:1764) passes the CrowPanel's dpList product number (EPD_CROWPANEL42 = 8) "
    "to bbep.setPanelType for 1-bit PNGs: panel type EP295_128x296_4GRAY, so the picture never shows "
    "(see test_images.CROWPANEL_1BIT_PNG)")

GEN2_4CLR = (
    "trmnl_gen2_4clr lacks BOARD_TRMNL_4CLR, so images don't take the 4-color path (png_draw_4clr): "
    "only part of the screen changes, in the wrong inks (see test_images.GEN2_4CLR)")

SHOWN_IMAGE_TESTS = ["Redirects.test_image_redirect", "NoContentLength.test_chunked_image"]
KNOWN_FAILURES = {
    # the 800x480 black-and-white SSD16xx boards get BMPs (the Waveshare's picture would also be a
    # row too high, see test_images.EP397_ROW_SHIFT)
    **{env: dict.fromkeys(SHOWN_IMAGE_TESTS, SSD16XX_BMP)
       for env in ("xteink_x4", "TRMNL_4inch26_DIY_Kit", "seeed_sticky", "WAVESHARE_397")},
    "CrowPanel42": dict.fromkeys(SHOWN_IMAGE_TESTS, CROWPANEL_1BIT_PNG),
    "trmnl_gen2_4clr": dict.fromkeys(SHOWN_IMAGE_TESTS, GEN2_4CLR),
}

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

dev = fixture(ProvisionedDevice)


def image_key(path: str) -> str:
    """The MockTrmnl.images key of an image path (BMPs are stored without their extension)."""
    name = path.removeprefix("/images/")
    return name.removesuffix(".bmp")


def tearDownModule():
    close_fixtures()


class Case(unittest.TestCase):
    def assertMatch(self, result: dict):
        self.assertTrue(result["match"], result)

    def setUp(self):
        m = dev().mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def refresh(self, s):
        """Wake the (sleeping) device for a refresh; returns the new requests' paths."""
        n = len(dev().mock.requests)
        s.wake()
        dev().mock.wait_for_request("/api/display", after=n, timeout_s=15)
        s.wait(state="deep_sleep", display_idle=True, timeout_s=15, settle_ms=300)
        return [r.path for r in dev().mock.requests[n:]]


class Redirects(Case):
    def test_api_display_redirect_to_a_relative_location(self):
        dev().mock.set_fault("/api/display", redirect="/api/display?moved=1", times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.assertEqual(self.refresh(s).count("/api/display"), 2)

    def test_api_display_permanent_redirect_to_an_absolute_url(self):
        m = dev().mock
        m.set_fault("/api/display", redirect=m.device_url + "/api/display?v=2", status=308, times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.assertEqual(self.refresh(s).count("/api/display"), 2)

    def test_image_redirect(self):
        m = dev().mock
        path, seven = device_image(m, "seven", panel_number("7"))
        moved = path.replace("seven", "moved")
        m.images[image_key(moved)] = m.images[image_key(path)]
        m.display = {"image": "seven", "refresh_rate": 300}
        m.set_fault(path, redirect=moved, times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.assertIn(moved, self.refresh(s))
            self.assertMatch(s.compare_screen(seven, tolerance=64, max_ratio=0))


class NoContentLength(Case):
    def test_chunked_image(self):
        m = dev().mock
        path, eight = device_image(m, "eight", panel_number("8"))
        m.display = {"image": "eight", "refresh_rate": 300}
        m.set_fault(path, chunked=True)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.refresh(s)
            self.assertMatch(s.compare_screen(eight, tolerance=64, max_ratio=0))

    def test_chunked_image_cut_short_is_not_shown(self):
        m = dev().mock
        path, _ = device_image(m, "nine", panel_number("9"))
        m.display = {"image": "nine", "refresh_rate": 300}
        m.set_fault(path, chunked=True, truncate=len(m.images[image_key(path)]) // 2)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            screen = s.screenshot()
            self.refresh(s)
            self.assertMatch(s.compare_screen(screen, tolerance=0, max_ratio=0))

    def test_chunked_api_display_answer(self):
        dev().mock.set_fault("/api/display", chunked=True)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.assertIn(served_path(dev().mock, "default"), self.refresh(s))


class ErrorLog(Case):
    def test_log_submission_follows_a_redirect(self):
        m = dev().mock
        m.set_fault("/images/*", status=404)
        m.set_fault("/api/log", redirect="/api/log?moved=1", times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            self.assertEqual(self.refresh(s).count("/api/log"), 2)

    def test_logs_are_kept_while_the_log_endpoint_fails(self):
        m = dev().mock
        m.set_fault("/images/*", status=404)
        m.set_fault("/api/log", status=500)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", display_idle=True, timeout_s=15)
            for _ in range(8):  # more errors than there are log slots
                self.refresh(s)
            m.clear_faults()
            paths = self.refresh(s)
            self.assertIn("/api/log", paths)
            logs = [r for r in m.requests if r.path == "/api/log" and r.body]
            self.assertGreater(len(logs[-1].json()["logs"]), 1)


class SetupRedirect(unittest.TestCase):
    def test_setup_logo_redirect(self):
        self.onboard_with_logo_redirect(absolute=True)

    @unittest.expectedFailure
    def test_setup_logo_redirect_to_a_relative_location(self):
        # DeviceSetup::downloadSetupImage follows a 307/308 with begin(getLocation()), without
        # resolving a relative Location against the image URL (HttpRetryRequest does, with
        # resolveRedirectLocation): the second GET fails and onboarding reports an error.
        self.onboard_with_logo_redirect(absolute=False)

    def onboard_with_logo_redirect(self, absolute: bool):
        # (/api/setup's logo is the mock's 800x480 BMP on every device)
        with device_mock() as mock:
            mock.images["moved"] = mock.images["default"]
            location = (mock.device_url if absolute else "") + "/images/moved.bmp"
            mock.set_fault("/images/default.bmp", redirect=location, times=1)
            with sim(erase=True, extra_args=("--offline",)) as s:
                s.wait(portal=True, timeout_s=15)
                s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
                mock.wait_for_request("/images/moved.bmp", timeout_s=15)
                mock.wait_for_request("/api/display", timeout_s=15)
                s.wait(state="deep_sleep", display_idle=True, timeout_s=15)


if __name__ == "__main__":
    unittest.main()
