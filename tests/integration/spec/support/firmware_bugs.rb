# frozen_string_literal: true

# Firmware bugs the tests know about. A test that fails because the firmware is wrong stays in
# and says so (`known_failure:` / `pending:` metadata) with one of these reasons, or its own.
module FirmwareBugs
  # Firmware memory bugs memcheck found, tolerated so the rest of a run is still checked
  # (the memcheck specs, general/tooling/ and with the devices, have a pending example for each,
  # which fails once the bug is fixed):
  KNOWN_MEMORY_BUGS = [
    # Clock::sync passes a String's c_str() to configTime(), which keeps the pointer for SNTP;
    # the String is freed when setTimeFromNTP returns, and later SNTP retries resolve the freed
    # name (heap-use-after-free in dns_gethostbyname on the tiT task). Matched by the free
    # (while the block is still free) and by the reading side.
    "_ZN5Clock14setTimeFromNTPEv",
    "sntp_request",
    # display_show_image flips an uncompressed BMP as if it were panel-sized: on the X an
    # 800x480 BMP makes flip_image read and write ~280 KB past the 48 KB buffer.
    "_Z10flip_imagePhiib",
    # ...and sends it as the panel's plane: on the BWRY (2 bits/pixel) writePlane reads a 1-bit
    # 800x480 BMP's 48 KB buffer as 96 KB.
    "_Z18bbepWriteImage2bppP10bbepstructh",
    # HttpRetryRequest::bodyAsString uses String::concat(buf, len) on a body that isn't
    # NUL-terminated; concat copies len + 1 bytes, reading one byte past the malloc'd body (and
    # leaving that byte, not a NUL, after the text). Seen when the server sends a
    # Content-Length (the built-in mock server does).
    "_ZNK16HttpRetryRequest12bodyAsStringEv",
    # display_show_msg_qa copies a panel-sized 1-bit frame from 62 bytes into startQA()'s
    # 48000-byte buffer (factory QA only): 62 bytes past its end on 800x480 panels.
    "_Z19display_show_msg_qaPhPKfS1_b"
  ].freeze
  # The SNTP use-after-free's functions (the first KNOWN_MEMORY_BUGS), for examples that check
  # for the others.
  SNTP_USE_AFTER_FREE = KNOWN_MEMORY_BUGS.first(2).freeze

  # Firmware bugs that keep general tests from passing on some devices (each has a
  # device-specific test of its own too).
  SSD16XX_BMP =
    "firmware: display_show_image() sends a 1-bit BMP to the SSD16xx's new-image RAM only " \
    "(src/display.cpp:1947 writePlane()) and asks for a partial refresh (display.cpp:1949); a " \
    "partial refresh is differential against the old-image RAM, which after the full or fast " \
    "refresh at boot doesn't hold what's on screen, so the BMP never appears " \
    "(devices/byod/ssd16xx_spec SsdBoard \"bmp after a fast refresh\")"
  ONE_BIT_PNG_PANEL_TYPE =
    "firmware: png_to_epd() calls bbep.setPanelType(dpList[...].OneBit) for 1-bit PNGs " \
    "(src/display.cpp:1764) on boards brought up with bbep.begin(<product>), passing a product " \
    "number as a panel type: the image is drawn for another panel and never shows " \
    "(devices/byod/ssd16xx_spec CrowPanel42 \"shows the served image\")"
  GEN2_4CLR_IMAGE =
    "firmware: the trmnl_gen2_4clr env (platformio.ini:910) defines BOARD_TRMNL_GEN2 but not " \
    "BOARD_TRMNL_4CLR, which the 4-color image path is compiled under (src/display.cpp:1752): " \
    "images go out as two 1-bit planes the BWRY panel reads as 2 bits per pixel, so they never " \
    "show right (devices/og_gen2_spec OgGen2Bwry)"
  BMP_FLIP_OVERFLOW =
    "firmware: display_show_image() flips an uncompressed BMP with the panel's dimensions " \
    "(src/display.cpp:1940 flip_image(image_buffer+62, bbep.width(), bbep.height())): on a " \
    "panel bigger than 800x480 it writes past the BMP's 48062-byte buffer (52272 bytes on " \
    "792x528, 240000 on 1200x1600), corrupting the heap: the next free crashes"
  EP397_ROW_SHIFT =
    "firmware (bb_epaper 2.1.9, the Waveshare 3.97\"'s): EP397_800x480's init sequences make the " \
    "RAM Y address count down from 479 but start its counter at 0, so everything shows one row " \
    "too high, its top row at the bottom (devices/byod/ssd16xx_spec Waveshare397)"
  XIAO_C3_BUTTON_WAKE =
    "firmware: the XIAO ESP32-C3's device_list[] row puts the button on GPIO 9 (src/display.cpp:56), " \
    "and goto sleep enables it as a deep-sleep wakeup (src/bl.cpp:2303), which the C3 only has on " \
    "GPIO 0-5: the IDF refuses (\"gpio 9 is an invalid deep sleep wakeup IO\") and the button " \
    "never wakes the device"
  # ...and bl_init() waits 2 s (bl.cpp:731-733) before it reads the button, so a wake press is over
  # by then, and classify_button_presses() (button.cpp:66-71) then waits for another press with no
  # timeout: were it woken, the device would stay awake until pressed again.
  XIAO_C3_BUTTON =
    "XIAO C3: its button is GPIO 9 (display.cpp:56), which can't wake a C3 from deep sleep " \
    "(bl.cpp:2303's esp_deep_sleep_enable_gpio_wakeup fails); and were it woken, bl.cpp:731-733's 2 s " \
    "delay outlasts the press and button.cpp:66-71 then waits for another press forever"
  GEN2_NTP_HANG =
    "firmware: ClockGen2::waitForSync() (src/misc/clock/clock_gen2.cpp:9) loops until the clock " \
    "reads 2020 or later, with no timeout: without an NTP server (no internet, no DNS) the " \
    "device never gets past the time sync and never sleeps"

  # The devices a served image doesn't show right on (for known_failure:), and why. The
  # Waveshare 3.97"'s BMPs also hit SSD16XX_BMP.
  WRONG_IMAGES = {
    %w[xteink_x4 TRMNL_4inch26_DIY_Kit seeed_sticky] => SSD16XX_BMP,
    "CrowPanel42" => ONE_BIT_PNG_PANEL_TYPE,
    "WAVESHARE_397" => EP397_ROW_SHIFT,
    "trmnl_gen2_4clr" => GEN2_4CLR_IMAGE
  }.freeze
end
