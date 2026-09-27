"""--memcheck: whole onboarding and refresh cycles of the device under test (and the X)
under the memory checker (heap use-after-free, overflows, bad frees; stack high-water marks).

Firmware bugs it found are in support.KNOWN_MEMORY_BUGS: the clean-cycle tests tolerate
them (so everything else is still checked), and each has a test here that fails where the
bug shows (KNOWN_FAILURES, or @expectedFailure for the device-specific classes)."""

import shutil
import tempfile
import unittest
from pathlib import Path

import setup_cache
from support import (BUILD, BWRY_BUILD, DEVICE, KNOWN_MEMORY_BUGS, TURBO, MockTrmnl, device_image, panel_number, sim,
                     skip_if)
from support_x import SSID_24, X_BUILD, onboard, x_sim

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

SNTP_BUG = ("_ZN5Clock14setTimeFromNTPEv", "sntp_request")
BODY_BUG = "_ZNK16HttpRetryRequest12bodyAsStringEv"
QA_BUG = "_Z19display_show_msg_qaPhPKfS1_b"


def all_bugs_but(*names: str) -> tuple[str, ...]:
    """The known memory bugs to suppress while a test checks for the others."""
    return tuple(b for b in KNOWN_MEMORY_BUGS if b not in names)


IDF_SMALL_STACKS = (
    "framework: Arduino-ESP32 2.0.17's prebuilt ESP32-S3 sdkconfig gives the IDLE and ipc tasks "
    "1024-byte stacks (CONFIG_FREERTOS_IDLE_TASK_STACKSIZE, CONFIG_ESP_IPC_TASK_STACK_SIZE), and "
    "IDLE0 / ipc0 come within 256 bytes of their end")
QA_SECOND_DISPLAY_INIT = (
    "firmware: the factory QA aborts before it shows its results: its second display_init() "
    "fails on FastEPD boards (see test_errors.QA_SECOND_DISPLAY_INIT)")

# Arduino 3 builds: its String::concat(buf, len) copies len bytes, not len + 1
ARDUINO_3 = ("TRMNL_X", "TRMNL_X_PAPERS3", "TRMNL_X_LILYGO_T5PRO", "TRMNL_X_SENSORIAC5", "trmnl_gen2", "trmnl_gen2_4clr")
# The IDF tasks the firmware doesn't size (see IDF_SMALL_STACKS).
IDF_TASKS = ("IDLE", "ipc")
# startQA()'s buffer is 48000 bytes: display_show_msg_qa() reads past it on panels whose
# 1-bit frame is bigger than 48000 - 62 bytes (bb_epaper boards; FastEPD ones don't copy it).
QA_OVERREADS = DEVICE.og_font and DEVICE.size[0] // 8 * DEVICE.size[1] > 48000 - 62


def fixed_on(*envs: str, why: str):
    """An expected failure, except on these environments, where the bug doesn't show (`why`)."""
    return lambda test: test if DEVICE.env in envs else unittest.expectedFailure(test)


def shows_on(*envs: str, why: str):
    """An expected failure on these environments only: elsewhere the bug doesn't show (`why`)."""
    return lambda test: unittest.expectedFailure(test) if DEVICE.env in envs else test


def expected_failure_if(cond: bool):
    return lambda test: unittest.expectedFailure(test) if cond else test


KNOWN_FAILURES = {
    **{env: {"MemcheckOG.test_idf_task_stacks_have_room": IDF_SMALL_STACKS} for env in ()},
    "TRMNL_X_PAPERS3": {"MemcheckOG.test_qa_screen_is_not_read_past_its_buffer": QA_SECOND_DISPLAY_INIT},
    "TRMNL_X_LILYGO_T5PRO": {"MemcheckOG.test_qa_screen_is_not_read_past_its_buffer": QA_SECOND_DISPLAY_INIT},
}


def assert_no_low_stacks(test: unittest.TestCase, report: dict, but: tuple[str, ...] = ()) -> None:
    low = [(t["task"], t["min_free"]) for t in report["stacks"] if t["low"] and not t["task"].startswith(but)]
    test.assertEqual(low, [], "tasks close to overflowing their stacks")


class MemcheckOG(unittest.TestCase):
    def onboard(self, s, mock):
        s.wait(portal=True, timeout_s=90)
        s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
        mock.wait_for_request("/api/display", timeout_s=120)
        s.wait(state="deep_sleep", timeout_s=120)

    def test_onboarding_and_refresh_cycles_are_clean(self):
        with MockTrmnl() as mock, sim(BUILD, erase=True, memcheck="halt", extra_args=("--offline",)) as s:
            device_image(mock, "one", panel_number("1"))
            mock.display = {"image": "one", "refresh_rate": 300}
            self.onboard(s, mock)
            n = len(mock.requests)
            s.wake()
            mock.wait_for_request("/api/display", after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(mock.requests)
            if DEVICE.button:
                s.press(150)
            else:
                s.wake()
            mock.wait_for_request("/api/display", after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)

            report = s.memcheck()
            self.assertTrue(report["enabled"])
            self.assertEqual(report["violations"], [])
            heap = report["heap"]
            self.assertGreater(heap["allocs"], 500)
            self.assertGreater(heap["internal"]["peak_bytes"], 50_000)
            if DEVICE.psram_frame_buffers:
                self.assertGreater(heap["psram"]["peak_bytes"], 1_000_000)
            elif not DEVICE.psram:
                self.assertEqual(heap["psram"]["peak_bytes"], 0)
            stacks = {t["task"]: t for t in report["stacks"]}
            self.assertEqual(stacks["loopTask"]["size"], 8192)
            self.assertGreater(stacks["loopTask"]["max_used"], 1000)
            self.assertGreaterEqual(stacks["loopTask"]["instances"], 3)  # one per boot
            assert_no_low_stacks(self, report, but=IDF_TASKS)  # (see test_idf_task_stacks_have_room)

    def test_idf_task_stacks_have_room(self):
        # The IDF's own small tasks (IDLE, ipc), sized by the framework's sdkconfig, over an
        # onboarding.
        with MockTrmnl() as mock, sim(BUILD, erase=True, memcheck="log", extra_args=("--offline",)) as s:
            device_image(mock, "one", panel_number("1"))
            mock.display = {"image": "one", "refresh_rate": 300}
            self.onboard(s, mock)
            report = s.memcheck()
            self.assertTrue([t for t in report["stacks"] if t["task"].startswith("IDLE")], "no IDLE task seen")
            assert_no_low_stacks(self, {"stacks": [t for t in report["stacks"] if t["task"].startswith(IDF_TASKS)]})

    @shows_on("trmnl", "xteink_x4", why="only where SNTP's first request goes out after setTimeFromNTP returned")
    def test_sntp_server_name_is_not_used_after_free(self):
        # Clock::sync hands configTime() the c_str() of a String that setTimeFromNTP frees on
        # return; SNTP keeps the pointer and resolves it again on every retry.
        with MockTrmnl() as mock, sim(BUILD, erase=True, memcheck="log", memcheck_suppress=all_bugs_but(*SNTP_BUG),
                                      extra_args=("--offline",)) as s:
            device_image(mock, "one", panel_number("1"))
            mock.display = {"image": "one", "refresh_rate": 300}
            self.onboard(s, mock)
            s.assert_no_memory_errors()

    @fixed_on(*ARDUINO_3, why="Arduino 3's String::concat(buf, len) copies len bytes, not len + 1")
    def test_api_display_body_is_not_read_past_its_end(self):
        # bodyAsString() calls String::concat(body, size), which copies size + 1 bytes of a
        # body that isn't NUL-terminated. The built-in server sends a Content-Length, which
        # takes the path with a malloc'd body.
        with sim(BUILD, erase=True, memcheck="log", memcheck_suppress=all_bugs_but(BODY_BUG),
                 extra_args=("--offline",)) as s:
            url = s.mock.start()
            s.mock.display(refresh_rate=300)
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "password", server=url)
            s.mock.wait_for_request("/api/display", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
            s.assert_no_memory_errors()

    @skip_if("shipment", why="its factory flow runs no QA test (main.cpp)")
    @expected_failure_if(QA_OVERREADS)
    def test_qa_screen_is_not_read_past_its_buffer(self):
        # A fresh device near a "TRMNL_QA" network runs the factory test (test_errors.FactoryQa)
        # and shows its results over startQA()'s white buffer.
        nets = [{"ssid": "TRMNL_QA", "rssi": -40}, {"ssid": "TRMNL-Sim"}]
        with sim(BUILD, erase=True, networks=nets, memcheck="log", memcheck_suppress=all_bugs_but(QA_BUG),
                 extra_args=("--offline",)) as s:
            s.wait(console=r"QA Test Passed", timeout_s=180)
            s.assert_no_memory_errors()


class MemcheckBwry(unittest.TestCase):
    ENV = "trmnl_4clr"
    @classmethod
    def setUpClass(cls):
        if not (BWRY_BUILD / "firmware.elf").exists():
            raise unittest.SkipTest(f"no trmnl_4clr build at {BWRY_BUILD} (set TRMNL_BWRY_BUILD)")

    @unittest.expectedFailure
    def test_1bit_bmp_is_not_sent_as_a_2bit_plane(self):
        # display_show_image hands an uncompressed 1-bit BMP to the driver as the frame
        # buffer; on the 4-color panel writePlane reads it as 2 bits per pixel, 48 KB past
        # the end of the 48 KB buffer.
        with MockTrmnl() as mock, sim(BWRY_BUILD, erase=True, memcheck="log", memcheck_suppress=SNTP_BUG,
                                      extra_args=("--offline",)) as s:
            mock.display = {"image": "default", "refresh_rate": 300}  # the OG's BMP
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
            mock.wait_for_request("/api/display", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=180)
            s.assert_no_memory_errors()


class MemcheckXCase(unittest.TestCase):
    """A factory-fresh X (the QA flow and modem flashing run under memcheck too, once per
    firmware/simulator: it comes from the setup cache and is only cached if it was clean),
    then onboarding on 2.4 GHz: WiFi stop/start around the portal is where Arduino once
    freed a netif the event task still used."""
    ENV = "TRMNL_X"

    @classmethod
    def setUpClass(cls):
        if not (X_BUILD / "firmware.elf").exists():
            raise unittest.SkipTest(f"no TRMNL_X build at {X_BUILD} (set TRMNL_X_BUILD)")
        inputs = {"build": setup_cache.build_id(X_BUILD), "turbo": TURBO}
        cls.shipped, _ = setup_cache.entry("x-shipped-memcheck", inputs, cls._factory)
        cls.dir = Path(tempfile.mkdtemp(prefix="trmnl-x-memcheck-"))

    @staticmethod
    def _factory(out: Path) -> dict:
        with x_sim(flash=out / "flash.bin", erase=True, memcheck="halt", name="x-memcheck-factory") as s:
            s.wait_for_console(r"Entering shipment mode light sleep loop", timeout_s=180)
        return {}

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.dir, ignore_errors=True)

    def boot(self, **kw):
        flash = self.dir / f"flash-{len(list(self.dir.iterdir()))}.bin"
        shutil.copy(self.shipped / "flash.bin", flash)
        return x_sim(flash=flash, **kw)


class MemcheckX(MemcheckXCase):
    def test_onboarding_and_refresh_are_clean(self):
        with MockTrmnl() as mock, self.boot(memcheck="halt") as s:
            mock.set_png("dots", lambda x, y: (x // 8 + y // 8) & 1)
            mock.display = {"image": "dots", "refresh_rate": 300}
            onboard(s, mock, SSID_24)
            n = len(mock.requests)
            s.touch("center", 150)
            mock.wait_for_request("/api/display", after=n, timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)

            report = s.memcheck()
            self.assertEqual(report["violations"], [])
            heap = report["heap"]
            self.assertGreater(heap["allocs"], 500)
            self.assertGreater(heap["psram"]["peak_bytes"], 1_000_000)  # frame buffers
            stacks = {t["task"]: t for t in report["stacks"]}
            for task in ("loopTask", "sys_evt", "tiT", "IDLE0", "IDLE1"):
                self.assertIn(task, stacks)
            assert_no_low_stacks(self, report)



class MemcheckXBmp(MemcheckXCase):
    @unittest.expectedFailure
    def test_bmp_image_is_flipped_within_its_buffer(self):
        # display_show_image flips an uncompressed BMP with the panel's dimensions: an
        # 800x480 BMP (48 KB) is flipped as 1872x1404, far past the end of its buffer.
        with MockTrmnl() as mock, self.boot(memcheck="log", memcheck_suppress=SNTP_BUG) as s:
            mock.display = {"image": "default", "refresh_rate": 300}  # the OG's BMP
            onboard(s, mock, SSID_24)
            s.assert_no_memory_errors()


if __name__ == "__main__":
    unittest.main()
