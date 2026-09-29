# frozen_string_literal: true

require "json"

# Error handling on the device under test: the error screens (and their weak-WiFi variants),
# quiet retries on timer wakes, /api/setup failures during onboarding, and factory QA.

# Firmware bugs
message_below_panel =
  "firmware: display_show_msg() draws the error text at fixed rows from 340 down " \
  "(src/display.cpp:2319 for API_UNABLE_TO_CONNECT), below the bottom of a 300-row panel: " \
  "the error screen is the bare logo"
unregistered_below_panel =
  "firmware: display_show_msg() draws the 'MAC ... not registered' message from row 340 down " \
  "(src/display.cpp:2737, MAC_NOT_REGISTERED), below the bottom of a 300-row panel"
qa_results_below_panel =
  "firmware: display_show_msg_qa() draws the voltages, temperatures and verdict at rows " \
  "340/370/400 (src/display.cpp:2651, 2655, 2663), below the bottom of a 300-row panel"
qa_1bpp_buffer_on_color_panel =
  "firmware: display_show_msg_qa() copies startQA()'s white 1-bit buffer (src/qa.cpp:298-299) " \
  "into the frame buffer with a 1-bpp size (src/display.cpp:2596, 2622), but a color panel's " \
  "buffer has 2 (4-color) or 4 (Spectra 6) bits per pixel: the 0xFF bytes cover the top half " \
  "(in red) or quarter of the screen, and the rest keeps the QA start screen (logo, " \
  "'Starting QA test') under the results"
qa_second_display_init =
  "firmware: startQA() calls display_init() again before showing the results " \
  "(src/qa.cpp:353, after qa.cpp:301); on FastEPD boards display_init() calls " \
  "bbep.initPanel() (src/display.cpp:295), whose second esp_lcd_new_i80_bus() fails " \
  "(ESP_ERR_NOT_FOUND: the S3's one i80 bus is taken) and ESP_ERROR_CHECK aborts: the device " \
  "reboots into QA again"
qa_panel_rev_breaks_spi =
  "firmware: startQA() calls display_init() again before showing the results " \
  "(src/qa.cpp:353); display_init() reads the panel ID by bit-banging the SPI pins " \
  "(get_panel_rev(), src/display.cpp:197), and with Arduino 3 the peripheral manager then " \
  "detaches the pins from the SPI bus and stops it, while SPIClass::begin() returns early " \
  "(its bus is still set): nothing reaches the panel and the QA results never show"

# the QA result screens
qa_screens = {
  "CrowPanel42" => qa_results_below_panel,
  %w[trmnl_4clr seeed_reTerminal_E1002 trmnl_gen2_4clr TRMNL_7inch5_OG_DIY_Kit_6CLR] => qa_1bpp_buffer_on_color_panel,
  "m5_paper_color" => "#{qa_1bpp_buffer_on_color_panel}; the result lines are wider than the 400-pixel panel, too",
  "trmnl_gen2" => qa_panel_rev_breaks_spi,
  "WAVESHARE_397" => FirmwareBugs::EP397_ROW_SHIFT,
  "TRMNL_X_LILYGO_T5PRO" => qa_second_display_init
}
qa_fail_screen = qa_screens.merge(
  # the verdict line (in qa_fail_details) runs into the start screen's text
  "seeed_reTerminal_E1004" => qa_1bpp_buffer_on_color_panel,
  "TRMNL_X_PAPERS3" => qa_second_display_init
)
unregistered = { "CrowPanel42" => unregistered_below_panel }
# the /api/setup failures whose screen shows the server's logo
setup_logo = { %w[xteink_x3 seeed_reTerminal_E1004] =>
                 "the setup error screen shows the server's logo BMP, and #{FirmwareBugs::BMP_FLIP_OVERFLOW}" }

# WIFI_CONNECTION_RSSI is -100: at or below it the device blames the WiFi signal.
weak = [{ ssid: "TRMNL-Sim", rssi: -100 }]

RSpec.describe "Errors", :parallel, env: :any do
  fixture(:dev) { ProvisionedDevice.new }

  # Wait until the device sleeps or restarts (a crash); returns its status.
  def settle(s, timeout: 120)
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout
    loop do
      st = s.status
      return st if st["boot_count"] > 1 || st["state"] == "halted"
      return s.wait(state: "deep_sleep", timeout: 30, settle_ms: 200)["status"] if st["state"] == "deep_sleep"
      if Process.clock_gettime(Process::CLOCK_MONOTONIC) > deadline
        raise TrmnlSim::TimeoutError, "device neither slept nor restarted: #{st}"
      end

      sleep 0.2
    end
  end

  # Power on (an error is shown right away), wait for the error screen and yield the simulator.
  def boot_with_error(**kw)
    dev.boot(**kw) do |s|
      st = settle(s)
      expect([st["boot_count"], st["state"]]).to eq([1, "deep_sleep"])
      yield s if block_given?
    end
  end

  describe "ErrorScreens" do
    before { dev.reset }

    it "api unreachable", :smoke do
      dev.mock.set_fault("/api/display", close: true)
      boot_with_error { |s| expect(s.status["display_refreshes"]).to be > 1 }
    end

    it "api unreachable on weak wifi" do
      dev.mock.set_fault("/api/display", close: true)
      boot_with_error(networks: weak)
    end

    it "image cut short on weak wifi" do
      dev.mock.set_fault("/images/*", truncate: 1000)
      boot_with_error(networks: weak)
    end

    it "image host unreachable" do
      dev.mock.set_fault("/images/*", close: true)
      boot_with_error
    end

    it "image url that can't be fetched", known_failure: { "CrowPanel42" => message_below_panel,
                                                           "WAVESHARE_397" => FirmwareBugs::EP397_ROW_SHIFT } do
      # HTTPClient refuses the URL, so the request never starts: HTTPS_UNABLE_TO_CONNECT.
      # The screen blames the API although it answered (only HTTP errors from the image
      # host get the "image download failed" screen).
      dev.mock.display = { image_url: "ftp://10.0.2.2/image.bmp", filename: "plugin-bbbbbb-1", refresh_rate: 300 }
      boot_with_error do |s|
        # "WiFi connected, unable connect to API." and how to retry (on the 960x540 parallel
        # panels the logo, centred, runs into the text's fixed rows)
        expect(s).to show_message("api_unable_to_connect.png", [200, 320, 400, 64])
      end
      log = dev.mock.wait_for_request("/api/log", timeout: 10)
      expect(log.body).to include("HTTPS_UNABLE_TO_CONNECT - Unable to create WiFiClient")
    end

    it "image too large" do
      dev.mock.set_file("/huge.bmp", "image/bmp", "\0".b * 100_000)
      dev.mock.display = { image_url: "#{dev.mock.device_url}/huge.bmp", filename: "plugin-aaaaaa-1",
                           refresh_rate: 300 }
      boot_with_error
    end
  end

  describe "Retries" do
    before { dev.reset }

    it "timer wakes retry quietly then show the error",
       known_failure: { "CrowPanel42" => "#{message_below_panel}; the screen before it is the logo too " \
                                         "(#{FirmwareBugs::ONE_BIT_PNG_PANEL_TYPE})" } do
      dev.boot_asleep do |s|
        s.wait_for_deep_sleep
        screen = s.screenshot
        dev.mock.set_fault("/api/display", status: 500)
        quiet = true
        attempt = 0
        5.times do |i|
          attempt = i
          dev.mock.next_request("/api/display", timeout: 90) { s.wake }
          s.wait(state: "deep_sleep", timeout: 180, settle_ms: 200)
          quiet = s.compare_screen(screen, tolerance: 0, max_ratio: 0)["match"]
          break unless quiet
        end
        expect(quiet).to be(false), "the error was never shown"
        expect(attempt).to be >= 2, "retried quietly first"
        dev.mock.clear_faults
        dev.mock.next_request(dev.mock.image_path("default"), timeout: 90) { s.wake }
        s.wait_for_deep_sleep
      end
    end
  end

  # Onboarding against a server whose /api/setup misbehaves.
  describe "SetupErrors" do
    let(:mock) { TrmnlSim::MockTrmnl.new }

    after { mock.close }

    # Onboard a fresh device against `mock` until it asks /api/setup; yields the simulator.
    def onboard(**kw)
      sim(erase: true, extra_args: ["--offline"], **kw) do |s|
        s.wait(portal: true, timeout: 90)
        s.portal_connect("TRMNL-Sim", "password", server: mock.device_url)
        mock.wait_for_request("/api/setup", timeout: 120)
        yield s
      end
    end

    # Onboard, and expect the device to go to sleep on the error screen without restarting.
    def onboard_and_sleep(**kw)
      onboard(**kw) do |s|
        yield s if block_given?
        expect(settle(s)["boot_count"]).to eq(1)
      end
    end

    # The message's lines (on the rows from 340 down, under the logo) are each centred.
    def expect_centred_lines(s, at_least)
      width = device.width
      lines = text_lines(s, 300)
      expect(lines.size).to be >= at_least, "message lines: #{lines}"
      lines.each do |top, bottom, left, right|
        # a glyph's side bearings can differ by a pixel or two
        expect((left - (width - 1 - right)).abs).to be <= 3,
                                                    "rows #{top}-#{bottom}: ink from x=#{left} to #{right} on a " \
                                                    "#{width}-pixel panel"
      end
    end

    it "unregistered mac shows the signup message" do
      mock.setup = nil # 404 "MAC Address not registered"
      onboard do |s|
        st = settle(s)
        expect([st["boot_count"], st["state"]]).to eq([1, "deep_sleep"])
        expect(mock.count("/api/display")).to eq(0)
      end
    end

    it "long unregistered message is wrapped", known_failure: unregistered do
      message = "Your device #{'is not yet registered with any account, ' * 3}" \
                "visit https://usetrmnl.com/signup/with-a-very-long-link-that-cannot-be-wrapped-anywhere-at-all " \
                "and enter Device ID SIMTST"
      mock.setup = nil
      mock.set_fault("/api/setup", body: JSON.generate({ status: 404, api_key: nil, friendly_id: nil, image_url: nil,
                                                         message: }))
      onboard do |s|
        st = settle(s, timeout: 30)
        expect([st["boot_count"], st["state"]]).to eq([1, "deep_sleep"])
        # wrapped at spaces, the link broken inside, every line centred
        expect_centred_lines(s, 4)
      end
    end

    it "unregistered message is centred", known_failure: unregistered do
      # trmnl.app's "MAC ... not registered - send to support@trmnl.com to activate your TRMNL"
      # (issue #650: drawn from the left edge)
      mock.setup = nil
      onboard do |s|
        settle(s)
        expect_centred_lines(s, 2)
      end
    end

    it "setup server error", known_failure: setup_logo do
      mock.set_fault("/api/setup", status: 500)
      onboard_and_sleep
    end

    it "setup server error on weak wifi", known_failure: setup_logo do
      mock.set_fault("/api/setup", status: 500)
      onboard_and_sleep(networks: weak)
    end

    it "setup malformed json", known_failure: setup_logo do
      mock.set_fault("/api/setup", body: '{"status": 200, "api_key": ')
      onboard_and_sleep
    end

    { "missing" => { status: 404 }, "not an image" => { body: "not an image at all" },
      "host unreachable" => { close: true } }.each do |what, fault|
      it "setup logo #{what}" do
        mock.set_fault("/images/*", **fault)
        onboard_and_sleep { mock.wait_for_request("/images/default.bmp", timeout: 120) }
      end
    end
  end

  # A fresh device near a "TRMNL_QA" network runs the factory test: 7 s of CPU and radio
  # load, comparing the chip temperature (and battery voltage) before and after. Every build
  # but the TRMNL X's has it (setup() in src/main.cpp calls startQA() until it passed once;
  # the X has its own factory flow, see trmnl_x_spec).
  describe "FactoryQa", skip_if: :shipment,
                        why: "its factory flow flashes the modem and ships; it runs no QA test (main.cpp)" do
    # Start a fresh device next to the QA network; yields it once the stress test runs.
    def start_qa
      sim(erase: true, networks: [{ ssid: "TRMNL_QA", rssi: -40 }, { ssid: "TRMNL-Sim" }],
          extra_args: ["--offline"]) do |s|
        s.wait(console: /Stress test started/, timeout: 30)
        yield s
      end
    end

    def finish_qa(s)
      s.wait(console: /QA Test Passed/, timeout: 30)
      s.wait(display_idle: true, timeout: 30)
      expect(s.status["boot_count"]).to eq(1), "the device crashed"
    end

    it "qa passes and the button continues to setup", known_failure: qa_screens, needs: :button do
      start_qa do |s|
        finish_qa(s)
        expect(s).to show_message("qa_pass.png", [300, 200, 200, 70])
        # "Initial temperature: 25.0000 C, Final temperature: 25.0000 C  Diff: 0.0000 C".
        # (The voltage line above it isn't compared: measureVoltageAverage() reads GPIO 3 on
        # every board, and on the OG scales the raw ADC value as if 4095 were 3.3 V (5.41 V
        # for a 4.1 V battery; at 11 dB the C3 tops out at 2.5 V).)
        expect(s).to show_message("qa_temperatures.png", [60, 352, 680, 26])
        s.press(200) # "press button to clear screen" (a short press)
        s.wait(console: /painting screen white/, timeout: 30)
        s.wait(portal: true, timeout: 90)
        s.power_cycle # passed: not run again
        s.wait(portal: true, timeout: 90)
        expect(s.console(0).count { _1.include?("Stress test started") }).to eq(1)
      end
    end

    it "qa fails when the chip heats up", known_failure: qa_fail_screen do
      start_qa do |s|
        s.set_faults(chip_temp_c: 30) # 5 °C warmer after the load: 3 °C is the limit
        finish_qa(s)
        expect(s).to show_message("qa_fail.png", [300, 200, 200, 70])
        # "... Final temperature: 30.0000 C  Diff: 5.0000 C" and "QA failed, please use
        # another board and put in failure pile for investigation"
        expect(s).to show_message("qa_fail_details.png", [40, 352, 720, 58])
      end
    end

    it "button stops qa", needs: :button do
      start_qa do |s|
        s.press(200)
        s.wait(console: /QA test stopped by user/, timeout: 30)
        s.wait(portal: true, timeout: 90) # carries on as a normal first boot
      end
    end
  end
end
