"""The devices the integration tests know, by PlatformIO environment, and the device under
test.

General tests (modules or classes with `ENV = ANY`: setup, portal, WiFi, HTTP, errors,
faults...) run against whichever device is under test: TRMNL_SIM_DEVICE=<env> (run.py sets
it for `bin/spec <env>`), else the TRMNL OG. They read what they need to know about it from
its `Device` (panel size, inks, button, battery...), skip what doesn't apply (`needs`,
`only_on` in support.py), and a module's `KNOWN_FAILURES` marks what a device's firmware
gets wrong.
"""

import os
from dataclasses import dataclass

# `ENV = ANY`: the tests run on any device (the one under test).
ANY = "*"


@dataclass(frozen=True)
class Device:
    env: str
    """PlatformIO environment (the build directory's name)."""
    model: str
    """The firmware's DEVICE_MODEL (the Model header)."""
    name: str
    """The simulator's board name."""
    size: tuple[int, int] = (800, 480)
    """The panel as the device reports it (Width/Height headers)."""
    inks: str = "mono"
    """mono, bwr, bwry, spectra6 or gray16."""
    chip: str = "esp32s3"
    battery_v: float | None = 4.1
    """Battery-Voltage it reports at the simulator's default 4.1 V battery (None: varies)."""
    button: bool = True
    """Has a button the simulator can press (and that wakes it from deep sleep)."""
    panel_rev: bool = False
    """Reads the panel revision (Panel-Rev header)."""
    general: str | None = None
    """Why the general tests can't run on it (None: they can)."""

    @property
    def default_bmp(self) -> bool:
        """Takes the TRMNL server's 800x480 1-bit BMP as the default screen (other panels get
        a PNG of their size)."""
        return self.size == (800, 480) and self.inks in ("mono", "bwry")


_DEVICES = [
    Device("trmnl", "og", "TRMNL OG", chip="esp32c3", panel_rev=True),
    Device("trmnl_4clr", "og_4clr", "TRMNL BWRY", inks="bwry", chip="esp32c3"),
    Device("TRMNL_X", "x", "TRMNL X", size=(1872, 1404), inks="gray16", battery_v=None, button=False,
           general="onboarding goes through the factory flow and shipment mode; see test_trmnl_x"),
    Device("seeed_reTerminal_E1002", "reterminal_e1002", "reTerminal E1002", inks="spectra6"),
    Device("seeed_xiao_esp32c3", "seeed_esp32c3", "XIAO ESP32-C3 + 7.5\" panel", chip="esp32c3", battery_v=0.0),
    Device("seeed_xiao_esp32s3", "seeed_esp32s3", "XIAO ESP32-S3 + 7.5\" panel", battery_v=0.0),
    Device("TRMNL_7inch5_OG_DIY_Kit", "xiao_epaper_display", "TRMNL 7.5\" DIY Kit"),
    Device("TRMNL_7inch5_OG_DIY_Kit_3CLR", "xiao_epaper_3clr", "TRMNL 7.5\" BWR DIY Kit", inks="bwr"),
    Device("TRMNL_7inch5_OG_DIY_Kit_6CLR", "xiao_epaper_6clr", "TRMNL 7.3\" Spectra 6 DIY Kit", inks="spectra6"),
    Device("TRMNL_4inch26_DIY_Kit", "xiao_epaper_mini", "TRMNL 4.26\" DIY Kit"),
    Device("seeed_reTerminal_E1001", "reterminal_e1001", "reTerminal E1001"),
    Device("seeed_reTerminal_E1004", "reterminal_e1004", "reTerminal E1004", size=(1200, 1600), inks="spectra6"),
    Device("seeed_sticky", "seeed_sticky", "Seeed Sticky"),
    Device("xteink_x4", "xteink_x4", "Xteink X4", chip="esp32c3", battery_v=0.0),
    Device("xteink_x3", "xteink_x3", "Xteink X3", size=(792, 528), chip="esp32c3"),
    Device("WAVESHARE_397", "waveshare_397", "Waveshare ESP32-S3 3.97\""),
    Device("CrowPanel42", "crowpanel42", "CrowPanel 4.2\"", size=(400, 300), battery_v=4.2),
    Device("m5_paper_mono", "m5_paper_mono", "M5Paper Mono", battery_v=4.2),
    Device("m5_paper_color", "m5_paper_color", "M5Paper Color", size=(400, 600), inks="spectra6", battery_v=4.2),
    Device("TRMNL_X_PAPERS3", "m5_papers3", "M5Stack PaperS3", size=(960, 540), inks="gray16", button=False),
    Device("TRMNL_X_LILYGO_T5PRO", "lilygo_t5pro", "LilyGo T5 4.7\" S3 Pro", size=(960, 540), inks="gray16"),
    Device("TRMNL_X_SENSORIAC5", "sensoria_c5", "Sensoria C5", size=(1280, 720), inks="gray16", chip="esp32c5",
           battery_v=0.0, general="the firmware reboots instead of sleeping (see test_byod_parallel.SensoriaC5)"),
    Device("trmnl_steam", "trmnl_steam", "TRMNL Steam", size=(648, 480), chip="esp32c3",
           general="the firmware never boots (see test_byod_uc81xx.TrmnlSteamBoots)"),
    Device("trmnl_gen2", "og_gen2", "TRMNL OG gen 2", chip="esp32c5"),
    Device("trmnl_gen2_4clr", "og_gen2_4clr", "TRMNL BWRY gen 2", inks="bwry", chip="esp32c5"),
]

DEVICES: dict[str, Device] = {d.env: d for d in _DEVICES}


def device(env: str) -> Device:
    """The device of PlatformIO environment `env` (case-insensitive)."""
    if env in DEVICES:
        return DEVICES[env]
    for d in _DEVICES:
        if d.env.lower() == env.lower():
            return d
    raise KeyError(f"unknown device {env!r} (known: {', '.join(DEVICES)})")


def by_build_name(name: str) -> Device | None:
    """The device a build directory (named after its environment) is for."""
    return DEVICES.get(name)


def under_test() -> Device:
    """The device the general tests run on: TRMNL_SIM_DEVICE, else the TRMNL OG."""
    return device(os.environ.get("TRMNL_SIM_DEVICE") or "trmnl")


# ---- how much of the general suite runs where (run.py tiers) ----------------------------------
#
# Devices share firmware code paths by chip, panel controller and inks. `bin/spec` runs
# every device's own tests, the full general suite on the OG, and SMOKE (one test per area)
# on every other device; `--comprehensive` runs the full general suite on one representative
# per family as well; `--exhaustive`, on every device.

FAMILIES: dict[str, list[str]] = {
    # family: its devices, the representative first
    "ESP32-C3, UC81xx, black and white": ["trmnl", "seeed_xiao_esp32c3", "xteink_x3", "trmnl_steam"],
    "ESP32-C3, UC81xx, 4-color": ["trmnl_4clr"],
    "ESP32-C3, SSD16xx": ["xteink_x4"],
    "ESP32-S3, UC81xx, black and white": ["seeed_reTerminal_E1001", "seeed_xiao_esp32s3", "TRMNL_7inch5_OG_DIY_Kit",
                                          "TRMNL_7inch5_OG_DIY_Kit_3CLR"],
    "ESP32-S3, UC81xx, Spectra 6": ["seeed_reTerminal_E1002", "TRMNL_7inch5_OG_DIY_Kit_6CLR", "seeed_reTerminal_E1004",
                                    "m5_paper_color"],
    "ESP32-S3, SSD16xx": ["CrowPanel42", "TRMNL_4inch26_DIY_Kit", "WAVESHARE_397", "seeed_sticky", "m5_paper_mono"],
    "ESP32-S3, parallel, TRMNL X": ["TRMNL_X"],
    "ESP32-S3, parallel, BYOD": ["TRMNL_X_PAPERS3", "TRMNL_X_LILYGO_T5PRO"],
    "ESP32-C5": ["trmnl_gen2", "trmnl_gen2_4clr", "TRMNL_X_SENSORIAC5"],
}

REPRESENTATIVES = [devices[0] for devices in FAMILIES.values()]

# One general test per area, run on every device (test ids: module.Class.test).
SMOKE = [
    "test_setup_mode.FreshDevice.test_setup_screen_names_the_access_point",
    "test_setup_mode.Onboarding.test_onboarding_registers_with_server",
    "test_setup_mode.Onboarding.test_wrong_password_shows_wifi_error_and_sleeps",
    "test_portal.ScanList.test_access_points_are_merged_by_ssid",
    "test_refresh_cycle.RefreshCycle.test_reports_device_identity",
    "test_refresh_cycle.RefreshCycle.test_reports_battery_voltage",
    "test_refresh_cycle.RefreshCycle.test_image_is_rendered_exactly",
    "test_refresh_cycle.RefreshCycle.test_timer_wake_fetches_next_image",
    "test_refresh_cycle.RefreshCycle.test_button_press_wakes_and_refreshes",
    "test_refresh_cycle.RefreshCycle.test_credentials_survive_power_cycle",
    "test_refresh_cycle.FirmwareUpdate.test_ota_update_installs_and_boots_other_slot",
    "test_images.Png.test_2bit_png_uses_4_gray_levels",
    "test_https.OtherServer.test_onboarding_and_refreshes_over_https",
    "test_faults.ServerErrors.test_http_500_from_api_display_is_retried_then_sleeps",
    "test_errors.ErrorScreens.test_api_unreachable",
    "test_savepoints.SavePoints.test_timer_wake_refreshes_without_onboarding",
    "test_special_functions.SpecialFunctions.test_identify_shows_the_identify_image",
]
