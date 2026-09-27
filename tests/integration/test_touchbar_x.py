"""The TRMNL X touch bar: browsing cached images, holds, the WiFi-reset and power-off
confirmations, and slide mode (swipes; flicks aren't enabled)."""

import time
import unittest

from support import close_fixtures, fixture
from support_x import ProvisionedX, ShippedX, X_BUILD
from test_trmnl_x import digits

ENV = "TRMNL_X"  # the PlatformIO environment these tests run (bin/spec TRMNL_X)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

shipped = fixture(ShippedX)
dev = fixture(lambda: ProvisionedX(shipped()))


def setUpModule():
    if not (X_BUILD / "firmware.elf").exists():
        raise unittest.SkipTest(f"no TRMNL_X build at {X_BUILD} (set TRMNL_X_BUILD)")


def tearDownModule():
    close_fixtures()


def hold_edges(s, *zones):
    """Put fingers on several zones at the same instant (and leave them there)."""
    s.pause(True)
    try:
        for z in zones:
            s.touch_down(z)
    finally:
        s.pause(False)


def ask_to_reset_wifi(s, since: int):
    """Hold both edges, let go as the confirmation prompt appears, and wait until it is on
    the screen: the firmware draws the prompt (a few seconds on the X panel) before it reads
    the touch bar again, so an answer given during the refresh is missed."""
    hold_edges(s, "left", "right")
    s.wait(console=r"Entering WiFi reset confirmation mode", since=since, timeout_s=15)
    lift(s, "left", "right")
    # The display is still idle right after that line; wait for the prompt to be drawn.
    s.wait(console=r"display_show_msg end", since=since, timeout_s=15)
    s.wait(display_idle=True, settle_ms=100, timeout_s=15)


def lift(s, *zones):
    for z in zones:
        s.touch_up(z)


class Case(unittest.TestCase):
    def setUp(self):
        m = dev().mock
        m.requests.clear()
        m.display_queue.clear()
        m.clear_faults()
        m.display = {"image": "default", "refresh_rate": 300}

    def boot_two_images(self, mode: str | None = None):
        """Boot and show "1" then "2" (both cached); returns the simulator (asleep) and
        their expected screens."""
        m = dev().mock
        one, two = m.set_png("one", digits("1")), m.set_png("two", digits("2"))
        extra = {"touchbar_mode": mode} if mode else {}
        m.display_queue = [{"image": "one", "refresh_rate": 300, **extra}]
        m.display = {"image": "two", "refresh_rate": 300, **extra}
        s = dev().boot()
        try:
            m.wait_for_request("/images/one.png", timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)
            s.wake()
            m.wait_for_request("/images/two.png", timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
        except BaseException:
            s.close()
            raise
        return s, one, two

    def console_since(self, s, c) -> str:
        return "\n".join(s.console(c))


class TapMode(Case):
    def test_right_tap_goes_forward_through_cached_images(self):
        s, one, two = self.boot_two_images()
        with s:
            s.touch("left", 150)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])
            c = s.status()["console_total"]
            s.touch("right", 150)
            s.wait(console=r"Next button tapped", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])

    def test_holding_left_or_right_browses_too(self):
        s, one, two = self.boot_two_images()
        with s:
            c = s.status()["console_total"]
            s.touch("left", 2500)
            s.wait(console=r"Back button hold", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])
            c = s.status()["console_total"]
            s.touch("right", 2500)
            s.wait(console=r"Next button hold", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])

    def test_middle_hold_refreshes(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=15)
            n, c = len(dev().mock.requests), s.status()["console_total"]
            s.touch("center", 2500)
            s.wait(console=r"Middle button hold", since=c, timeout_s=15)
            dev().mock.wait_for_request("/api/display", after=n, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)


class WifiResetConfirmation(Case):
    def ask(self, s):
        s.wait(state="deep_sleep", timeout_s=15)
        c = s.status()["console_total"]
        ask_to_reset_wifi(s, c)
        return c

    def test_holding_both_edges_then_the_middle_resets_wifi(self):
        with dev().boot_asleep() as s:
            self.ask(s)
            s.touch("center", 1500)
            s.wait(portal=True, timeout_s=15)

    def test_tapping_an_edge_cancels(self):
        with dev().boot_asleep() as s:
            c = self.ask(s)
            s.touch("left", 150)
            s.wait(console=r"Confirmation cancelled - outer button", since=c, timeout_s=15)
            st = s.wait(state="deep_sleep", timeout_s=15)["status"]
            self.assertIsNone(st["portal_url"])

    def test_tapping_the_middle_cancels(self):
        with dev().boot_asleep() as s:
            c = self.ask(s)
            s.touch("center", 150)
            s.wait(console=r"Confirmation cancelled - tap on middle", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)

    def test_no_answer_cancels_after_15_seconds(self):
        with dev().boot_asleep() as s:
            c = self.ask(s)
            s.wait(console=r"Confirmation timeout - cancelling", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)


class SlideMode(Case):
    def test_swipes_browse_cached_images(self):
        s, one, two = self.boot_two_images(mode="slide")
        with s:
            c = s.status()["console_total"]
            s.gesture("swipe_back")
            s.wait(console=r"SLIDER: Swipe <-", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])
            c = s.status()["console_total"]
            s.gesture("swipe_next")
            s.wait(console=r"SLIDER: Swipe ->", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])

    def test_flicks_are_not_enabled(self):
        # Slide mode's GESTURE_SELECT (0x0B) enables tap, swipe and hold but not flick, so
        # the controller never reports one (read_gesture_event's flick cases are unused).
        s, one, two = self.boot_two_images(mode="slide")
        with s:
            boots, n = s.status()["boot_count"], len(dev().mock.requests)
            for gesture in ("flick_back", "flick_next"):
                s.gesture(gesture)
                t0 = s.status()["sim_time_s"]
                while s.status()["sim_time_s"] < t0 + 2:
                    time.sleep(0.1)
            st = s.status()
            self.assertEqual((st["state"], st["boot_count"]), ("deep_sleep", boots))
            self.assertEqual(len(dev().mock.requests), n)
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])

    @unittest.expectedFailure
    def test_taps(self):
        # A tap wakes the device with the finger already lifted: the wake stub's slider
        # coordinate is 0xFFFF, so read_gesture_event() clears the TAP before
        # check_channel_states() runs, and the tap is handled as a plain wake (a refresh)
        # with no "... button pressed" or indicator.
        s, one, two = self.boot_two_images(mode="slide")
        with s:
            for zone, line in (("left", "Back button pressed"), ("center", "Middle button pressed"),
                               ("right", "Next button pressed")):
                c = s.status()["console_total"]
                s.touch(zone, 150)
                s.wait(console=line, since=c, timeout_s=15)
                s.wait(state="deep_sleep", timeout_s=15, settle_ms=300)

    def test_holding_both_edges_asks_and_a_middle_hold_confirms(self):
        s, _, _ = self.boot_two_images(mode="slide")
        with s:
            ask_to_reset_wifi(s, s.status()["console_total"])
            s.touch("center", 1500)
            s.wait(portal=True, timeout_s=15)

    def test_a_tap_cancels_the_confirmation(self):
        s, _, two = self.boot_two_images(mode="slide")
        with s:
            c = s.status()["console_total"]
            ask_to_reset_wifi(s, c)
            s.touch("left", 150)
            s.wait(console=r"WiFi reset cancelled by user - tap detected", since=c, timeout_s=15)
            st = s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)["status"]
            self.assertIsNone(st["portal_url"])
            self.assertTrue(s.compare_screen(two, tolerance=64, max_ratio=0)["match"])

    def test_back_to_tap_mode(self):
        s, one, two = self.boot_two_images(mode="slide")
        with s:
            dev().mock.display = {"image": "two", "refresh_rate": 300, "touchbar_mode": "tap"}
            n = len(dev().mock.requests)
            s.wake()
            dev().mock.wait_for_request("/api/display", after=n, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            c = s.status()["console_total"]
            s.touch("left", 150)
            s.wait(console=r"Back button tapped", since=c, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15, settle_ms=500)
            self.assertTrue(s.compare_screen(one, tolerance=64, max_ratio=0)["match"])


class PowerOffConfirmation(unittest.TestCase):
    """In the setup portal, holding both edges asks whether to power off (back to shipment
    mode); a middle hold confirms, a tap or 15 s without an answer cancels."""

    def ask(self, s) -> int:
        s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=30)
        s.dock(True)
        s.wait(portal=True, timeout_s=30)
        s.dock(False)
        s.wait(display_idle=True, settle_ms=200, timeout_s=15)
        c = s.status()["console_total"]
        hold_edges(s, "left", "right")
        s.wait(console=r"Entering power-off confirmation mode", since=c, timeout_s=15)
        lift(s, "left", "right")
        s.wait(console=r"display_show_msg end", since=c, timeout_s=15)
        s.wait(display_idle=True, settle_ms=100, timeout_s=15)
        return c

    def test_middle_hold_powers_off(self):
        with shipped().boot() as s:
            c = self.ask(s)
            boots = s.status()["boot_count"]
            s.touch("center", 1500)
            s.wait(console=r"Confirmed - holding middle button in tap mode", since=c, timeout_s=15)
            deadline = time.time() + 30
            while s.status()["boot_count"] == boots:
                self.assertLess(time.time(), deadline, "did not restart")
                time.sleep(0.2)
            # shipment status cleared: back to shipment mode until the charger is connected
            c = s.status()["console_total"]
            s.wait(console=r"Entering shipment mode light sleep loop", since=c, timeout_s=60)

    def cancelled(self, s, c):
        s.wait(console=r"Entering power-off confirmation mode", since=c, timeout_s=1)
        s.wait(display_idle=True, settle_ms=200, timeout_s=30)
        self.assertIsNotNone(s.status()["portal_url"])  # the portal carries on

    def test_a_tap_cancels(self):
        with shipped().boot() as s:
            c = self.ask(s)
            s.touch("left", 150)  # the portal runs the touch bar in tap mode
            s.wait(console=r"Confirmation cancelled - outer button in tap mode", since=c, timeout_s=15)
            self.cancelled(s, c)

    def test_no_answer_cancels_after_15_seconds(self):
        with shipped().boot() as s:
            c = self.ask(s)
            s.wait(console=r"Confirmation timeout - cancelling", since=c, timeout_s=30)
            self.cancelled(s, c)


if __name__ == "__main__":
    unittest.main()
