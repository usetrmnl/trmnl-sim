# frozen_string_literal: true

require "json"
require "uri"

module TrmnlSim
  # Drives the simulator's built-in mock TRMNL server over the control API (/mock/...). Images
  # added are converted for the simulated panel; `expected(name)` is the PNG a screenshot should
  # then match.
  #
  #     url = sim.mock.start
  #     sim.mock.add_image("hello", png_bytes, current: true)
  #     sim.mock.display(refresh_rate: 600)
  #     sim.portal_connect("TRMNL-Sim", "pw", server: url)
  #     req = sim.mock.wait_for_request("/api/display")
  #     sim.compare_screen(sim.mock.expected("hello"))["match"]
  class BuiltinServer
    # A request the built-in server recorded (i, method, path, headers, body, status, summary,
    # sim_time_s); reads like a MockTrmnl RecordedRequest too.
    class Request
      attr_reader :raw, :headers

      def initialize(raw)
        @raw = raw
        @headers = Headers.new(raw["headers"])
      end

      def [](key) = key.to_s == "headers" ? headers : raw[key.to_s]
      def path = raw["path"]
      def method = raw["method"]
      def body = raw["body"]
      def status = raw["status"]
      def index = raw["i"]
      def inspect = "#<BuiltinServer::Request #{raw.except('headers').inspect}>"
    end

    attr_reader :sim

    def initialize(sim)
      @sim = sim
    end

    def state = sim.get_json("/mock")

    # Start listening (0 = any free port); returns the device URL (http://10.0.2.2:PORT).
    def start(port: 0) = sim.post("/mock/start", { port: })["device_url"]

    def stop = sim.post("/mock/stop")
    def device_url = state["device_url"]

    # The number of requests so far, a cursor for `wait_for_request(after:)`.
    def cursor = state["total_requests"]

    # Add (or replace) an image from PNG/JPEG/BMP/GIF bytes, converted for the panel (raw: true
    # serves a PNG/BMP unchanged). Names: letters, digits, - and _.
    def add_image(name, data, current: false, dither: true, fit: "contain", raw: false)
      query = URI.encode_www_form(name:, current: flag(current), dither: flag(dither), fit:, raw: flag(raw))
      code, _, body = sim.request(:post, "/mock/images?#{query}", raw: data.b)
      out = JSON.parse(body)
      raise Error, "add_image(#{name}) -> #{code}: #{out.fetch('error', out)}" unless code == 200

      out
    end

    # The PNG the screen should show for image `name`.
    def expected(name)
      code, _, data = sim.request(:get, "/mock/images/#{name}/expected")
      raise Error, "expected(#{name}) -> #{code}: #{data[0, 200].inspect}" unless code == 200

      data
    end

    def remove_image(name) = sim.request(:delete, "/mock/images/#{name}")

    # Change the /api/display answer: image, refresh_rate, special_function, playlist,
    # auto_advance, registered, friendly_id, api_key, extra (raw fields; nil removes).
    def display(**fields) = sim.post("/mock/display", fields)

    # Raw /api/display fields for the next answer only, e.g. update_firmware: true,
    # firmware_url: ..., or reset_firmware: true (plus image: NAME).
    def queue(**fields) = sim.post("/mock/queue", fields)

    # Append HTTP and connection failures to each route's queue, in the firmware's
    # scripts/mock_server.py syntax KIND[=ARG][:COUNT] (e.g. "503:2", "timeout=20", "reset:1",
    # image "truncate:1"); returns both queues.
    def faults(display: [], image: []) = sim.post("/mock/faults", { display:, image: })

    # Empty the fault queue of `route` ("display" or "image"), or both.
    def clear_faults(route = nil) = sim.delete("/mock/faults#{"?route=#{route}" if route}")

    # Serve bytes at `path` (e.g. a firmware.bin for OTA); returns the device URL.
    def set_file(path, data)
      code, _, body = sim.request(:post, "/mock/files?#{URI.encode_www_form(path:)}", raw: data.b)
      raise Error, "set_file -> #{code}: #{body[0, 200].inspect}" unless code == 200

      JSON.parse(body)["url"]
    end

    # Recorded requests from index `since` on.
    def requests(since = 0) = sim.get_json("/mock/requests?since=#{since}")["requests"].map { Request.new(_1) }

    def count(path) = requests.count { |r| r.path == path }

    # Wait for a request to `path` with index >= `after` (see `cursor`).
    def wait_for_request(path, after: 0, timeout: 60)
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout
      loop do
        found = requests(after).find { |r| r.path == path }
        return found if found
        if Process.clock_gettime(Process::CLOCK_MONOTONIC) > deadline
          raise TimeoutError, "no request to #{path} within #{timeout}s (seen: #{requests.map(&:path)})"
        end

        sleep 0.1
      end
    end

    # The first request to `path` made after the block started.
    def next_request(path, timeout: 60)
      after = cursor
      yield if block_given?
      wait_for_request(path, after:, timeout:)
    end

    private

    def flag(value) = value ? 1 : 0
  end
end
