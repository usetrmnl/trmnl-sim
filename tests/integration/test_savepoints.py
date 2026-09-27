"""Save points (on the device under test): save a device in deep sleep, restore it in a fresh simulator, and
carry on where it left off: same screen, no re-onboarding, both wake sources."""

import shutil
import tempfile
import unittest
from pathlib import Path

from support import (BUILD, BWRY_BUILD, DEVICE, OG_BUILD, ProvisionedDevice, Simulator, big_number, device_image, device_number, needs, sim)
from trmnl_sim import SimError

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

# Another device's firmware, which must refuse this device's save points.
OTHER_BUILD = BWRY_BUILD if DEVICE.env == "trmnl" else OG_BUILD

# Firmware bug on the SSD16xx boards (see test_refresh_cycle): the next image, a 1-bit BMP, is
# refreshed partially against a stale old-image RAM.
SSD_BMP = "SSD16xx: a 1-bit BMP is refreshed partially against a stale old-image RAM (display.cpp:1947)"
CROWPANEL_PNG = ("CrowPanel: png_to_epd() calls bbep.setPanelType(dpList[...].OneBit) (display.cpp:1764) with the "
                 "bb_epaper product number the panel was begun with, selecting a 2.9\" 128x296 panel: 1-bit PNGs "
                 "never show (see test_byod_ssd.CrowPanel42)")
# Firmware bugs on the XIAO ESP32-C3: GPIO 9 can't wake a C3 from deep sleep; and bl_init() waits 2 s (bl.cpp:731-733) before it reads the
# button, so a wake press is over by then, and classify_button_presses() (button.cpp:66-71)
# then waits for another press with no timeout: the device stays awake until pressed again.
XIAO_C3_BUTTON = ("XIAO C3: its button is GPIO 9 (display.cpp:56), which can't wake a C3 from deep sleep "
                  "(bl.cpp:2303's esp_deep_sleep_enable_gpio_wakeup fails); and were it woken, bl.cpp:731-733's 2 s "
                  "delay outlasts the press and button.cpp:66-71 then waits for another press forever")
# Firmware (bb_epaper) bug on the Waveshare 3.97": EP397_800x480 starts the RAM Y counter at 0
# instead of 479, so every picture lands one row too high, its top row at the bottom (see
# test_byod_ssd.Waveshare397); its BMPs also hit the SSD16xx partial refresh bug.
WAVESHARE_ROW = ("bb_epaper EP397_800x480 init sets the RAM Y counter to 0 while counting down from 479: "
                 "every screen is one row too high (see test_byod_ssd.Waveshare397)")
# Firmware bug on the gen-2 BWRY: its env defines BOARD_TRMNL_GEN2 but not BOARD_TRMNL_4CLR, so
# images take the 1-bit two-plane path the BWRY panel misreads (see test_og_gen2.OgGen2Bwry).
GEN2_BWRY_IMAGES = ("trmnl_gen2_4clr lacks BOARD_TRMNL_4CLR, so display.cpp's 4-color image path isn't compiled: "
                    "images come out half drawn in the wrong inks (see test_og_gen2.OgGen2Bwry)")
KNOWN_FAILURES = {
    "xteink_x4": {
        "SavePoints.test_timer_wake_refreshes_without_onboarding": SSD_BMP,
        "SavePoints.test_in_memory_slot_goes_back_in_time": SSD_BMP,
    },
    "CrowPanel42": {
        "SavePoints.test_timer_wake_refreshes_without_onboarding": CROWPANEL_PNG,
        "SavePoints.test_in_memory_slot_goes_back_in_time": CROWPANEL_PNG,
    },
    "TRMNL_4inch26_DIY_Kit": {
        "SavePoints.test_timer_wake_refreshes_without_onboarding": SSD_BMP,
        "SavePoints.test_in_memory_slot_goes_back_in_time": SSD_BMP,
    },
    "seeed_sticky": {
        "SavePoints.test_timer_wake_refreshes_without_onboarding": SSD_BMP,
        "SavePoints.test_in_memory_slot_goes_back_in_time": SSD_BMP,
    },
    "seeed_xiao_esp32c3": {
        "SavePoints.test_button_wakes_a_restored_device": XIAO_C3_BUTTON,
    },
    "WAVESHARE_397": {
        "SavePoints.test_timer_wake_refreshes_without_onboarding": WAVESHARE_ROW,
        "SavePoints.test_in_memory_slot_goes_back_in_time": WAVESHARE_ROW,
    },
    "trmnl_gen2_4clr": {
        "SavePoints.test_timer_wake_refreshes_without_onboarding": GEN2_BWRY_IMAGES,
        "SavePoints.test_in_memory_slot_goes_back_in_time": GEN2_BWRY_IMAGES,
    },
}

dev: ProvisionedDevice
tmp: Path
saved: Path  # deep sleep, showing `seven`, taken in setUpModule
seven: bytes
saved_screen: bytes  # the screen when it was saved (`seven`, unless the device drew it wrong)
saved_status: dict
saved_screen: bytes  # the screen when `saved` was taken (a gray panel's differs from `seven` a little)


def setUpModule():
    global dev, tmp, saved, seven, saved_screen, saved_status
    dev = ProvisionedDevice()
    tmp = Path(tempfile.mkdtemp(prefix="trmnl-savepoints-"))
    saved = tmp / "asleep.trmnlsave"
    path, seven = device_image(dev.mock, "seven", device_number("7"))
    dev.mock.display = {"image": "seven", "refresh_rate": 300}
    with dev.boot() as s:
        dev.mock.wait_for_request(path, timeout_s=120)
        saved_status = s.wait(state="deep_sleep", display_idle=True, timeout_s=120)["status"]
        # (whether it shows `seven` right is test_refresh_cycle's business; a save point
        # must bring back whatever is on screen)
        saved_screen = s.screenshot()
        info = s.save_point(saved, label="asleep showing 7")
        assert info["deep_sleep"] and info["label"] == "asleep showing 7", info


def tearDownModule():
    dev.close()
    shutil.rmtree(tmp, ignore_errors=True)


class SavePoints(unittest.TestCase):
    def setUp(self):
        dev.mock.requests.clear()
        dev.mock.display_queue.clear()
        self.eight_path, self.eight = device_image(dev.mock, "eight", device_number("8"))
        dev.mock.display = {"image": "eight", "refresh_rate": 300}

    def restored(self) -> Simulator:
        return dev.restore(saved)

    def test_restores_into_the_same_deep_sleep(self):
        with self.restored() as s:
            st = s.wait(state="deep_sleep", timeout_s=30)["status"]
            self.assertEqual(st["boot_count"], saved_status["boot_count"])
            self.assertEqual(st["display_refreshes"], saved_status["display_refreshes"])
            self.assertAlmostEqual(st["wake_at_s"], saved_status["wake_at_s"], places=3)
            self.assertGreaterEqual(st["sim_time_s"], saved_status["sim_time_s"])
            self.assertTrue(s.compare_screen(saved_screen, tolerance=0, max_ratio=0)["match"])
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
            s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
            self.assertTrue(s.compare_screen(self.eight)["match"])
            self.assertNotIn("/api/setup", [r.path for r in dev.mock.requests])

    @needs("button")
    def test_button_wakes_a_restored_device(self):
        with self.restored() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            s.press(150)
            req = dev.mock.wait_for_request("/api/display", timeout_s=120)
            self.assertEqual(req.headers["Update-Source"], DEVICE.button_source)
            s.wait(state="deep_sleep", timeout_s=120)

    def test_in_memory_slot_goes_back_in_time(self):
        with self.restored() as s:
            s.wait(state="deep_sleep", timeout_s=30)
            slot = s.save_point()
            self.assertEqual([p["id"] for p in s.save_points()], [slot["id"]])
            s.wake()
            dev.mock.wait_for_request(self.eight_path, timeout_s=120)
            st = s.wait(state="deep_sleep", display_idle=True, timeout_s=120)["status"]
            self.assertTrue(s.compare_screen(self.eight)["match"])
            info = s.restore(id=slot["id"])
            self.assertEqual(info["label"], slot["label"])
            back = s.wait(state="deep_sleep", timeout_s=30)["status"]
            self.assertTrue(s.compare_screen(saved_screen, tolerance=0, max_ratio=0)["match"])
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
        with dev.restore(path) as s:
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

    @unittest.skipUnless((OTHER_BUILD / "firmware.elf").exists(), f"no {OTHER_BUILD.name} build")
    def test_other_firmware_is_refused(self):
        with self.assertRaisesRegex(SimError, "restore it with the build it was saved from"):
            sim(OTHER_BUILD, restore=saved)
        with sim(OTHER_BUILD, erase=True) as s:
            with self.assertRaisesRegex(SimError, "save point was taken with firmware"):
                s.restore(saved)


if __name__ == "__main__":
    unittest.main()
