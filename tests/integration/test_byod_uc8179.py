"""BYOD boards with the UC8179 panels the TRMNL OG family uses: 7.5" black and white and
7.3" Spectra 6, on XIAO ESP32-C3/S3 boards, the TRMNL DIY kits and the reTerminal E1001."""

import unittest

from support_byod import ByodBoard

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker


class XiaoEsp32c3(ByodBoard, unittest.TestCase):
    ENV = "seeed_xiao_esp32c3"
    NAME = "XIAO ESP32-C3 + 7.5\" panel"
    MODEL = "seeed_esp32c3"
    BATTERY_V = 0.0  # batt_pin 0xff: nothing to read


class XiaoEsp32s3(ByodBoard, unittest.TestCase):
    # main's seeed_xiao_esp32s3 env lacks `framework = arduino`; build it with that added
    ENV = "seeed_xiao_esp32s3"
    NAME = "XIAO ESP32-S3 + 7.5\" panel"
    MODEL = "seeed_esp32s3"
    BATTERY_V = 0.0


class DiyKit75(ByodBoard, unittest.TestCase):
    ENV = "TRMNL_7inch5_OG_DIY_Kit"
    NAME = "TRMNL 7.5\" DIY Kit"
    MODEL = "xiao_epaper_display"


class DiyKitSpectra6(ByodBoard, unittest.TestCase):
    ENV = "TRMNL_7inch5_OG_DIY_Kit_6CLR"
    NAME = "TRMNL 7.3\" Spectra 6 DIY Kit"
    MODEL = "xiao_epaper_6clr"
    INKS = "spectra6"


class ReTerminalE1001(ByodBoard, unittest.TestCase):
    ENV = "seeed_reTerminal_E1001"
    NAME = "reTerminal E1001"
    MODEL = "reterminal_e1001"


if __name__ == "__main__":
    unittest.main()
