"""Special functions (on the device under test): the server assigns one in an /api/display answer, and a
double click (or a 1-5 s press) of the button runs it on the next wake. Also the
/api/display status codes and actions around it."""

import unittest

from support import DEVICES, ProvisionedDevice, close_fixtures, device_image, device_number, fixture, needs

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

# Firmware bug on the SSD16xx boards: display_show_image() (display.cpp:1947) sends a 1-bit
# BMP with bbep.writePlane(), which without a second plane writes only the new-image RAM
# (0x24), and asks for a partial refresh, which drives only the pixels differing from the
# "old" RAM (0x26); that holds an earlier picture (or its inverse after a fast refresh), so
# the BMP comes out as a mix of the two. See test_byod_ssd.SsdBoard.test_bmp_after_a_fast_refresh.
SSD_BMP = "SSD16xx: a 1-bit BMP is refreshed partially against a stale old-image RAM (display.cpp:1947)"
CROWPANEL_PNG = ("CrowPanel: png_to_epd() calls bbep.setPanelType(dpList[...].OneBit) (display.cpp:1764) with the "
                 "bb_epaper product number the panel was begun with, selecting a 2.9\" 128x296 panel: 1-bit PNGs "
                 "never show (see test_byod_ssd.CrowPanel42)")
# Firmware bug: classify_button_presses() (button.cpp:62-92) times a press from when it starts
# reading the button, not from the wake. If the first click is still held then (the firmware
# took longer to boot than usual) but released within 50 ms, it is NoAction and the second
# click is never waited for.
SLOW_BOOT_CLICK = ("button.cpp:84-92: a first click still held when classify_button_presses() starts reading (here "
                   "~50 ms after the wake) but released within 50 ms counts as NoAction; the double click is lost")

# The sleep special function keeps the screen only where it isn't a BMP (see
# test_sleep_keeps_the_screen): BMPs aren't cached under their filename.
BMP_NOT_CACHED = ("sleep: status=false/HTTPS_SUCCESS takes the cached-image path (bl.cpp:1478), but BMPs are only "
                  "saved as /current.bmp: \"Cached image is empty or unreadable\", an error log and message")
# Firmware bug on the XIAO ESP32-C3: bl_init() waits 2 s (bl.cpp:731-733) before it reads the
# button, so a wake press is over by then, and classify_button_presses() (button.cpp:66-71)
# then waits for another press with no timeout: the device stays awake until pressed again.
XIAO_C3_BUTTON = ("XIAO C3: after bl.cpp:731-733's 2 s delay the wake press is over, and button.cpp:66-71 waits "
                  "for a new press forever (no timeout): a button wake never reaches the server")
KNOWN_FAILURES = {
    "xteink_x4": {
        "SpecialFunctions.test_identify_shows_the_identify_image": SSD_BMP,
        "SpecialFunctions.test_restart_playlist_shows_the_first_item": SSD_BMP,
        "SpecialFunctions.test_guest_mode_shows_the_guest_image_for_its_refresh_rate": SSD_BMP,
    },
    "CrowPanel42": {
        "SpecialFunctions.test_identify_shows_the_identify_image": CROWPANEL_PNG,
        "SpecialFunctions.test_restart_playlist_shows_the_first_item": CROWPANEL_PNG,
        "SpecialFunctions.test_guest_mode_shows_the_guest_image_for_its_refresh_rate": CROWPANEL_PNG,
        "ApiStatus.test_screen_wiper_clears_then_shows_the_next_item": CROWPANEL_PNG,
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
    },
    "seeed_reTerminal_E1002": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
    },
    "seeed_xiao_esp32s3": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
    },
    "TRMNL_7inch5_OG_DIY_Kit": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
    },
    "TRMNL_7inch5_OG_DIY_Kit_3CLR": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
    },
    "TRMNL_7inch5_OG_DIY_Kit_6CLR": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
    },
    "TRMNL_4inch26_DIY_Kit": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
        "SpecialFunctions.test_identify_shows_the_identify_image": SSD_BMP,
        "SpecialFunctions.test_restart_playlist_shows_the_first_item": SSD_BMP,
        "SpecialFunctions.test_guest_mode_shows_the_guest_image_for_its_refresh_rate": SSD_BMP,
    },
    "seeed_reTerminal_E1001": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
    },
    "seeed_sticky": {
        "Buttons.test_double_click_runs_the_special_function": SLOW_BOOT_CLICK,
        "SpecialFunctions.test_identify_shows_the_identify_image": SSD_BMP,
        "SpecialFunctions.test_restart_playlist_shows_the_first_item": SSD_BMP,
        "SpecialFunctions.test_guest_mode_shows_the_guest_image_for_its_refresh_rate": SSD_BMP,
    },
    "seeed_xiao_esp32c3": {
        "SpecialFunctions": XIAO_C3_BUTTON,
        "Identify": XIAO_C3_BUTTON,
        "Buttons.test_double_click_runs_the_special_function": XIAO_C3_BUTTON,
        "Buttons.test_short_tap_is_a_plain_button_wake": XIAO_C3_BUTTON,
    },
}
for _d in DEVICES.values():
    if _d.default_bmp:
        KNOWN_FAILURES.setdefault(_d.env, {})["SpecialFunctions.test_sleep_keeps_the_screen"] = BMP_NOT_CACHED

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
        s.wait(state="deep_sleep", display_idle=True, timeout_s=120)

    def run_function(self, s, answer: dict, press_ms: int = 1500):
        """Press the button (a medium press counts as a double click) and answer the
        special-function request with `answer`; returns that request."""
        dev().mock.display = answer
        n = len(dev().mock.requests)
        s.press(press_ms)
        req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=120)
        self.assertEqual(req.headers.get("special_function"), "true")
        return req


@needs("button")  # a double click (or medium press) runs the function
class SpecialFunctions(Case):
    def test_identify_shows_the_identify_image(self):
        seven_path, seven = device_image(dev().mock, "seven", device_number("7"))
        with dev().boot() as s:
            self.assign(s, "identify")
            self.run_function(s, {"image": "seven", "action": "identify", "refresh_rate": 300})
            dev().mock.wait_for_request(seven_path, timeout_s=90)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
            self.assertTrue(s.compare_screen(seven, tolerance=64, max_ratio=0)["match"])

    def test_sleep_uses_the_answered_refresh_rate(self):
        with dev().boot() as s:
            self.assign(s, "sleep")
            self.run_function(s, {"image": "default", "action": "sleep", "refresh_rate": 1800})
            st = s.wait(state="deep_sleep", timeout_s=90)["status"]
            self.assertAlmostEqual(st["wake_at_s"] - st["sim_time_s"], 1800, delta=30)

    def test_sleep_keeps_the_screen(self):
        # Where the server's default image is a BMP (see KNOWN_FAILURES), like send_to_me (see
        # there): status=false/HTTPS_SUCCESS makes the OG look for the answer's image in a
        # cache it doesn't have, then report and show an error.
        with dev().boot() as s:
            self.assign(s, "sleep")
            screen = s.screenshot()
            self.run_function(s, {"image": "default", "action": "sleep", "refresh_rate": 1800})
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
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
        one_path, one = device_image(dev().mock, "one", device_number("1"))
        with dev().boot() as s:
            self.assign(s, "restart_playlist")
            self.run_function(s, {"image": "one", "action": "restart_playlist", "refresh_rate": 300})
            dev().mock.wait_for_request(one_path, timeout_s=90)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])

    @unittest.expectedFailure
    def test_send_to_me_keeps_the_current_image(self):
        # After showing /current.bmp, send_to_me leaves status=false/HTTPS_SUCCESS, so
        # downloadAndShow() takes the "image already cached" path with the answer's filename.
        # The OG doesn't cache BMP images under their filename (only PNG and JPEG), so with a
        # BMP it is always "empty or unreadable": the device submits an error log and draws an error message
        # over the image it just showed. Where the image is a PNG, send_to_me doesn't get that
        # far: it looks for /current.bmp or /current.png (bl.cpp:2073), but PNGs are only
        # saved under their filename (bl.cpp:1616-1618 writes nothing but /current.bmp), so
        # it finds "No current image!" and shows an error instead.
        two_path, two = device_image(dev().mock, "two", device_number("2"))
        with dev().boot() as s:
            self.assign(s, "send_to_me", image="two")
            self.run_function(s, {"image": "two", "action": "send_to_me", "refresh_rate": 300})
            s.wait(state="deep_sleep", display_idle=True, timeout_s=90)
            self.assertEqual(dev().mock.count("/api/log"), 0)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])

    def test_guest_mode_shows_the_guest_image_for_its_refresh_rate(self):
        three_path, three = device_image(dev().mock, "three", device_number("3"))
        with dev().boot() as s:
            self.assign(s, "guest_mode")
            self.run_function(s, {"image": "three", "action": "guest_mode", "refresh_rate": 1200})
            dev().mock.wait_for_request(three_path, timeout_s=90)
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=90)["status"]
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


@needs("button")
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

    def test_screen_wiper_clears_then_shows_the_next_item(self):
        four_path, four = device_image(dev().mock, "four", device_number("4"))
        dev().mock.display_queue = [{"image": "default", "filename": "screen_wiper.png", "refresh_rate": 300}]
        dev().mock.display = {"image": "four", "refresh_rate": 300}
        with dev().boot() as s:
            dev().mock.wait_for_request(four_path, timeout_s=600)
            s.wait(state="deep_sleep", display_idle=True, timeout_s=600)
            self.assertTrue(s.compare_screen(four, tolerance=64, max_ratio=0)["match"])

    def test_screen_wiper_is_only_run_once_per_wake(self):
        dev().mock.display = {"image": "default", "filename": "screen_wiper.png", "refresh_rate": 300}
        with dev().boot() as s:
            s.wait(state="deep_sleep", timeout_s=600)
            self.assertEqual(dev().mock.count("/api/display"), 2)


@needs("button")
class Buttons(Case):
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

    def test_tap_then_long_press_resets_wifi(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.press(20)
            s.press(6000)
            s.wait(portal=True, timeout_s=120)

    def test_very_long_press_is_a_soft_reset(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            s.press(16000)
            s.wait(portal=True, timeout_s=180)


if __name__ == "__main__":
    unittest.main()
