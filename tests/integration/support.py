"""Shared fixtures for the integration tests."""

import os
import shutil
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(ROOT / "python"))

from trmnl_mock import MockTrmnl, big_number, expected_gray, png_image  # noqa: E402
from trmnl_sim import Simulator  # noqa: E402

import setup_cache  # noqa: E402
from devices import ANY, DEVICES, Device, by_build_name, under_test  # noqa: E402

# The TRMNL OG's build.
OG_BUILD = Path(os.environ.get("TRMNL_FIRMWARE_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/trmnl"))
BWRY_BUILD = Path(os.environ.get("TRMNL_BWRY_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/trmnl_4clr"))
E1002_BUILD = Path(os.environ.get("TRMNL_E1002_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/seeed_reTerminal_E1002"))
# PlatformIO build directories of the firmware checkout, one per environment
# (TRMNL_FIRMWARE_BUILDS=<checkout>/.pio/build for another checkout).
BUILDS = Path(os.environ.get("TRMNL_FIRMWARE_BUILDS", ROOT.parent / "trmnl-firmware/.pio/build"))


def build_of(env: str) -> Path:
    """The build of PlatformIO environment `env` (it may not exist: see `require_build`)."""
    return BUILDS / env


def build_for_env(env: str) -> Path:
    """The build the tests of PlatformIO environment `env` use (the OG, BWRY, X and E1002
    builds can be moved with their TRMNL_*_BUILD variables)."""
    x_build = Path(os.environ.get("TRMNL_X_BUILD", ROOT.parent / "trmnl-firmware/.pio/build/TRMNL_X"))
    known = {"trmnl": OG_BUILD, "trmnl_4clr": BWRY_BUILD, "TRMNL_X": x_build, "seeed_reTerminal_E1002": E1002_BUILD}
    return known.get(env, build_of(env))


# The device the general tests (ENV = ANY) run on, and its build: TRMNL_SIM_DEVICE=<env>,
# else the TRMNL OG (see devices.py).
DEVICE: Device = under_test()
BUILD = build_for_env(DEVICE.env)


def needs(*features: str):
    """Skip a test (or class) unless the device under test has these `Device` features,
    e.g. @needs("button"), @needs("panel_rev")."""
    import unittest

    missing = [f for f in features if not getattr(DEVICE, f)]
    return unittest.skipIf(bool(missing), f"{DEVICE.name} has no {', '.join(missing)}")


# TRMNL_SIM_SLOW=1 (bin/spec --slow): also run the tests marked @slow.
SLOW = os.environ.get("TRMNL_SIM_SLOW") == "1"


def slow(why: str):
    """Skip a test (or class) that is known to be slow unless TRMNL_SIM_SLOW=1
    (bin/spec --slow); `why` says what takes the time."""
    import unittest

    return unittest.skipUnless(SLOW, f"slow ({why}); bin/spec --slow runs it")


def only_on(*envs: str, why: str):
    """Skip a test (or class) unless the device under test is one of these environments;
    `why` says what makes it specific to them."""
    import unittest

    return unittest.skipUnless(DEVICE.env in envs, f"only on {', '.join(envs)}: {why}")


def skip_if(feature: str, why: str):
    """Skip a test (or class) on devices that have this `Device` feature, e.g.
    @skip_if("shipment", why="the X goes back to shipment mode instead")."""
    import unittest

    return unittest.skipIf(bool(getattr(DEVICE, feature)), f"not on {DEVICE.name}: {why}")


def device_image(mock: MockTrmnl, name: str, black, device: Device | None = None) -> tuple[str, bytes]:
    """Serve a black-and-white image the way the TRMNL server would for `device` (the one
    under test): an 800x480 1-bit BMP for the OG-size panels, else a PNG of the panel's size
    (a palette PNG for color panels). `black(x, y)` says which pixels are ink. Returns the
    path the device downloads and the screenshot it should give."""
    d = device or DEVICE
    w, h = d.size
    if d.default_bmp and d.inks == "mono":
        return f"/images/{name}.bmp", mock.set_image(name, black)
    if d.inks == "bwry":
        return f"/images/{name}.png", mock.set_color_png(name, lambda x, y: (0, 0, 0) if black(x, y) else (255, 255, 255), w, h)
    if d.inks == "spectra6":
        return f"/images/{name}.png", mock.set_spectra6_png(name, lambda x, y: (0, 0, 0) if black(x, y) else (255, 255, 255), w, h)

    def level(x, y):
        return 0 if black(x, y) else 1

    mock.images[name + ".png"] = png_image(level, w, h, bits=1)
    mock._stamp(name)
    return f"/images/{name}.png", expected_gray(level, w, h, bits=1)


def panel_number(text: str, scale: int = 24, device: Device | None = None):
    """`big_number(text)` centred on the panel of `device` (the one under test) instead of
    on 800x480: pixel(x, y) is True for ink."""
    w, h = (device or DEVICE).size
    px = big_number(text, scale)
    dx, dy = (800 - w) // 2, (480 - h) // 2
    return lambda x, y: px(x + dx, y + dy)


def served_path(mock: MockTrmnl, name: str) -> str:
    """The path the device downloads image `name` of `mock` from (as its /api/display answer
    says: images/<name>.png if there is such a PNG, else the BMP)."""
    return f"/images/{name}.png" if name + ".png" in mock.images else f"/images/{name}.bmp"


def device_mock(tls: bool = False, device: Device | None = None) -> MockTrmnl:
    """A MockTrmnl whose default screen is what the TRMNL server would send `device` (the one
    under test): the OG's 800x480 BMP, else a 1-bit PNG of the panel's size (as
    ProvisionedDevice serves)."""
    d = device or DEVICE
    mock = MockTrmnl(tls=tls)
    if not d.default_bmp:
        w, h = d.size
        number = panel_number("0", device=d)
        mock.images["default.png"] = png_image(lambda x, y: 0 if number(x, y) else 1, w, h, bits=1)
    return mock


def image_size(mock: MockTrmnl, path: str) -> int:
    """The size of the image `mock` serves at `path` (from device_image): e.g. to cut its
    download short halfway, whatever the panel."""
    return len(mock.images[path[len("/images/"):].removesuffix(".bmp")])


def partition_table(flash: bytes) -> list[dict]:
    """The partitions in a flash image (the table at 0x8000): label, type, subtype, offset,
    size."""
    import struct

    parts = []
    for off in range(0x8000, 0x9000, 32):
        magic, ptype, subtype, start, size, label = struct.unpack_from("<HBBII16s", flash, off)
        if magic != 0x50AA:
            break
        parts.append({"label": label.split(b"\0")[0].decode(), "type": ptype, "subtype": subtype,
                      "offset": start, "size": size})
    return parts


def ota_slot_label(flash: bytes, n: int) -> str:
    """The label of app slot ota_`n` in a flash image's partition table."""
    return next(p["label"] for p in partition_table(flash) if p["type"] == 0 and p["subtype"] == 0x10 + n)


def boot_slot(flash: bytes) -> str:
    """The label of the app partition the bootloader boots from a flash image, from otadata
    the way the IDF bootloader reads it: the valid entry with the highest sequence number
    picks slot (seq - 1) mod the number of slots; none valid, the factory app or ota_0."""
    import struct
    import zlib

    parts = partition_table(flash)
    apps = sorted((p for p in parts if p["type"] == 0 and 0x10 <= p["subtype"] < 0x20), key=lambda p: p["subtype"])
    factory = [p for p in parts if p["type"] == 0 and p["subtype"] == 0]
    otadata = next(p for p in parts if p["type"] == 1 and p["subtype"] == 0)
    seqs = []
    for sector in (0, 0x1000):
        seq, _, state, crc = struct.unpack_from("<I20sII", flash, otadata["offset"] + sector)
        # ESP_OTA_IMG_INVALID (3) / ABORTED (4) entries don't count
        if seq != 0xFFFFFFFF and crc == zlib.crc32(struct.pack("<I", seq), 0xFFFFFFFF) and state not in (3, 4):
            seqs.append(seq)
    if not seqs:
        return (factory or apps)[0]["label"]
    return apps[(max(seqs) - 1) % len(apps)]["label"]


def require_build(build: Path):
    """Skip the calling module (from setUpModule) when `build` hasn't been built."""
    import unittest

    if not (build / "firmware.elf").exists():
        raise unittest.SkipTest(f"no {build.name} build at {build} (pio run -e {build.name})")
GOLDEN = HERE / "golden"
# The golden screenshots of devices other than the TRMNL OG (whose goldens and regions the
# tests name): golden/<env>/<name>, compared in the region given here ({env: {name: (x, y,
# w, h)}}). See `golden`.
GOLDEN_REGIONS: dict[str, dict[str, tuple[int, int, int, int]]] = {
    "TRMNL_X": {
        # all but the "TRMNL firmware <version> (<git hash>)" line
        "setup_screen_body.png": (0, 64, 1872, 1340),
        "setup_screen_top_right.png": (640, 0, 1232, 64),
        # 'Connect your phone or computer to "TRMNL-000001" Wi-Fi'
        "setup_ssid_line.png": (400, 1236, 1072, 48),
        # "Can't establish WiFi connection. Will keep trying." (the X's font has the apostrophe)
        "wifi_failed_message.png": (480, 1160, 912, 48),
        # "WiFi connected, unable connect to API." and how to retry (tap the touch bar)
        "api_unable_to_connect.png": (580, 306, 712, 136),
    },
}


def golden(name: str, region: tuple[int, int, int, int]) -> tuple[Path, tuple[int, int, int, int]]:
    """The golden screenshot `name` and the region to compare it in, for the device under
    test: the OG's (golden/<name>, `region`), or the device's own from GOLDEN_REGIONS. A
    device without one skips the test. Use as `s.assert_screen(*golden(name, region))`."""
    import unittest

    if DEVICE.env == "trmnl":
        return GOLDEN / name, region
    own = GOLDEN_REGIONS.get(DEVICE.env, {}).get(name)
    if own is None:
        raise unittest.SkipTest(f"no {name} golden for {DEVICE.name}")
    return GOLDEN / DEVICE.env / name, own
TEST_MAC = "7C:DF:A1:00:00:01"
NETWORK = os.environ.get("TRMNL_SIM_NETWORK") == "1"
# Turbo (network-aware fast-forward) unless TRMNL_SIM_REALTIME=1.
TURBO = os.environ.get("TRMNL_SIM_REALTIME") != "1"
# TRMNL_SIM_MEMCHECK=1: every simulator runs with --memcheck=halt, and a test fails on
# any memory error (the simulator halts at it, or leaving the `with` block raises).
MEMCHECK = "halt" if os.environ.get("TRMNL_SIM_MEMCHECK") == "1" else None
# Firmware memory bugs memcheck found, tolerated so the rest of a run is still checked
# (test_memcheck.py has an expected failure for each, which turns into an unexpected
# success once the bug is fixed):
KNOWN_MEMORY_BUGS = (
    # Clock::sync passes a String's c_str() to configTime(), which keeps the pointer for
    # SNTP; the String is freed when setTimeFromNTP returns, and later SNTP retries resolve
    # the freed name (heap-use-after-free in dns_gethostbyname on the tiT task). Matched by
    # the free (while the block is still free) and by the reading side.
    "_ZN5Clock14setTimeFromNTPEv",
    "sntp_request",
    # display_show_image flips an uncompressed BMP as if it were panel-sized: on the X an
    # 800x480 BMP makes flip_image read and write ~280 KB past the 48 KB buffer.
    "_Z10flip_imagePhiib",
    # ...and sends it as the panel's plane: on the BWRY (2 bits/pixel) writePlane reads a
    # 1-bit 800x480 BMP's 48 KB buffer as 96 KB.
    "_Z18bbepWriteImage2bppP10bbepstructh",
    # HttpRetryRequest::bodyAsString uses String::concat(buf, len) on a body that isn't
    # NUL-terminated; concat copies len + 1 bytes, reading one byte past the malloc'd body
    # (and leaving that byte, not a NUL, after the text). Seen when the server sends a
    # Content-Length (the built-in mock server does).
    "_ZNK16HttpRetryRequest12bodyAsStringEv",
)


# Firmware bugs that keep general tests from passing on some devices, for the modules'
# KNOWN_FAILURES (each has a device-specific test of its own too).
SSD16XX_BMP_BUG = (
    "firmware: display_show_image() sends a 1-bit BMP to the SSD16xx's new-image RAM only "
    "(src/display.cpp:1947 writePlane()) and asks for a partial refresh (display.cpp:1949); a "
    "partial refresh is differential against the old-image RAM, which after the full or fast "
    "refresh at boot doesn't hold what's on screen, so the BMP never appears "
    "(test_byod_ssd.SsdBoard.test_bmp_after_a_fast_refresh)")
ONE_BIT_PNG_PANEL_TYPE_BUG = (
    "firmware: png_to_epd() calls bbep.setPanelType(dpList[...].OneBit) for 1-bit PNGs "
    "(src/display.cpp:1764) on boards brought up with bbep.begin(<product>), passing a product "
    "number as a panel type: the image is drawn for another panel and never shows "
    "(test_byod_ssd.CrowPanel42.test_shows_the_served_image)")
GEN2_NTP_HANG_BUG = (
    "firmware: ClockGen2::waitForSync() (src/misc/clock/clock_gen2.cpp:9) loops until the clock "
    "reads 2020 or later, with no timeout: without an NTP server (no internet, no DNS) the "
    "device never gets past the time sync and never sleeps")


def _current_test_id() -> str:
    """Name artifacts after the running test (found by walking the call stack)."""
    import inspect
    import unittest

    for f in inspect.stack():
        obj = f.frame.f_locals.get("self") or f.frame.f_locals.get("cls")
        if isinstance(obj, unittest.TestCase):
            return obj.id()
        if isinstance(obj, type) and issubclass(obj, unittest.TestCase):
            return f"{obj.__module__}.{obj.__name__}"
    return "sim"


def device_of(build: Path) -> Device | None:
    """The device a build is for (None: unknown)."""
    build = Path(build)
    if build == BUILD:
        return DEVICE
    return by_build_name(build.name) or next((d for d in DEVICES.values() if build_for_env(d.env) == build), None)


def sim(build: Path = BUILD, **kw) -> Simulator:
    """A simulator of `build` (default: the device under test's). A device that doesn't get
    from an erased flash to its setup portal on its own (the X: factory flow, shipment
    mode, dock) boots its "unboxed" state for `erase=True` instead, and a device without a
    button maps presses to its equivalent (see support_x.XSim)."""
    kw.setdefault("name", _current_test_id())
    kw.setdefault("mac", TEST_MAC)
    kw.setdefault("turbo", TURBO)
    kw.setdefault("memcheck", MEMCHECK)
    kw.setdefault("memcheck_suppress", KNOWN_MEMORY_BUGS)
    d = device_of(build)
    if d is not None and d.env == "TRMNL_X":
        import support_x

        return support_x.general_sim(build, **kw)
    return Simulator(build, **kw)


_built_fixtures: list = []


def fixture(make):
    """A module fixture built on first use, so a worker running one class of a module
    (run.py splits modules that set PARALLEL_BY_CLASS) only builds what that class needs.
    Call the returned function to get it; close_fixtures() (in tearDownModule) closes the
    ones built."""
    value = None

    def get():
        nonlocal value
        if value is None:
            value = make()
            _built_fixtures.append(value)
        return value

    return get


def close_fixtures():
    """Close the fixtures built so far, newest first."""
    while _built_fixtures:
        _built_fixtures.pop().close()


class ProvisionedDevice:
    """A mock API server plus a device that already completed onboarding against a server
    like it: `boot()` powers on a copy of its flash, `boot_asleep()` resumes it in the deep
    sleep that followed onboarding (a save point), skipping the first refresh cycle.

    The onboarded device comes from the setup cache (see setup_cache). It knows the server
    by the port of the mock it was onboarded with, so every simulator started here gets
    `--host-port` to send that port to this fixture's mock."""

    NAME = "provisioned"
    SIM_ARGS: tuple[str, ...] = ("--offline",)
    DEVICE_HOST = "10.0.2.2"

    def __init__(self, build: Path | None = None, panel_size: tuple[int, int] | None | object = ...):
        """`build`: default, the device under test's. `panel_size`: serve the default image as
        a 1-bit PNG of that size (as the TRMNL server does for other panels) instead of the
        OG's 800x480 BMP; default, what the build's device takes (see Device.default_bmp)."""
        build = build or BUILD
        if panel_size is ...:
            d = device_of(build)
            panel_size = None if d is None or d.default_bmp else d.size
        self.build = build
        self.panel_size = panel_size
        inputs = {"build": setup_cache.build_id(build), "memcheck": MEMCHECK, "turbo": TURBO, "args": self.SIM_ARGS, "host": self.DEVICE_HOST}
        if panel_size:
            inputs["panel_size"] = list(panel_size)
        self.cache, meta = setup_cache.entry(f"{self.NAME}-{build.name}", inputs, self._onboard)
        self.mock = self.new_mock()
        self.host_ports = {meta["port"]: self.mock.port}
        self.dir = Path(tempfile.mkdtemp(prefix="trmnl-provisioned-"))

    def new_mock(self) -> MockTrmnl:
        """A mock server for this device (onboarding and tests): serving images/default
        (panel-sized with `panel_size`) and a 300 s refresh rate. Subclasses change what
        it serves."""
        mock = MockTrmnl()
        if self.panel_size:
            w, h = self.panel_size
            number = big_number("0")
            mock.images["default.png"] = png_image(lambda x, y: 0 if number(x, y) else 1, w, h, bits=1)
        mock.device_host = self.DEVICE_HOST
        mock.display = {"image": "default", "refresh_rate": 300}
        return mock

    def _onboard(self, out: Path) -> dict:
        with self.new_mock() as mock:
            with sim(self.build, flash=out / "flash.bin", erase=True, extra_args=self.SIM_ARGS, name=f"{self.NAME}-setup") as s:
                s.wait(portal=True, timeout_s=90)
                s.portal_connect("TRMNL-Sim", "password", server=mock.device_url)
                mock.wait_for_request("/api/display", timeout_s=120)
                # a slow panel (Spectra 6) can still be refreshing when the chip sleeps
                s.wait(state="deep_sleep", display_idle=True, timeout_s=120)
                s.save_point(out / "asleep.trmnlsave", label="onboarded, asleep")
            return {"port": mock.port}

    def _sim(self, **kw) -> Simulator:
        extra = tuple(kw.pop("extra_args", ())) + self.SIM_ARGS
        return sim(self.build, extra_args=extra, host_ports=self.host_ports, **kw)

    def boot(self, **kw) -> Simulator:
        """Power on a copy of the provisioned device."""
        flash = self.dir / f"flash-{len(list(self.dir.iterdir()))}.bin"
        shutil.copy(self.cache / "flash.bin", flash)
        return self._sim(flash=flash, **kw)

    def boot_asleep(self, **kw) -> Simulator:
        """The provisioned device in deep sleep right after onboarding (showing the mock's
        default image); wake it, press, touch... to carry on."""
        return self.restore(self.cache / "asleep.trmnlsave", **kw)

    def restore(self, path, **kw) -> Simulator:
        """A simulator resumed from a save point of this device (reaching this mock)."""
        return self._sim(restore=path, **kw)

    def close(self):
        self.mock.close()
        shutil.rmtree(self.dir, ignore_errors=True)


__all__ = ["BUILD", "SLOW", "slow", "OG_BUILD", "DEVICE", "DEVICES", "ANY", "needs", "only_on", "device_image", "panel_number", "served_path", "device_mock", "BUILDS", "build_of", "build_for_env", "require_build", "BWRY_BUILD", "E1002_BUILD", "GOLDEN", "TEST_MAC", "NETWORK", "MEMCHECK", "KNOWN_MEMORY_BUGS", "sim", "fixture", "close_fixtures", "ProvisionedDevice", "MockTrmnl", "big_number", "Simulator", "skip_if", "image_size", "device_of", "GOLDEN_REGIONS", "golden", "partition_table", "ota_slot_label", "boot_slot", "SSD16XX_BMP_BUG", "ONE_BIT_PNG_PANEL_TYPE_BUG", "GEN2_NTP_HANG_BUG"]
