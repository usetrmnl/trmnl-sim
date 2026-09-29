# frozen_string_literal: true

# Special functions (on the device under test): the server assigns one in an /api/display answer,
# and a double click (or a 1-5 s press) of the button runs it on the next wake. Also the
# /api/display status codes and actions around it.

# Firmware bug: classify_button_presses() (button.cpp:62-92) times a press from when it starts
# reading the button, not from the wake. If the first click is still held then (the firmware took
# longer to boot than usual) but released within 50 ms, it is NoAction and the second click is
# never waited for.
slow_boot_click = "button.cpp:84-92: a first click still held when classify_button_presses() starts reading (here " \
                  "~50 ms after the wake) but released within 50 ms counts as NoAction; the double click is lost"

# The sleep special function keeps the screen only where it isn't a BMP (see "sleep keeps the
# screen"): BMPs aren't cached under their filename.
bmp_not_cached = "sleep: status=false/HTTPS_SUCCESS takes the cached-image path (bl.cpp:1478), but BMPs are only " \
                 "saved as /current.bmp: \"Cached image is empty or unreadable\", an error log and message"

# The XIAO C3's button never wakes it: every group that presses it fails there.
xiao_c3 = { "seeed_xiao_esp32c3" => FirmwareBugs::XIAO_C3_BUTTON }
wiper_images = { "CrowPanel42" => FirmwareBugs::ONE_BIT_PNG_PANEL_TYPE, "WAVESHARE_397" => FirmwareBugs::EP397_ROW_SHIFT,
                 "trmnl_gen2_4clr" => FirmwareBugs::GEN2_4CLR_IMAGE }
lost_double_click = {
  %w[CrowPanel42 seeed_reTerminal_E1002 seeed_xiao_esp32s3 TRMNL_7inch5_OG_DIY_Kit TRMNL_7inch5_OG_DIY_Kit_3CLR
     TRMNL_7inch5_OG_DIY_Kit_6CLR TRMNL_4inch26_DIY_Kit seeed_reTerminal_E1001 seeed_sticky WAVESHARE_397
     m5_paper_mono seeed_reTerminal_E1004] => slow_boot_click
}
# Every device whose server default image is a BMP.
bmp_screens = { Devices::ALL.select(&:default_bmp?).map(&:env) => bmp_not_cached }

RSpec.describe "Special functions", :parallel, env: :any do
  fixture(:dev) { ProvisionedDevice.new }

  before { dev.reset }

  # Boot answers assign `function`; returns once the device sleeps with it saved.
  def assign(s, function, image: "default")
    dev.mock.display = { image:, refresh_rate: 300, special_function: function }
    dev.mock.wait_for_request("/api/display", timeout: 90)
    s.wait_for_deep_sleep(timeout: 120, display_idle: true)
  end

  # Press the button (a medium press counts as a double click) and answer the special-function
  # request with `answer`; returns that request.
  def run_function(s, answer, press_ms: 1500)
    dev.mock.display = answer
    req = dev.mock.next_request("/api/display", timeout: 120) { s.press(press_ms) }
    expect(req).to have_header("special_function", "true")
    req
  end

  # Assign `function`, run it with `answer` and wait for the device to show the image `name`
  # (the number `number`) it answers with.
  def run_to_image(function, answer, name, number)
    path, expected = device_image(dev.mock, name, device_number(number))
    dev.boot do |s|
      assign(s, function)
      run_function(s, answer.merge(image: name))
      dev.mock.wait_for_request(path, timeout: 90)
      st = s.wait_for_deep_sleep(timeout: 90, display_idle: true)
      expect(s).to show_image(expected, tolerance: 64, max_ratio: 0)
      st
    end
  end

  # Assign `function`, run it with `answer`; returns the status once the device sleeps again.
  def run_to_sleep(function, answer)
    dev.boot do |s|
      assign(s, function)
      run_function(s, answer)
      s.wait_for_deep_sleep(timeout: 90)
    end
  end

  describe "SpecialFunctions", known_failure: xiao_c3, needs: %i[double_click button] do # runs the special function
    it "identify shows the identify image", :smoke, known_failure: xiao_c3.merge(FirmwareBugs::WRONG_IMAGES) do
      run_to_image("identify", { action: "identify", refresh_rate: 300 }, "seven", "7")
    end

    it "sleep uses the answered refresh rate" do
      st = run_to_sleep("sleep", { image: "default", action: "sleep", refresh_rate: 1800 })
      expect(st["wake_at_s"] - st["sim_time_s"]).to be_within(30).of(1800)
    end

    it "sleep keeps the screen", known_failure: xiao_c3.merge(bmp_screens) do
      # Where the server's default image is a BMP (see bmp_screens), like send_to_me (see there):
      # status=false/HTTPS_SUCCESS makes the OG look for the answer's image in a cache it doesn't
      # have, then report and show an error.
      dev.boot do |s|
        assign(s, "sleep")
        screen = s.screenshot
        run_function(s, { image: "default", action: "sleep", refresh_rate: 1800 })
        s.wait_for_deep_sleep(timeout: 90, display_idle: true)
        expect(dev.mock.count("/api/log")).to eq(0)
        # the same picture: PNG devices redraw the cached image (with a fast refresh, whose
        # simulated grays differ slightly from a full one's), the OG leaves it alone
        expect(s).to show_image(screen, tolerance: 64, max_ratio: 0)
      end
    end

    it "sleep without the sleep action is ignored" do
      st = run_to_sleep("sleep", { image: "default", action: "nothing", refresh_rate: 1800 })
      expect(st["wake_at_s"] - st["sim_time_s"]).to be_within(30).of(300)
    end

    it "add wifi opens the portal first" do
      dev.boot do |s|
        assign(s, "add_wifi")
        dev.mock.display = { image: "default", action: "add_wifi", refresh_rate: 300 }
        req = dev.mock.next_request("/api/display", timeout: 120) do
          s.press(1500)
          s.wait(portal: true, timeout: 120)
          s.portal_connect("TRMNL-Sim", "password", server: dev.mock.device_url)
        end
        expect(req).to have_header("special_function", "true")
        s.wait_for_deep_sleep(timeout: 120)
      end
    end

    it "restart playlist shows the first item", known_failure: xiao_c3.merge(FirmwareBugs::WRONG_IMAGES) do
      run_to_image("restart_playlist", { action: "restart_playlist", refresh_rate: 300 }, "one", "1")
    end

    # After showing /current.bmp, send_to_me leaves status=false/HTTPS_SUCCESS, so
    # downloadAndShow() takes the "image already cached" path with the answer's filename. The OG
    # doesn't cache BMP images under their filename (only PNG and JPEG), so with a BMP it is
    # always "empty or unreadable": the device submits an error log and draws an error message
    # over the image it just showed. Where the image is a PNG, send_to_me doesn't get that far:
    # it looks for /current.bmp or /current.png (bl.cpp:2073), but PNGs are only saved under
    # their filename (bl.cpp:1616-1618 writes nothing but /current.bmp), so it finds "No current
    # image!" and shows an error instead.
    it "send to me keeps the current image",
       pending: "send_to_me finds no cached image (BMPs aren't cached by name, PNGs not as /current.*, " \
                "bl.cpp:2073): an error log and message" do
      _, two = device_image(dev.mock, "two", device_number("2"))
      dev.boot do |s|
        assign(s, "send_to_me", image: "two")
        run_function(s, { image: "two", action: "send_to_me", refresh_rate: 300 })
        s.wait_for_deep_sleep(timeout: 90, display_idle: true)
        expect(dev.mock.count("/api/log")).to eq(0)
        expect(s).to show_image(two, tolerance: 64, max_ratio: 0)
      end
    end

    it "guest mode shows the guest image for its refresh rate",
       known_failure: xiao_c3.merge(FirmwareBugs::WRONG_IMAGES) do
      st = run_to_image("guest_mode", { action: "guest_mode", refresh_rate: 1200 }, "three", "3")
      expect(st["wake_at_s"] - st["sim_time_s"]).to be_within(30).of(1200)
    end

    # Rewind shows /last.bmp or /last.png, which no firmware code writes: it reads the missing
    # /last.png (display_read_file returns NULL), pretends it decoded
    # ("image_proccess_response = PNG_NO_ERR; // DEBUG") and passes NULL to display_show_image.
    it "rewind without a previous image does not crash",
       pending: "rewind passes the NULL of a missing /last.png to display_show_image (PNG_NO_ERR // DEBUG)" do
      dev.boot do |s|
        assign(s, "rewind")
        run_function(s, { image: "default", action: "rewind", refresh_rate: 300 })
        st = s.wait_for_deep_sleep(timeout: 120)
        expect(st["state"]).not_to eq("halted")
        expect(st["boot_count"]).to eq(1), "the device restarted (crashed)"
      end
    end
  end

  describe "Identify", known_failure: xiao_c3, needs: %i[double_click button] do # runs the special function
    %w[identify restart_playlist guest_mode].each do |function|
      it "#{function.tr('_', ' ')} with the empty state image" do
        run_to_sleep(function, { image: "default", filename: "empty_state", action: function, refresh_rate: 300 })
      end
    end

    it "unregistered status during a special function" do
      run_to_sleep("identify", { image: "default", status: 202, refresh_rate: 300 })
    end

    it "reset status during a special function" do
      # "status": 500 in an /api/display answer means "reset": forget WiFi and the API key.
      dev.boot do |s|
        assign(s, "identify")
        run_function(s, { image: "default", status: 500, refresh_rate: 300 })
        s.wait(portal: true, min_boots: 3, timeout: 120)
      end
    end

    it "other actions are rejected" do
      dev.boot do |s|
        aggregate_failures do
          %w[identify add_wifi restart_playlist rewind send_to_me guest_mode].each do |function|
            dev.mock.display = { image: "default", refresh_rate: 300, special_function: function }
            n = dev.mock.cursor
            s.wake
            dev.mock.wait_for_request("/api/display", after: n, timeout: 90)
            s.wait_for_deep_sleep(timeout: 90, settle_ms: 200)
            if function == "add_wifi"
              # Opens the portal first; joining again gets to the request.
              dev.mock.display = { image: "default", action: "wrong", refresh_rate: 300 }
              s.press(1500)
              s.wait(portal: true, timeout: 120)
              s.portal_connect("TRMNL-Sim", "password", server: dev.mock.device_url)
              dev.mock.wait_for_request("/api/display", after: n + 1, timeout: 120)
            else
              run_function(s, { image: "default", action: "wrong", refresh_rate: 300 })
            end
            st = s.wait_for_deep_sleep(timeout: 120, settle_ms: 200)
            expect(st["state"]).not_to eq("halted"), function
          end
        end
      end
    end
  end

  describe "ApiStatus" do
    it "unregistered status polls again soon" do
      dev.mock.display = { image: "default", status: 202, refresh_rate: 300 }
      dev.boot do |s|
        st = s.wait_for_deep_sleep
        expect(st["wake_at_s"] - st["sim_time_s"]).to be < 300
      end
    end

    it "reset status forgets the device" do
      dev.mock.display = { image: "default", status: 500, refresh_rate: 300 }
      dev.boot { |s| s.wait(portal: true, min_boots: 2, timeout: 120) }
    end

    it "empty state image means no plugin yet" do
      dev.mock.display = { image: "default", filename: "empty_state", refresh_rate: 300 }
      dev.boot do |s|
        dev.mock.wait_for_request("/api/display", timeout: 90)
        s.wait_for_deep_sleep
        dev.mock.next_request("/api/display", timeout: 90) { s.wake }
        s.wait_for_deep_sleep(settle_ms: 200)
      end
    end

    it "reset firmware forgets the device" do
      dev.mock.display = { image: "default", reset_firmware: true, refresh_rate: 300 }
      dev.boot do |s|
        dev.mock.wait_for_request("/api/display", timeout: 90)
        s.wait(portal: true, min_boots: 2, timeout: 120)
      end
    end

    wiper = "the wiper runs 100 full refreshes; about 3.5 minutes on the TRMNL X"

    it "screen wiper clears then shows the next item", known_failure: wiper_images, slow: wiper do
      path, four = device_image(dev.mock, "four", device_number("4"))
      dev.mock.display_queue << { image: "default", filename: "screen_wiper.png", refresh_rate: 300 }
      dev.mock.display = { image: "four", refresh_rate: 300 }
      dev.boot do |s|
        dev.mock.wait_for_request(path, timeout: 600)
        s.wait_for_deep_sleep(timeout: 600, display_idle: true)
        expect(s).to show_image(four, tolerance: 64, max_ratio: 0)
      end
    end

    it "screen wiper is only run once per wake", slow: wiper do
      dev.mock.display = { image: "default", filename: "screen_wiper.png", refresh_rate: 300 }
      dev.boot do |s|
        s.wait_for_deep_sleep(timeout: 600)
        expect(dev.mock.count("/api/display")).to eq(2)
      end
    end
  end

  describe "Buttons", known_failure: xiao_c3, needs: :button do
    it "double click runs the special function", known_failure: xiao_c3.merge(lost_double_click),
                                                 needs: :double_click do
      dev.boot do |s|
        assign(s, "sleep")
        dev.mock.display = { image: "default", action: "sleep", refresh_rate: 1800 }
        req = dev.mock.next_request("/api/display", timeout: 120) { s.double_click(ms: 80, gap_ms: 150) }
        expect(req).to have_header("special_function", "true")
        st = s.wait_for_deep_sleep
        expect(st["wake_at_s"] - st["sim_time_s"]).to be_within(30).of(1800)
      end
    end

    it "short tap is a plain button wake" do
      dev.boot_asleep do |s|
        s.wait_for_deep_sleep
        req = dev.mock.next_request("/api/display", timeout: 90) { s.press(20) }
        expect(req).not_to have_header("special_function")
        s.wait_for_deep_sleep
      end
    end

    # the OG reads a press in the double-click window after the waking tap
    it "tap then long press resets wifi", needs: :double_click do
      dev.boot_asleep do |s|
        s.wait_for_deep_sleep
        s.press(20)
        s.press(6000)
        s.wait(portal: true, timeout: 120)
      end
    end

    it "very long press is a soft reset", needs: :soft_reset_press do
      dev.boot_asleep do |s|
        s.wait_for_deep_sleep
        s.press(16_000)
        s.wait(portal: true, timeout: 180)
      end
    end
  end
end
