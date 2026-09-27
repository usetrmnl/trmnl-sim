"""--memcheck: whole onboarding and refresh cycles of the OG and the X under the memory
checker (heap use-after-free, overflows, bad frees; stack high-water marks).

Firmware bugs it found are in support.KNOWN_MEMORY_BUGS: the clean-cycle tests tolerate
them (so everything else is still checked), and each has an expected failure here."""

import shutil
import tempfile
import unittest
from pathlib import Path

import setup_cache
from support import BUILD, BWRY_BUILD, DEVICE, TURBO, MockTrmnl, big_number, device_image, sim
from support_x import SSID_24, X_BUILD, onboard, x_sim

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

SNTP_BUG = ("_ZN5Clock14setTimeFromNTPEv", "sntp_request")


def fixed_on(*envs: str, why: str):
    """An expected failure, except on these environments, where the bug doesn't show (`why`)."""
    return lambda test: test if DEVICE.env in envs else unittest.expectedFailure(test)


def assert_no_low_stacks(test: unittest.TestCase, report: dict) -> None:
    low = [(t["task"], t["min_free"]) for t in report["stacks"] if t["low"]]
    test.assertEqual(low, [], "tasks close to overflowing their stacks")


class MemcheckOG(unittest.TestCase):
    def onboard(self, s, mock):
        s.wait(portal=True, timeout_s=90)
        s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
        mock.wait_for_request("/api/display", timeout_s=120)
        s.wait(state="deep_sleep", timeout_s=120)

    def test_onboarding_and_refresh_cycles_are_clean(self):
        with MockTrmnl() as mock, sim(BUILD, erase=True, memcheck="halt", extra_args=("--offline",)) as s:
            device_image(mock, "one", big_number("1"))
            mock.display = {"image": "one", "refresh_rate": 300}
            self.onboard(s, mock)
            n = len(mock.requests)
            s.wake()
            mock.wait_for_request("/api/display", after=n, timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=90)
            n = len(mock.requests)
            s.press(150)
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
            else:
                self.assertEqual(heap["psram"]["peak_bytes"], 0)
            stacks = {t["task"]: t for t in report["stacks"]}
            self.assertEqual(stacks["loopTask"]["size"], 8192)
            self.assertGreater(stacks["loopTask"]["max_used"], 1000)
            self.assertGreaterEqual(stacks["loopTask"]["instances"], 3)  # one per boot
            assert_no_low_stacks(self, report)

    @unittest.expectedFailure
    def test_sntp_server_name_is_not_used_after_free(self):
        # Clock::sync hands configTime() the c_str() of a String that setTimeFromNTP frees on
        # return; SNTP keeps the pointer and resolves it again on every retry.
        with MockTrmnl() as mock, sim(BUILD, erase=True, memcheck="log", memcheck_suppress=(),
                                      extra_args=("--offline",)) as s:
            self.onboard(s, mock)
            s.assert_no_memory_errors()

    @fixed_on("TRMNL_X", why="Arduino 3's String::concat(buf, len) copies len bytes, not len + 1")
    def test_api_display_body_is_not_read_past_its_end(self):
        # bodyAsString() calls String::concat(body, size), which copies size + 1 bytes of a
        # body that isn't NUL-terminated. The built-in server sends a Content-Length, which
        # takes the path with a malloc'd body.
        with sim(BUILD, erase=True, memcheck="log", memcheck_suppress=SNTP_BUG, extra_args=("--offline",)) as s:
            url = s.mock.start()
            s.mock.display(refresh_rate=300)
            s.wait(portal=True, timeout_s=90)
            s.portal_connect("TRMNL-Sim", "password", server=url)
            s.mock.wait_for_request("/api/display", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)
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
