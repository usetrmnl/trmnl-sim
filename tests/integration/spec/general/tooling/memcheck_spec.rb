# frozen_string_literal: true

require "fileutils"
require "tmpdir"

# --memcheck: whole onboarding and refresh cycles of the device under test under the
# memory checker (heap use-after-free, overflows, bad frees; stack high-water marks).
#
# Firmware bugs it found are in FirmwareBugs::KNOWN_MEMORY_BUGS: the clean-cycle tests tolerate
# them (so everything else is still checked), and each has an example here that fails where the
# bug shows (known_failure:). The TRMNL X's and BWRY's own memcheck specs are with their devices'
# (core/trmnl_x/memcheck_spec.rb, core/trmnl_bwry_spec.rb).

sntp_bug = FirmwareBugs::SNTP_USE_AFTER_FREE
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
  "fails on FastEPD boards (see general/errors_spec QA_SECOND_DISPLAY_INIT)"
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

General.describe "Memcheck" do
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
      # body that isn't NUL-terminated. MockTrmnl sends a Content-Length, which takes the path
      # with a malloc'd body.
      onboarded(memcheck: "log", memcheck_suppress: all_bugs_but.(body_bug)) do |s, _mock|
        s.assert_no_memory_errors
      end
    end

    it "does not read the qa screen past its buffer",
       known_failure: { %w[TRMNL_X_PAPERS3 TRMNL_X_LILYGO_T5PRO] => qa_second_display_init,
                        qa_overreads => qa_overread },
       skip_if: :shipment, why: "its factory flow runs no QA test (main.cpp)" do
      # A fresh device near a "TRMNL_QA" network runs the factory test (general/errors_spec FactoryQa)
      # and shows its results over startQA()'s white buffer.
      nets = [{ ssid: "TRMNL_QA", rssi: -40 }, { ssid: "TRMNL-Sim" }]
      sim(erase: true, networks: nets, memcheck: "log", memcheck_suppress: all_bugs_but.(qa_bug),
          extra_args: ["--offline"]) do |s|
        s.wait(console: /QA Test Passed/, timeout: 180)
        s.assert_no_memory_errors
      end
    end
  end
end
