"""Save points (TRMNL OG): save a device in deep sleep, restore it in a fresh simulator, and
carry on where it left off: same screen, no re-onboarding, both wake sources."""

import shutil
import tempfile
import unittest
from pathlib import Path

from support import BUILD, BWRY_BUILD, ProvisionedDevice, big_number, sim
from trmnl_sim import SimError

dev: ProvisionedDevice
tmp: Path
saved: Path  # deep sleep, showing `seven`, taken in setUpModule
seven: bytes
saved_status: dict


def setUpModule():
    global dev, tmp, saved, seven, saved_status
    dev = ProvisionedDevice()
    tmp = Path(tempfile.mkdtemp(prefix="trmnl-savepoints-"))
    saved = tmp / "asleep.trmnlsave"
    seven = dev.mock.set_image("seven", big_number("7"))
    dev.mock.display = {"image": "seven", "refresh_rate": 300}
    with dev.boot() as s:
        dev.mock.wait_for_request("/images/seven.bmp", timeout_s=120)
        saved_status = s.wait(state="deep_sleep", timeout_s=120)["status"]
        assert s.compare_screen(seven)["match"]
        info = s.save_point(saved, label="asleep showing 7")
        assert info["deep_sleep"] and info["label"] == "asleep showing 7", info


def tearDownModule():
    dev.close()
    shutil.rmtree(tmp, ignore_errors=True)


class SavePoints(unittest.TestCase):
    def setUp(self):
        dev.mock.requests.clear()
        dev.mock.display_queue.clear()
        self.eight = dev.mock.set_image("eight", big_number("8"))
        dev.mock.display = {"image": "eight", "refresh_rate": 300}

    def restored(self) -> "sim":
        return sim(BUILD, restore=saved, extra_args=("--offline",))

    def test_restores_into_the_same_deep_sleep(self):
        with self.restored() as s:
            st = s.wait(state="deep_sleep", timeout_s=30)["status"]
            self.assertEqual(st["boot_count"], saved_status["boot_count"])
            self.assertEqual(st["display_refreshes"], saved_status["display_refreshes"])
            self.assertAlmostEqual(st["wake_at_s"], saved_status["wake_at_s"], places=3)
            self.assertGreaterEqual(st["sim_time_s"], saved_status["sim_time_s"])
            self.assertTrue(s.compare_screen(seven, tolerance=0, max_ratio=0)["match"])
            s.wait_for_console(r"restored save point \"asleep showing 7\": deep sleep, wakes in")
            self.assertEqual(dev.mock.requests, [])

    def test_timer_wake_refreshes_without_onboarding(self):
        with self.restored() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            s.wake()
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["Access-Token"], dev.mock.api_key)
            self.assertEqual(req.headers["Update-Source"], "timer")
            s.wait(console=r"rst:0x5 \(DSLEEP\)", min_boots=saved_status["boot_count"] + 1)
            s.wait(state="deep_sleep", timeout_s=120)
            self.assertTrue(s.compare_screen(self.eight)["match"])
            self.assertNotIn("/api/setup", [r.path for r in dev.mock.requests])

    def test_button_wakes_a_restored_device(self):
        with self.restored() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            s.press(150)
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["Update-Source"], "button")
            s.wait(state="deep_sleep", timeout_s=120)

    def test_in_memory_slot_goes_back_in_time(self):
        with self.restored() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            slot = s.save_point()
            self.assertEqual([p["id"] for p in s.save_points()], [slot["id"]])
            s.wake()
            dev.mock.wait_for_request("/images/eight.bmp", timeout_s=120)
            st = s.wait(state="deep_sleep", timeout_s=120)["status"]
            self.assertTrue(s.compare_screen(self.eight)["match"])
            info = s.restore(id=slot["id"])
            self.assertEqual(info["label"], slot["label"])
            back = s.wait(state="deep_sleep", timeout_s=30)["status"]
            self.assertTrue(s.compare_screen(seven, tolerance=0, max_ratio=0)["match"])
            self.assertEqual(back["boot_count"], st["boot_count"] - 1)
            self.assertLess(back["sim_time_s"], st["sim_time_s"])

    def test_power_off_save_point_boots_the_saved_flash(self):
        path = tmp / "off.trmnlsave"
        with self.restored() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            s.wake()
            s.wait(console=r"rst:0x5 \(DSLEEP\)")
            s.pause()
            try:
                info = s.save_point(path)
            except SimError as e:  # caught mid-refresh: try again once it is idle
                self.assertIn("refreshing", str(e))
                s.pause(False)
                s.wait(display_idle=True, state="deep_sleep", timeout_s=120)
                self.skipTest("the device refreshed before it could be paused")
            self.assertFalse(info["deep_sleep"])
        dev.mock.requests.clear()
        with sim(BUILD, restore=path, extra_args=("--offline",)) as s:
            s.wait_for_console(r"powering on")
            s.wait(console=r"rst:0x1 \(POWERON\)")
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["Access-Token"], dev.mock.api_key)
            self.assertNotIn("/api/setup", [r.path for r in dev.mock.requests])

    def test_bad_files_are_refused(self):
        junk = tmp / "junk.trmnlsave"
        junk.write_bytes(b"not a save point")
        with self.restored() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            with self.assertRaisesRegex(SimError, "not a trmnl-sim save point"):
                s.restore(junk)
            with self.assertRaisesRegex(SimError, "no save point #99"):
                s.restore(id=99)
            self.assertEqual(s.status()["state"], "deep_sleep")
        with self.assertRaisesRegex(SimError, "not a trmnl-sim save point"):
            sim(BUILD, restore=junk)

    @unittest.skipUnless((BWRY_BUILD / "firmware.elf").exists(), "no trmnl_4clr build")
    def test_other_firmware_is_refused(self):
        with self.assertRaisesRegex(SimError, "restore it with the build it was saved from"):
            sim(BWRY_BUILD, restore=saved)
        with sim(BWRY_BUILD, erase=True) as s:
            with self.assertRaisesRegex(SimError, "save point was taken with firmware"):
                s.restore(saved)


if __name__ == "__main__":
    unittest.main()
