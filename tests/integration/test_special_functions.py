"""Special functions (TRMNL OG): the server assigns one in an /api/display answer, and a
double click (or a 1-5 s press) of the button runs it on the next wake. Also the
/api/display status codes and actions around it."""

import unittest

from support import ProvisionedDevice, big_number, close_fixtures, device_image, fixture, needs, slow

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

    def assign(self, s, function: str, image: str = "default"):
        """Boot answers assign `function`; returns once the device sleeps with it saved."""
        dev().mock.display = {"image": image, "refresh_rate": 300, "special_function": function}
        dev().mock.wait_for_request("/api/display", timeout_s=90)
        s.wait(state="deep_sleep", timeout_s=90)

    def run_function(self, s, answer: dict, press_ms: int = 1500):
        """Press the button (a medium press counts as a double click) and answer the
        special-function request with `answer`; returns that request."""
        dev().mock.display = answer
        n = len(dev().mock.requests)
        s.press(press_ms)
        req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=120)
        self.assertEqual(req.headers.get("special_function"), "true")
        return req


@needs("double_click")  # runs the special function
class SpecialFunctions(Case):
    def test_identify_shows_the_identify_image(self):
        seven = dev().mock.set_image("seven", big_number("7"))
        with dev().boot() as s:
            self.assign(s, "identify")
            self.run_function(s, {"image": "seven", "action": "identify", "refresh_rate": 300})
            dev().mock.wait_for_request("/images/seven.bmp", timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            self.assertTrue(s.compare_screen(seven, tolerance=64, max_ratio=0)["match"])

    def test_sleep_uses_the_answered_refresh_rate(self):
        with dev().boot() as s:
            self.assign(s, "sleep")
            self.run_function(s, {"image": "default", "action": "sleep", "refresh_rate": 1800})
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 1800, delta=30)

    @unittest.expectedFailure
    def test_sleep_keeps_the_screen(self):
        # Like send_to_me (see there): status=false/HTTPS_SUCCESS makes the OG look for the
        # answer's image in a cache it doesn't have, then report and show an error.
        with dev().boot() as s:
            self.assign(s, "sleep")
            screen = s.screenshot()
            self.run_function(s, {"image": "default", "action": "sleep", "refresh_rate": 1800})
            s.wait(state="deep_sleep", timeout_s=90)
            self.assertEqual(dev().mock.count("/api/log"), 0)
            self.assertTrue(s.compare_screen(screen, tolerance=0, max_ratio=0)["match"])

    def test_sleep_without_the_sleep_action_is_ignored(self):
        with dev().boot() as s:
            self.assign(s, "sleep")
            self.run_function(s, {"image": "default", "action": "nothing", "refresh_rate": 1800})
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 300, delta=30)

    def test_add_wifi_opens_the_portal_first(self):
        with dev().boot() as s:
            self.assign(s, "add_wifi")
            dev().mock.display = {"image": "default", "action": "add_wifi", "refresh_rate": 300}
            n = len(dev().mock.requests)
            s.press(1500)
            s.wait(portal=True, timeout_s=120)
            s.portal_connect("TRMNL-Sim", "password", server=dev().mock.device_url)
            req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=120)
            self.assertEqual(req.headers.get("special_function"), "true")
            s.wait(state="deep_sleep", timeout_s=120)

    def test_restart_playlist_shows_the_first_item(self):
        one = dev().mock.set_image("one", big_number("1"))
        with dev().boot() as s:
            self.assign(s, "restart_playlist")
            self.run_function(s, {"image": "one", "action": "restart_playlist", "refresh_rate": 300})
            dev().mock.wait_for_request("/images/one.bmp", timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])

    @unittest.expectedFailure
    def test_send_to_me_keeps_the_current_image(self):
        # After showing /current.bmp, send_to_me leaves status=false/HTTPS_SUCCESS, so
        # downloadAndShow() takes the "image already cached" path with the answer's filename.
        # The OG doesn't cache BMP images under their filename (only PNG and JPEG), so with a
        # BMP it is always "empty or unreadable": the device submits an error log and draws an error message
        # over the image it just showed.
        two = dev().mock.set_image("two", big_number("2"))
        with dev().boot() as s:
            self.assign(s, "send_to_me", image="two")
            self.run_function(s, {"image": "two", "action": "send_to_me", "refresh_rate": 300})
            s.wait(state="deep_sleep", timeout_s=90)
            self.assertEqual(dev().mock.count("/api/log"), 0)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])

    def test_guest_mode_shows_the_guest_image_for_its_refresh_rate(self):
        three = dev().mock.set_image("three", big_number("3"))
        with dev().boot() as s:
            self.assign(s, "guest_mode")
            self.run_function(s, {"image": "three", "action": "guest_mode", "refresh_rate": 1200})
            dev().mock.wait_for_request("/images/three.bmp", timeout_s=90)
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertTrue(s.compare_screen(three, tolerance=64, max_ratio=0)["match"])
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 1200, delta=30)

    @unittest.expectedFailure
    def test_rewind_without_a_previous_image_does_not_crash(self):
        # Rewind shows /last.bmp or /last.png, which no firmware code writes: it reads the
        # missing /last.png (display_read_file returns NULL), pretends it decoded
        # ("image_proccess_response = PNG_NO_ERR; // DEBUG") and passes NULL to
        # display_show_image.
        with dev().boot() as s:
            self.assign(s, "rewind")
            self.run_function(s, {"image": "default", "action": "rewind", "refresh_rate": 300})
            st = s.wait(state="deep_sleep", timeout_s=120)["status"]
            self.assertNotEqual(st["state"], "halted")
            self.assertEqual(st["boot_count"], 1, "the device restarted (crashed)")


@needs("double_click")  # runs the special function
class Identify(Case):
    def test_identify_with_the_empty_state_image(self):
        with dev().boot() as s:
            self.assign(s, "identify")
            self.run_function(s, {"image": "default", "filename": "empty_state", "action": "identify",
                                  "refresh_rate": 300})
            s.wait(state="deep_sleep", timeout_s=90)

    def test_restart_playlist_with_the_empty_state_image(self):
        with dev().boot() as s:
            self.assign(s, "restart_playlist")
            self.run_function(s, {"image": "default", "filename": "empty_state", "action": "restart_playlist",
                                  "refresh_rate": 300})
            s.wait(state="deep_sleep", timeout_s=90)

    def test_guest_mode_with_the_empty_state_image(self):
        with dev().boot() as s:
            self.assign(s, "guest_mode")
            self.run_function(s, {"image": "default", "filename": "empty_state", "action": "guest_mode",
                                  "refresh_rate": 300})
            s.wait(state="deep_sleep", timeout_s=90)

    def test_unregistered_status_during_a_special_function(self):
        with dev().boot() as s:
            self.assign(s, "identify")
            self.run_function(s, {"image": "default", "status": 202, "refresh_rate": 300})
            s.wait(state="deep_sleep", timeout_s=90)

    def test_reset_status_during_a_special_function(self):
        # "status": 500 in an /api/display answer means "reset": forget WiFi and the API key.
        with dev().boot() as s:
            self.assign(s, "identify")
            self.run_function(s, {"image": "default", "status": 500, "refresh_rate": 300})
            s.wait(portal=True, min_boots=3, timeout_s=120)

    def test_other_actions_are_rejected(self):
        with dev().boot() as s:
            for function in ("identify", "add_wifi", "restart_playlist", "rewind", "send_to_me", "guest_mode"):
                with self.subTest(function):
                    dev().mock.display = {"image": "default", "refresh_rate": 300, "special_function": function}
                    n = len(dev().mock.requests)
                    s.wake()
                    dev().mock.wait_for_request("/api/display", after=n, timeout_s=90)
                    s.wait(state="deep_sleep", timeout_s=90, settle_ms=200)
                    if function == "add_wifi":
                        # Opens the portal first; joining again gets to the request.
                        dev().mock.display = {"image": "default", "action": "wrong", "refresh_rate": 300}
                        s.press(1500)
                        s.wait(portal=True, timeout_s=120)
                        s.portal_connect("TRMNL-Sim", "password", server=dev().mock.device_url)
                        dev().mock.wait_for_request("/api/display", after=n + 1, timeout_s=120)
                    else:
                        self.run_function(s, {"image": "default", "action": "wrong", "refresh_rate": 300})
                    st = s.wait(state="deep_sleep", timeout_s=120, settle_ms=200)["status"]
                    self.assertNotEqual(st["state"], "halted")


class ApiStatus(Case):
    def test_unregistered_status_polls_again_soon(self):
        dev().mock.display = {"image": "default", "status": 202, "refresh_rate": 300}
        with dev().boot() as s:
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertLess(st["wake_at_s"] - st["sim_time_s"], 300)

    def test_reset_status_forgets_the_device(self):
        dev().mock.display = {"image": "default", "status": 500, "refresh_rate": 300}
        with dev().boot() as s:
            s.wait(portal=True, min_boots=2, timeout_s=120)

    def test_empty_state_image_means_no_plugin_yet(self):
        dev().mock.display = {"image": "default", "filename": "empty_state", "refresh_rate": 300}
        with dev().boot() as s:
            dev().mock.wait_for_request("/api/display", timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev().mock.requests)
            s.wake()
            dev().mock.wait_for_request("/api/display", after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90, settle_ms=200)

    def test_reset_firmware_forgets_the_device(self):
        dev().mock.display = {"image": "default", "reset_firmware": True, "refresh_rate": 300}
        with dev().boot() as s:
            dev().mock.wait_for_request("/api/display", timeout_s=90)
            s.wait(portal=True, min_boots=2, timeout_s=120)

    @slow("the wiper runs 100 full refreshes; about 3.5 minutes on the TRMNL X")
    def test_screen_wiper_clears_then_shows_the_next_item(self):
        path, four = device_image(dev().mock, "four", big_number("4"))
        dev().mock.display_queue = [{"image": "default", "filename": "screen_wiper.png", "refresh_rate": 300}]
        dev().mock.display = {"image": "four", "refresh_rate": 300}
        with dev().boot() as s:
            dev().mock.wait_for_request(path, timeout_s=600)
            s.wait(state="deep_sleep", timeout_s=600)
            self.assertTrue(s.compare_screen(four, tolerance=64, max_ratio=0)["match"])

    @slow("the wiper runs 100 full refreshes; about 3.5 minutes on the TRMNL X")
    def test_screen_wiper_is_only_run_once_per_wake(self):
        dev().mock.display = {"image": "default", "filename": "screen_wiper.png", "refresh_rate": 300}
        with dev().boot() as s:
            s.wait(state="deep_sleep", timeout_s=600)
            self.assertEqual(dev().mock.count("/api/display"), 2)


class Buttons(Case):
    @needs("double_click")
    def test_double_click_runs_the_special_function(self):
        with dev().boot() as s:
            self.assign(s, "sleep")
            dev().mock.display = {"image": "default", "action": "sleep", "refresh_rate": 1800}
            n = len(dev().mock.requests)
            s.double_click(80, 150)
            req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=120)
            self.assertEqual(req.headers.get("special_function"), "true")
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 1800, delta=30)

    def test_short_tap_is_a_plain_button_wake(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(dev().mock.requests)
            s.press(20)
            req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=90)
            self.assertIsNone(req.headers.get("special_function"))
            s.wait(state="deep_sleep", timeout_s=90)

    @needs("double_click")  # the OG reads a press in the double-click window after the waking tap
    def test_tap_then_long_press_resets_wifi(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.press(20)
            s.press(6000)
            s.wait(portal=True, timeout_s=120)

    @needs("soft_reset_press")
    def test_very_long_press_is_a_soft_reset(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.press(16000)
            s.wait(portal=True, timeout_s=180)


if __name__ == "__main__":
    unittest.main()
