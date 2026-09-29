# frozen_string_literal: true

require "fileutils"
require "tmpdir"

# --memcheck: whole onboarding and refresh cycles of the device under test (and the X) under the
# memory checker (heap use-after-free, overflows, bad frees; stack high-water marks).
#
# Firmware bugs it found are in FirmwareBugs::KNOWN_MEMORY_BUGS: the clean-cycle tests tolerate
# them (so everything else is still checked), and each has an example here that fails where the
# bug shows (known_failure:, or pending: for the device-specific groups).

module MemcheckSpec
  # A factory-fresh X (the QA flow and modem flashing run under memcheck too, once per
  # firmware/simulator: it comes from the setup cache and is only cached if it was clean).
  class ShippedMemcheckX
    def initialize
      inputs = { build: SetupCache.build_id(TrmnlX.build), turbo: Builds::TURBO }
      @cache, = SetupCache.entry("x-shipped-memcheck", inputs) do |dir|
        TrmnlX.sim(flash: File.join(dir, "flash.bin"), erase: true, memcheck: "halt", name: "x-memcheck-factory") do |s|
          s.wait_for_console(/Entering shipment mode light sleep loop/, timeout: 180)
        end
        {}
      end
      @dir = Dir.mktmpdir("trmnl-x-memcheck-")
    end

    # Boot a copy of it.
    def boot(**, &)
      flash = File.join(@dir, "flash-#{Dir.children(@dir).size}.bin")
      FileUtils.cp(File.join(@cache, "flash.bin"), flash)
      TrmnlX.sim(flash:, **, &)
    end

    def close = FileUtils.rm_rf(@dir)
  end
end

sntp_bug = %w[_ZN5Clock14setTimeFromNTPEv sntp_request]
body_bug = "_ZNK16HttpRetryRequest12bodyAsStringEv"
qa_bug = "_Z19display_show_msg_qaPhPKfS1_b"

# The known memory bugs to suppress while an example checks for the others.
all_bugs_but = ->(*names) { FirmwareBugs::KNOWN_MEMORY_BUGS - names }

idf_small_stacks =
  "framework: Arduino-ESP32 2.0.17's prebuilt ESP32-S3 sdkconfig gives the IDLE and ipc tasks " \
  "1024-byte stacks (CONFIG_FREERTOS_IDLE_TASK_STACKSIZE, CONFIG_ESP_IPC_TASK_STACK_SIZE), and " \
  "IDLE0 / ipc0 come within 256 bytes of their end"
qa_second_display_init =
  "firmware: the factory QA aborts before it shows its results: its second display_init() " \
  "fails on FastEPD boards (see errors_spec QA_SECOND_DISPLAY_INIT)"
sntp_use_after_free =
  "firmware: Clock::setTimeFromNTP() hands configTime() a String's c_str() (src/misc/clock/clock.cpp:47) " \
  "that is freed on return while SNTP keeps retrying with it"
body_overread =
  "firmware: HttpRetryRequest::bodyAsString() String::concat()s a body that isn't NUL-terminated " \
  "(src/services/http_retry_request.cpp:66): Arduino 2's concat(buf, len) copies len + 1 bytes"
qa_overread =
  "firmware: display_show_msg_qa() copies a panel-sized 1-bit frame into startQA()'s 48000-byte " \
  "buffer (src/qa.cpp:216, 251), past its end on this panel"

every_env = Devices::ALL.map(&:env)
# Arduino 3 builds: its String::concat(buf, len) copies len bytes, not len + 1
# (the envs on pioarduino's platform in platformio.ini)
arduino3 = %w[TRMNL_X TRMNL_X_PAPERS3 TRMNL_X_LILYGO_T5PRO TRMNL_X_SENSORIAC5 trmnl_gen2 trmnl_gen2_4clr seeed_sticky
              m5_paper_color seeed_reTerminal_E1004]
# Arduino 3's SNTP (IDF 5's lwIP) doesn't ask for the freed name again before the device sleeps.
sntp_fixed_on = %w[TRMNL_X TRMNL_X_PAPERS3 TRMNL_X_LILYGO_T5PRO TRMNL_X_SENSORIAC5]
# The Arduino 2 ESP32-S3 builds, whose IDLE and ipc stacks are too small (idf_small_stacks).
idf_small_stack_envs = Devices::ALL.select { |d| d.chip == "esp32s3" && !arduino3.include?(d.env) }.map(&:env)
# The IDF tasks the firmware doesn't size (see idf_small_stacks).
idf_tasks = %w[IDLE ipc]
# startQA()'s buffer is 48000 bytes: display_show_msg_qa() reads past it on panels whose 1-bit
# frame is bigger than 48000 - 62 bytes (bb_epaper boards; FastEPD ones don't copy it).
# (Not seen on the reTerminal E1004, though its 1200x1600 frame is far bigger: open question.)
qa_overreads = Devices::ALL.select do |d|
  d.og_font? && d.width / 8 * d.height > 48_000 - 62 && d.env != "seeed_reTerminal_E1004"
end.map(&:env)

RSpec.describe "Memcheck", :parallel, env: :any do
  fixture(:shipped_x) { MemcheckSpec::ShippedMemcheckX.new }

  def expect_no_low_stacks(stacks, but: [])
    low = stacks.select { |t| t["low"] && !t["task"].start_with?(*but) }.map { |t| [t["task"], t["min_free"]] }
    expect(low).to eq([]), "tasks close to overflowing their stacks: #{low}"
  end

  describe "MemcheckOG" do
    def onboard(s, mock)
      s.wait(portal: true, timeout: 90)
      s.portal_connect("TRMNL-Sim", "password", server: mock.device_url)
      mock.wait_for_request("/api/display", timeout: 120)
      s.wait_for_deep_sleep(timeout: 120)
    end

    # A fresh device under memcheck onboarded against a mock showing "one"; yields both.
    def onboarded(**sim_kw)
      TrmnlSim::MockTrmnl.open do |mock|
        sim(erase: true, extra_args: ["--offline"], **sim_kw) do |s|
          device_image(mock, "one", panel_number("1"))
          mock.display = { image: "one", refresh_rate: 300 }
          onboard(s, mock)
          yield s, mock
        end
      end
    end

    it "sees clean onboarding and refresh cycles",
       known_failure: { "seeed_xiao_esp32c3" => FirmwareBugs::XIAO_C3_BUTTON_WAKE } do
      onboarded(memcheck: "halt") do |s, mock|
        mock.next_request("/api/display", timeout: 90) { s.wake }
        s.wait_for_deep_sleep
        mock.next_request("/api/display", timeout: 90) { device.button? ? s.press(150) : s.wake }
        s.wait_for_deep_sleep

        report = s.memcheck
        expect(report["enabled"]).to be(true)
        expect(report["violations"]).to eq([])
        heap = report["heap"]
        expect(heap["allocs"]).to be > 500
        expect(heap["internal"]["peak_bytes"]).to be > 50_000
        if device.psram_frame_buffers?
          expect(heap["psram"]["peak_bytes"]).to be > 1_000_000
        elsif !device.psram?
          expect(heap["psram"]["peak_bytes"]).to eq(0)
        end
        stacks = report["stacks"].to_h { |t| [t["task"], t] }
        expect(stacks["loopTask"]["size"]).to eq(8192)
        expect(stacks["loopTask"]["max_used"]).to be > 1000
        expect(stacks["loopTask"]["instances"]).to be >= 3 # one per boot
        expect_no_low_stacks(report["stacks"], but: idf_tasks) # (see "leaves the idf task stacks room")
      end
    end

    it "leaves the idf task stacks room", known_failure: { idf_small_stack_envs => idf_small_stacks } do
      # The IDF's own small tasks (IDLE, ipc), sized by the framework's sdkconfig, over an
      # onboarding.
      onboarded(memcheck: "log") do |s|
        report = s.memcheck
        expect(report["stacks"].select { |t| t["task"].start_with?("IDLE") }).not_to be_empty, "no IDLE task seen"
        expect_no_low_stacks(report["stacks"].select { |t| t["task"].start_with?(*idf_tasks) })
      end
    end

    it "does not use the sntp server name after free",
       known_failure: { %w[trmnl_gen2 trmnl_gen2_4clr] => FirmwareBugs::GEN2_NTP_HANG,
                        every_env - sntp_fixed_on => sntp_use_after_free } do
      # Clock::sync hands configTime() the c_str() of a String that setTimeFromNTP frees on
      # return; SNTP keeps the pointer and resolves it again on every retry.
      # Without DNS (the server is reached by its IP) the NTP servers' names don't resolve,
      # so SNTP keeps retrying after setTimeFromNTP returned.
      onboarded(memcheck: "log", memcheck_suppress: all_bugs_but.(*sntp_bug),
                faults: { net: { dns: "servfail" } }) do |s, _mock|
        s.assert_no_memory_errors
      end
    end

    it "does not read the api display body past its end", known_failure: { every_env - arduino3 => body_overread } do
      # bodyAsString() calls String::concat(body, size), which copies size + 1 bytes of a
      # body that isn't NUL-terminated. The built-in server sends a Content-Length, which
      # takes the path with a malloc'd body.
      sim(erase: true, memcheck: "log", memcheck_suppress: all_bugs_but.(body_bug), extra_args: ["--offline"]) do |s|
        url = s.mock.start
        s.mock.display(refresh_rate: 300)
        s.wait(portal: true, timeout: 90)
        s.portal_connect("TRMNL-Sim", "password", server: url)
        s.mock.wait_for_request("/api/display", timeout: 120)
        s.wait_for_deep_sleep(timeout: 120)
        s.assert_no_memory_errors
      end
    end

    it "does not read the qa screen past its buffer",
       known_failure: { %w[TRMNL_X_PAPERS3 TRMNL_X_LILYGO_T5PRO] => qa_second_display_init,
                        qa_overreads => qa_overread },
       skip_if: :shipment, why: "its factory flow runs no QA test (main.cpp)" do
      # A fresh device near a "TRMNL_QA" network runs the factory test (errors_spec FactoryQa)
      # and shows its results over startQA()'s white buffer.
      nets = [{ ssid: "TRMNL_QA", rssi: -40 }, { ssid: "TRMNL-Sim" }]
      sim(erase: true, networks: nets, memcheck: "log", memcheck_suppress: all_bugs_but.(qa_bug),
          extra_args: ["--offline"]) do |s|
        s.wait(console: /QA Test Passed/, timeout: 180)
        s.assert_no_memory_errors
      end
    end
  end

  describe "MemcheckBwry", env: "trmnl_4clr" do
    # display_show_image hands an uncompressed 1-bit BMP to the driver as the frame buffer; on
    # the 4-color panel writePlane reads it as 2 bits per pixel, 48 KB past the end of the 48 KB
    # buffer.
    it "does not send a 1bit bmp as a 2bit plane",
       pending: "display_show_image() sends a 1-bit BMP as the BWRY's 2-bit plane (src/display.cpp:1829)" do
      TrmnlSim::MockTrmnl.open do |mock|
        sim(Builds.for_env("trmnl_4clr"), erase: true, memcheck: "log", memcheck_suppress: sntp_bug,
                                          extra_args: ["--offline"]) do |s|
          mock.display = { image: "default", refresh_rate: 300 } # the OG's BMP
          s.wait(portal: true, timeout: 90)
          s.portal_connect("TRMNL-Sim", "password", server: mock.device_url)
          mock.wait_for_request("/api/display", timeout: 120)
          s.wait_for_deep_sleep(timeout: 180)
          s.assert_no_memory_errors
        end
      end
    end
  end

  # A factory-fresh X (see MemcheckSpec::ShippedMemcheckX), then onboarding on 2.4 GHz: WiFi
  # stop/start around the portal is where Arduino once freed a netif the event task still used.
  describe "MemcheckX", env: "TRMNL_X" do
    it "sees clean onboarding and refresh" do
      TrmnlSim::MockTrmnl.open do |mock|
        shipped_x.boot(memcheck: "halt") do |s|
          mock.set_png("dots", ->(x, y) { ((x / 8) + (y / 8)) & 1 })
          mock.display = { image: "dots", refresh_rate: 300 }
          TrmnlX.onboard(s, mock, TrmnlX::SSID_24)
          mock.next_request("/api/display", timeout: 120) { s.touch("center", ms: 150) }
          s.wait_for_deep_sleep(timeout: 120)

          report = s.memcheck
          expect(report["violations"]).to eq([])
          heap = report["heap"]
          expect(heap["allocs"]).to be > 500
          expect(heap["psram"]["peak_bytes"]).to be > 1_000_000 # frame buffers
          stacks = report["stacks"].to_h { |t| [t["task"], t] }
          expect(stacks.keys).to include("loopTask", "sys_evt", "tiT", "IDLE0", "IDLE1")
          expect_no_low_stacks(report["stacks"])
        end
      end
    end
  end

  describe "MemcheckXBmp", env: "TRMNL_X" do
    # display_show_image flips an uncompressed BMP with the panel's dimensions: an 800x480 BMP
    # (48 KB) is flipped as 1872x1404, far past the end of its buffer.
    it "flips a bmp image within its buffer", pending: FirmwareBugs::BMP_FLIP_OVERFLOW do
      TrmnlSim::MockTrmnl.open do |mock|
        shipped_x.boot(memcheck: "log", memcheck_suppress: sntp_bug) do |s|
          mock.display = { image: "default", refresh_rate: 300 } # the OG's BMP
          TrmnlX.onboard(s, mock, TrmnlX::SSID_24)
          s.assert_no_memory_errors
        end
      end
    end
  end
end
