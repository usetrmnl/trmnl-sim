"""HTTP details on the TRMNL OG: redirects, bodies without a Content-Length, and the
error log (/api/log) when submitting fails."""

import unittest

from support import MockTrmnl, ProvisionedDevice, big_number, close_fixtures, device_image, fixture, image_size, sim

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

dev = fixture(ProvisionedDevice)


def tearDownModule():
    close_fixtures()


class Case(unittest.TestCase):
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
        s.wait(state="deep_sleep", timeout_s=15, settle_ms=300)
        return [r.path for r in dev().mock.requests[n:]]


class Redirects(Case):
    def test_api_display_redirect_to_a_relative_location(self):
        dev().mock.set_fault("/api/display", redirect="/api/display?moved=1", times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            self.assertEqual(self.refresh(s).count("/api/display"), 2)

    def test_api_display_permanent_redirect_to_an_absolute_url(self):
        m = dev().mock
        m.set_fault("/api/display", redirect=m.device_url + "/api/display?v=2", status=308, times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            self.assertEqual(self.refresh(s).count("/api/display"), 2)

    def test_image_redirect(self):
        m = dev().mock
        path, seven = device_image(m, "seven", big_number("7"))
        key = path[len("/images/"):].removesuffix(".bmp")  # seven (BMP) or seven.png
        m.images[key.replace("seven", "moved")] = m.images[key]
        m.display = {"image": "seven", "refresh_rate": 300}
        m.set_fault(path, redirect=path.replace("seven", "moved"), times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            self.assertIn(path.replace("seven", "moved"), self.refresh(s))
            self.assertTrue(s.compare_screen(seven, tolerance=64, max_ratio=0)["match"])


class NoContentLength(Case):
    def test_chunked_image(self):
        m = dev().mock
        path, eight = device_image(m, "eight", big_number("8"))
        m.display = {"image": "eight", "refresh_rate": 300}
        m.set_fault(path, chunked=True)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            self.refresh(s)
            self.assertTrue(s.compare_screen(eight, tolerance=64, max_ratio=0)["match"])

    def test_chunked_image_cut_short_is_not_shown(self):
        m = dev().mock
        path, _ = device_image(m, "nine", big_number("9"))
        m.display = {"image": "nine", "refresh_rate": 300}
        m.set_fault(path, chunked=True, truncate=min(10000, image_size(m, path) // 2))
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            screen = s.screenshot()
            self.refresh(s)
            self.assertTrue(s.compare_screen(screen, tolerance=0, max_ratio=0)["match"])

    def test_chunked_api_display_answer(self):
        dev().mock.set_fault("/api/display", chunked=True)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            self.assertIn(dev().mock.image_path("default"), self.refresh(s))


class ErrorLog(Case):
    def test_log_submission_follows_a_redirect(self):
        m = dev().mock
        m.set_fault("/images/*", status=404)
        m.set_fault("/api/log", redirect="/api/log?moved=1", times=1)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            self.assertEqual(self.refresh(s).count("/api/log"), 2)

    def test_logs_are_kept_while_the_log_endpoint_fails(self):
        m = dev().mock
        m.set_fault("/images/*", status=404)
        m.set_fault("/api/log", status=500)
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
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
        with MockTrmnl() as mock:
            mock.images["moved"] = mock.images["default"]
            location = (mock.device_url if absolute else "") + "/images/moved.bmp"
            mock.set_fault("/images/default.bmp", redirect=location, times=1)
            with sim(erase=True, extra_args=("--offline",)) as s:
                s.wait(portal=True, timeout_s=15)
                s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
                mock.wait_for_request("/images/moved.bmp", timeout_s=15)
                mock.wait_for_request("/api/display", timeout_s=15)
                s.wait(state="deep_sleep", timeout_s=15)


if __name__ == "__main__":
    unittest.main()
