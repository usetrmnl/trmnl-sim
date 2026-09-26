"""The TRMNL X touch bar: browsing cached images, holds, the WiFi-reset
confirmation, and slide mode (swipes and flicks)."""

import unittest

from support import close_fixtures, fixture
from support_x import ProvisionedX, ShippedX, X_BUILD
from test_trmnl_x import digits

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

    @unittest.skip("not implemented yet: tap, middle and next presses in slide mode")
    def test_taps(self):
        pass

    @unittest.skip("not implemented yet: the middle hold isn't seen as a confirmation in slide mode "
                   "(check_wifi_reset_confirm needs a HOLD gesture while CH1 is touched)")
    def test_holding_both_edges_asks_and_a_middle_hold_confirms(self):
        pass

    @unittest.skip("not implemented yet: cancelling the slide-mode confirmation with a tap")
    def test_a_tap_cancels_the_confirmation(self):
        pass

    @unittest.skip("not implemented yet: switching back from slide to tap mode")
    def test_back_to_tap_mode(self):
        pass


if __name__ == "__main__":
    unittest.main()
