# frozen_string_literal: true

require "net/http"

# The simulator's built-in mock TRMNL server (`/mock/...` control API) instead of MockTrmnl:
# onboarding against it, converted images on screen, switching images, OTA files.

RSpec.describe "Builtin server", env: :any do
  # Start the built-in server and onboard the fresh device `s` against it.
  def onboard(s, refresh_rate: 300)
    url = s.mock.start
    s.mock.display(refresh_rate:)
    s.wait(portal: true, timeout: 90)
    s.portal_connect("TRMNL-Sim", "password", server: url)
    url
  end

  # An 8-bit gray PNG of the panel's size with `text` in black on white, and the screenshot it
  # should give (black and white are inks on every panel).
  def black_and_white(text)
    px = panel_number(text)
    level = ->(x, y) { px.(x, y) ? 0 : 255 }
    w, h = device.size
    [TrmnlSim::Images.png_image(level, w, h, bits: 8), TrmnlSim::Images.expected_gray(level, w, h, bits: 8)]
  end

  # On the device under test (the group keeps the name it had when it ran on the OG only).
  describe "BuiltinServerOg" do
    it "onboards and shows uploaded images", known_failure: FirmwareBugs::WRONG_IMAGES do
      sim(erase: true, extra_args: ["--offline"]) do |s|
        # A black-and-white 8-bit gray PNG of the panel's size: converted to what the panel
        # takes (the OG's 1-bit BMP, a 1-bit/4-bit gray or palette PNG) without changing a pixel.
        data, reference = black_and_white("7")
        info = s.mock.add_image("seven", data, current: true)
        expect(info["filename"]).to start_with("plugin-"), info.inspect
        url = onboard(s)
        expect(url).to match(%r{\Ahttp://10\.0\.2\.2:\d+\z})

        setup = s.mock.wait_for_request("/api/setup", timeout: 120)
        expect(setup).to have_header("ID", "7C:DF:A1:00:00:01")
        req = s.mock.wait_for_request("/api/display", timeout: 120)
        expect(req).to have_header("Access-Token", "sim-test-api-key")
        s.mock.wait_for_request(info["path"], timeout: 120)
        st = s.wait_for_deep_sleep(display_idle: true, timeout: 120)
        expect(st["wake_at_s"] - st["sim_time_s"]).to be_within(15).of(300)
        expect(s).to show_image(s.mock.expected("seven"), tolerance: 64, max_ratio: 0)
        expect(s).to show_image(reference, tolerance: 64, max_ratio: 0)

        # Switch the image and wake the device: the next request fetches it.
        info = s.mock.add_image("eight", black_and_white("8")[0])
        s.mock.display(image: "eight")
        s.mock.next_request(info["path"], timeout: 120) { s.wake }
        s.wait_for_deep_sleep(display_idle: true, timeout: 120)
        expect(s).to show_image(s.mock.expected("eight"), tolerance: 64, max_ratio: 0)
      end
    end

    it "survives http faults" do
      # scripts/mock_server.py's failures, one wake each: the device gets the fault, sleeps
      # without crashing, and shows the image once the server is healthy again.
      faults = [%w[display 500], %w[display reset], %w[display close], %w[display bad-json], %w[display timeout=2],
                %w[image truncate], %w[image garbage], %w[image empty], %w[image reset], %w[image slow=1024,20]]
      data, reference = black_and_white("7")
      sim(erase: true, extra_args: ["--offline"]) do |s|
        info = s.mock.add_image("seven", data, current: true)
        onboard(s)
        s.mock.wait_for_request(info["path"], timeout: 120)
        s.wait_for_deep_sleep(display_idle: true, timeout: 120)
        aggregate_failures do
          faults.each do |route, spec|
            # a new version of the image, so the device downloads it again
            s.mock.add_image("seven", data, current: true)
            expect(s.mock.faults(route.to_sym => ["#{spec}:1"])[route]).to eq(["#{spec}:1"])
            cursor = s.mock.cursor
            boots = s.status["boot_count"]
            s.wake
            hit = wait_for_fault(s, cursor)
            expect(hit["summary"]).to start_with("fault #{spec}"), hit.inspect
            st = s.wait_for_deep_sleep(display_idle: true, timeout: 120)
            # waking from deep sleep is one boot; a crash would be another
            expect(st["boot_count"]).to eq(boots + 1),
                                        "the device restarted after #{route} #{spec} " \
                                        "(boot #{st['boot_count']}, not #{boots + 1})"
            expect(s.mock.state["faults"][route]).to eq([]), "#{route} #{spec} used up"
          end
        end
        s.mock.faults(display: ["503"])
        s.mock.clear_faults
        expect(s.mock.state["faults"]).to eq("display" => [], "image" => [])
        info = s.mock.add_image("seven", data, current: true)
        s.mock.next_request(info["path"], timeout: 120) { s.wake }
        s.wait_for_deep_sleep(display_idle: true, timeout: 120)
        expect(s).to show_image(reference, tolerance: 64, max_ratio: 0)
      end
    end

    it "refuses bad fault specs" do
      sim(extra_args: ["--offline"]) do |s|
        aggregate_failures do
          [{ display: ["truncate"] }, { image: ["600"] }, { display: ["500:0"] }, { nope: ["500"] },
           { display: %w[500 wat] }].each do |body|
            expect { s.post("/mock/faults", body) }.to raise_error(TrmnlSim::Error), body.inspect
          end
        end
        expect(s.mock.state["faults"]).to eq({ "display" => [], "image" => [] }), "nothing added"
      end
    end

    it "serves this build's firmware for OTA" do
      sim(extra_args: ["--offline"]) do |s|
        port = URI(s.mock.start).port
        firmware = Net::HTTP.start("127.0.0.1", port, open_timeout: 30, read_timeout: 30) do |http|
          http.get("/firmware.bin").body.b
        end
        expect(firmware == File.binread(File.join(build, "firmware.bin"))).to be(true), "/firmware.bin differs"
        url = s.mock.set_file("/blob.bin", "x" * 100_000)
        expect(url).to eq("http://10.0.2.2:#{port}/blob.bin")
      end
    end

    # The first request from index `after` on that a fault answered.
    def wait_for_fault(s, after, timeout: 120)
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout
      while Process.clock_gettime(Process::CLOCK_MONOTONIC) < deadline
        hit = s.mock.requests(after).find { |r| r["summary"].start_with?("fault ") }
        return hit if hit

        sleep 0.2
      end
      seen = s.mock.requests(after).map { [_1.path, _1["summary"]] }
      RSpec::Expectations.fail_with("no request met the fault: #{seen}")
    end
  end

  describe "BuiltinServerBwry", env: "trmnl_4clr" do
    it "reduces a color image to the four inks" do
      sim(Builds.for_env("trmnl_4clr"), erase: true, extra_args: ["--offline"]) do |s|
        # Without dithering, colors are classified like the firmware does.
        s.mock.add_image("bars", TrmnlSim::Images.png_rgb(TrmnlSim::Images.color_bars), current: true, dither: false)
        onboard(s)
        req = s.mock.wait_for_request("/api/display", timeout: 120)
        expect(req).to have_header("Model", "og_4clr")
        s.mock.wait_for_request("/images/bars.png", timeout: 120)
        s.wait_for_deep_sleep(timeout: 120)
        expect(s).to show_image(s.mock.expected("bars"), tolerance: 16, max_ratio: 0)
        expected = TrmnlSim::Images.expected_bwry(TrmnlSim::Images.color_bars)
        expect(s).to show_image(expected, tolerance: 16, max_ratio: 0)
      end
    end
  end
end
