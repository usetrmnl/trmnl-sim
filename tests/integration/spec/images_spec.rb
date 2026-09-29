# frozen_string_literal: true

# Image formats and bad images on the device under test (see devices.rb; the TRMNL X has its own,
# images_x_spec): PNG in every depth and color type, JPEG, BMP variants, and images the device must
# refuse. Images are the panel's size; what should be on screen is the same picture, reduced to what
# the panel shows.

# Firmware bugs, by the device they show on.
reduce_bpp_4bit =
  "ReduceBpp (display.cpp:945) builds a 4-bit gray PNG's odd pixels as (s[0] & 0xf) | (s[0] << 4) " \
  "without masking to 8 bits: white is 0xfff, and g >> 7 ORs 5 bits into the output byte, so most " \
  "of the image comes out black (png_draw's path for black-and-white SPI panels)"
crowpanel_1bit_png =
  "png_to_epd (display.cpp:1764) calls bbep.setPanelType(dpList[...].OneBit) for 1-bit PNGs, but the " \
  "CrowPanel's dpList row holds bb_epaper product numbers for bbep.begin() (its device_list[] row has " \
  "no pins): EPD_CROWPANEL42 = 8 is taken as panel type EP295_128x296_4GRAY, and the picture never " \
  "shows (see byod_ssd_spec CrowPanel42)"
bwry_truecolor =
  "trmnl_4clr doesn't set PNG_MAX_BUFFERED_PIXELS (platformio.ini [env:trmnl_4clr]; trmnl does), so " \
  "PNGdec's default row buffer is too small for 800 px truecolor rows: the decode corrupts the heap " \
  "and the device crashes and restarts (see BwryPng \"truecolor png\")"
bwry_wide_png =
  "png_draw_4clr (display.cpp:1325) writes every decoded row whole, (iWidth + 3) / 4 bytes, without " \
  "cropping it to the panel like png_draw does: a PNG wider than 800 px wraps into the next rows"
color_jpeg =
  "jpeg_to_epd decodes to 1 bit and jpeg_draw (display.cpp:1608) sends the rows with " \
  "startWrite(PLANE_0) as a 1-bpp plane, which on the color panels' UC81xx controllers is command " \
  "0x13; they take their pixels through 0x10 (2 bpp on BWRY, bbepWriteImage2bpp; 4 bpp on " \
  "Spectra 6, bbepWriteImage4bpp), so the JPEG never shows"
fastepd_wide_png =
  "FastEPD's png_draw (display.cpp:1470) takes a 1-bit PNG wider than the panel for a portrait one " \
  "and draws it rotated: for x up to the image's width it steps a row up from the bottom (d -= " \
  "iPitch), running off the top of the framebuffer (StoreProhibited in png_draw, display.cpp:1485), " \
  "and the device crashes and restarts, instead of cropping it as png_to_epd announces"
bwr_4gray =
  "png_to_epd sends PNGs of more than two colors or 2 bits (and 4/8-bit gray, truecolor) down the " \
  "4-gray path (display.cpp:1789), whose two gray bit planes land in this panel's black/white (DTM1) " \
  "and red (DTM2) planes: white comes out black and black red (see byod_uc81xx_spec DiyKitBwr)"
bwr_2bit_red =
  "png_draw's PNG_2_BIT_INVERTED case (display.cpp:1378) writes the inverted picture into the " \
  "second plane for 2-bit two-color PNGs, which on this 3-color panel is the red plane (the 1-bit " \
  "case clears it for BBEP_3COLOR, display.cpp:1366): the black comes out red"
sticky_4gray =
  "bb_epaper (the Sticky's pinned 0395f30) starts EP397_800x480_4GRAY refreshes with 0x22 0xD7, " \
  "whose load-LUT bit replaces the custom 4-gray LUT its init sequence wrote with the built-in " \
  "one, which shows the two gray planes as black and white: PNGs on the 4-gray path come out " \
  "inverted (see byod_ssd_spec SeeedSticky \"shows a 4 gray image\")"
ssd1677_window =
  "jpeg_draw (display.cpp:1607) sets an address window for every 8-row block, and this env's pinned " \
  "bb_epaper programs it ascending (bbepSetAddrWindow: X start < end, counter at the start; bytes, " \
  "not pixels, for EP397), while SET_ORIENTATION in the panel's init sequence (bbepSetFlip180) " \
  "puts the SSD1677 in data entry mode 0x02, X counting down from 799: the blocks run off the left " \
  "edge (M5Paper) or scatter into thin lines (Sticky). (bb_epaper 2.1.9 skips the window for " \
  "EP426/EP397, so the X4 and 4.26\" kit are fine)"
m5_1bit_png =
  "png_to_epd (display.cpp:1764) passes the M5Paper Mono's dpList product number (EPD_M5_PAPER_MONO " \
  "= 30) to bbep.setPanelType for 1-bit PNGs: panel type EP266YR_184x360, a UC81xx 4-color panel, so " \
  "the image goes out with UC81xx commands the SSD1677 doesn't understand and never shows (see " \
  "byod_m5_spec M5PaperMono)"
e1004_png_buffer =
  "PNG_MAX_BUFFERED_PIXELS=6432 (platformio.ini [env:seeed_reTerminal_E1004]) is sized for 800 px " \
  "rows, and PNGdec keeps two rows in that buffer while refusing only a row that alone doesn't fit " \
  "(png.inl:643): a 1200 px truecolor row (3601 bytes, 7202 for two) runs past it and the picture " \
  "comes out wrong (bands of garbage)"

envs_with_inks = ->(*inks) { Devices::ALL.select { inks.include?(_1.inks) }.map(&:env) }
bwr_kit = "TRMNL_7inch5_OG_DIY_Kit_3CLR"
# Tests whose picture comes out a row too high on the Waveshare 3.97" and half drawn on the gen-2
# BWRY...
shifted = { "WAVESHARE_397" => FirmwareBugs::EP397_ROW_SHIFT, "trmnl_gen2_4clr" => FirmwareBugs::GEN2_4CLR_IMAGE }
# ...and the 1-bit PNGs among them, which never show on the CrowPanel and the M5Paper Mono.
one_bit = shifted.merge("CrowPanel42" => crowpanel_1bit_png, "m5_paper_mono" => m5_1bit_png)

RSpec.describe "Images", :parallel, env: :any do
  fixture(:dev) { ProvisionedDevice.new }

  before { dev.reset }

  # The panel of the device under test: the general groups' images are its size.
  let(:w) { device.width }
  let(:h) { device.height }
  let(:seven) { panel_number("7") }

  def digit(level_black, level_white, text = "7")
    px = panel_number(text)
    ->(x, y) { px.(x, y) ? level_black : level_white }
  end

  # The screenshot a `bits`-deep gray PNG of `level` should give on the device under test's panel.
  # The color panels' PNG decoders (png_draw_4clr / png_draw_6clr in display.cpp) take a gray
  # sample's raw bits as its RGB value (2-bit 0x00/0x40/0x80/0xC0, 1-bit 0x00/0x80, 4-bit doubled)
  # and pick the nearest ink; the others show the grays.
  def expected(level, bits)
    if %w[bwry spectra6].include?(device.inks)
      widen = { 1 => ->(v) { v << 7 }, 2 => ->(v) { v << 6 }, 4 => ->(v) { v * 17 }, 8 => ->(v) { v } }.fetch(bits)
      color = ->(x, y) { [widen.(level.(x, y))] * 3 }
      quantized = device.inks == "bwry" ? :expected_bwry : :expected_spectra6
      return TrmnlSim::Images.public_send(quantized, color, w, h)
    end
    TrmnlSim::Images.expected_gray(level, w, h, bits:)
  end

  def png(level, bits = 1, width: w, height: h) = TrmnlSim::Images.png_image(level, width, height, bits:)

  # A 1-bit bottom-up BMP, blank (index 0) unless `black.(x, y)` says which pixels are index 0 (the
  # rest are index 1).
  def bmp(width = w, height = h, palette: [0, 0, 0, 0, 255, 255, 255, 0].pack("C*"), offset: nil, magic: "BM".b,
          black: nil)
    row = ((width + 31) / 32) * 4
    data = Array.new(row * height, 0)
    black && height.times do |y|
      base = (height - 1 - y) * row
      width.times { |x| data[base + (x / 8)] |= 0x80 >> (x % 8) unless black.(x, y) }
    end
    data = data.pack("C*")
    off = offset || (14 + 40 + palette.bytesize)
    header = magic + [14 + 40 + palette.bytesize + data.bytesize, 0, 0, off].pack("VvvV")
    info = [40, width, height, 1, 1, 0, data.bytesize, 2835, 2835, 2, 2].pack("Vl<l<vvVVl<l<VV")
    header + info + palette + data
  end

  def zeros(n) = "\0".b * n
  def data_file(name) = File.binread(File.join(Builds::HERE, "data", name))

  # Wait until the device sleeps or restarts (a crash); returns its status.
  def settle(s, timeout: 120)
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout
    loop do
      st = s.status
      return st if st["boot_count"] > 1 || st["state"] == "halted"
      if st["state"] == "deep_sleep"
        return s.wait(state: "deep_sleep", display_idle: true, timeout: 30, settle_ms: 200)["status"]
      end
      if Process.clock_gettime(Process::CLOCK_MONOTONIC) > deadline
        raise TrmnlSim::TimeoutError, "device neither slept nor restarted: #{st}"
      end

      sleep 0.2
    end
  end

  # Make the next /api/display answers point at `data`; returns its path.
  def serve(name, data, content_type, **fields)
    path = "/img/#{name}"
    url = dev.mock.set_file(path, content_type, data)
    uid = Digest::SHA1.hexdigest(name)[0, 6]
    dev.mock.display = { image_url: url, filename: "plugin-#{uid}-#{Time.now.to_i}", refresh_rate: 300, **fields }
    path
  end

  # Boot, wait until the device has downloaded `path` and gone to sleep without crashing, and yield
  # the simulator (asleep) and its status.
  def boot_and_fetch(path)
    dev.boot do |s|
      dev.mock.wait_for_request(path, timeout: 90)
      st = settle(s)
      expect([st["boot_count"], st["state"]]).to eq([1, "deep_sleep"]), "the device crashed"
      yield s, st if block_given?
    end
  end

  # Boot, get `data` shown, and yield the simulator (asleep afterwards).
  def show(name, data, content_type, **fields, &) = boot_and_fetch(serve(name, data, content_type, **fields), &)

  # Serve an image the device must refuse (or survive): it sleeps and doesn't halt.
  def survives(name, data, content_type)
    show(name, data, content_type) { |s| expect(s.status["state"]).not_to eq("halted") }
  end

  # Serve the next image (with the refresh's `fields`) and wake the sleeping device to show it.
  def show_next(s, name, data, **fields)
    path = serve(name, data, "image/png", **fields)
    dev.mock.next_request(path, timeout: 90) { s.wake }
    s.wait(state: "deep_sleep", display_idle: true, timeout: 90)
  end

  describe "Png" do
    it "1bit png", known_failure: one_bit do
      level = digit(0, 1)
      show("one.png", png(level), "image/png") do |s|
        expect(s).to show_image(expected(level, 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "2bit png with two colors is drawn as 1bit", known_failure: one_bit.merge(bwr_kit => bwr_2bit_red) do
      level = digit(0, 3)
      show("two-colors.png", png(level, 2), "image/png") do |s|
        expect(s).to show_image(expected(level, 2), tolerance: 64, max_ratio: 0)
      end
    end

    it "2bit png uses 4 gray levels", :smoke,
       known_failure: { "trmnl_gen2_4clr" => FirmwareBugs::GEN2_4CLR_IMAGE, bwr_kit => bwr_4gray,
                        "seeed_sticky" => sticky_4gray } do
      level = ->(x, y) { seven.(x, y) ? 0 : [3, x * 4 / w].min }
      show("gray4.png", png(level, 2), "image/png") do |s|
        expect(s).to show_image(expected(level, 2), tolerance: 48, max_ratio: 0.01)
      end
    end

    it "8bit gray png is reduced", known_failure: shifted.merge(bwr_kit => bwr_4gray, "seeed_sticky" => sticky_4gray) do
      level = digit(0, 255)
      show("gray8.png", png(level, 8), "image/png") do |s|
        expect(s).to show_image(expected(level, 8), tolerance: 64, max_ratio: 0)
      end
    end

    it "4bit gray png is reduced",
       known_failure: { envs_with_inks.("mono") => reduce_bpp_4bit, "trmnl_gen2_4clr" => FirmwareBugs::GEN2_4CLR_IMAGE,
                        bwr_kit => bwr_4gray } do
      # a known failure on the black-and-white panels (reduce_bpp_4bit)
      level = digit(0, 15)
      show("gray16.png", png(level, 4), "image/png") do |s|
        expect(s).to show_image(expected(level, 4), tolerance: 64, max_ratio: 0)
      end
    end

    it "palette png is reduced", known_failure: one_bit.merge(bwr_kit => bwr_2bit_red) do
      colors = [[0, 0, 0], [255, 255, 255]]
      data = TrmnlSim::Images.png_palette(->(x, y) { seven.(x, y) ? colors[0] : colors[1] }, colors, w, h)
      show("palette.png", data, "image/png") do |s|
        expect(s).to show_image(expected(digit(0, 1), 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "truecolor png is reduced",
       known_failure: shifted.merge(bwr_kit => bwr_4gray, "seeed_sticky" => sticky_4gray,
                                    "seeed_reTerminal_E1004" => e1004_png_buffer, "trmnl_4clr" => bwry_truecolor) do
      data = TrmnlSim::Images.png_rgb(->(x, y) { seven.(x, y) ? [0, 0, 0] : [255, 255, 255] }, w, h)
      show("rgb.png", data, "image/png") do |s|
        expect(s).to show_image(expected(digit(0, 1), 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "portrait png is rotated" do
      px = panel_number("7")
      level = ->(x, y) { px.(y, h - 1 - x) ? 0 : 1 } # the digit, turned
      show("portrait.png", png(level, width: h, height: w), "image/png") do |s|
        expect(s.screenshot).not_to be_empty
      end
    end

    it "larger png is cropped",
       known_failure: one_bit.merge(envs_with_inks.("gray16") => fastepd_wide_png, "trmnl_4clr" => bwry_wide_png) do
      px = panel_number("7")
      level = ->(x, y) { x < w && y < h && px.(x, y) ? 0 : 1 }
      show("large.png", png(level, width: w + 200, height: h + 120), "image/png") do |s|
        expect(s).to show_image(expected(digit(0, 1), 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "corrupt png is not drawn" do
      data = png(digit(0, 1))
      survives("corrupt.png", data.byteslice(0, 40) + zeros(data.bytesize - 40), "image/png")
    end

    it "same png again is not downloaded or redrawn" do
      level = digit(0, 1)
      show("again.png", png(level), "image/png") do |s|
        refreshes = s.status["display_refreshes"]
        n = dev.mock.cursor
        dev.mock.next_request("/api/display", timeout: 90) { s.wake }
        s.wait(state: "deep_sleep", display_idle: true, timeout: 90, settle_ms: 200)
        expect(dev.mock.paths.drop(n)).to eq(["/api/display"])
        expect(s.status["display_refreshes"]).to eq(refreshes)
      end
    end

    it "new version of a plugin image replaces the cached one", known_failure: one_bit do
      m = dev.mock
      old = png(digit(0, 1, "1"))
      new = png(digit(0, 1, "2"))
      m.set_file("/img/v1.png", "image/png", old)
      m.set_file("/img/v2.png", "image/png", new)
      m.display_queue = [{ image_url: "#{m.device_url}/img/v1.png", filename: "plugin-abc123-1000", refresh_rate: 300 }]
      m.display = { image_url: "#{m.device_url}/img/v2.png", filename: "plugin-abc123-2000", refresh_rate: 300 }
      dev.boot do |s|
        m.wait_for_request("/img/v1.png", timeout: 15)
        s.wait(state: "deep_sleep", display_idle: true, timeout: 15)
        c = s.status["console_total"]
        s.wake
        s.wait(console: /Deleting older version of plugin image/, since: c, timeout: 15)
        s.wait(state: "deep_sleep", display_idle: true, timeout: 15)
        expect(s).to show_image(expected(digit(0, 1, "2"), 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "long filenames are shortened", known_failure: one_bit do
      level = digit(0, 1)
      path = serve("long.png", png(level), "image/png")
      dev.mock.display[:filename] = "mashup-066cc3-weather-and-calendar-1771674964"
      dev.boot do |s|
        dev.mock.wait_for_request(path, timeout: 15)
        s.wait(state: "deep_sleep", display_idle: true, timeout: 15)
        expect(s).to show_image(expected(level, 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "temperature profile is saved", known_failure: one_bit do
      level = digit(0, 1)
      show("temp.png", png(level), "image/png", temperature_profile: "a") do |s|
        expect(s).to show_image(expected(level, 1), tolerance: 64, max_ratio: 0)
        show_next(s, "temp2.png", png(digit(0, 1, "8")), temperature_profile: "b")
        expect(s).to show_image(expected(digit(0, 1, "8"), 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "maximum compatibility forces full refreshes", known_failure: one_bit do
      level = digit(0, 1)
      show("compat.png", png(level), "image/png", maximum_compatibility: true) do |s|
        expect(s).to show_image(expected(level, 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "long refresh rates use fast instead of partial refreshes", known_failure: one_bit do
      show("slow1.png", png(digit(0, 1)), "image/png", refresh_rate: 3600) do |s|
        eight = digit(0, 1, "8")
        show_next(s, "slow2.png", png(eight), refresh_rate: 3600)
        expect(s).to show_image(expected(eight, 1), tolerance: 64, max_ratio: 0)
      end
    end
  end

  describe "Jpeg" do
    it "jpeg is dithered to 1bit",
       known_failure: { envs_with_inks.("bwry", "spectra6") => color_jpeg,
                        %w[seeed_sticky m5_paper_mono] => ssd1677_window } do
      show("five.jpg", data_file("five_#{w}x#{h}.jpg"), "image/jpeg") do |s|
        # dithering edges: up to 2% of an 800x480 screen, as many pixels on other panels (a share
        # of a big panel would let a blank screen pass)
        expect(s).to show_image(expected(digit(0, 1, "5"), 1), tolerance: 64, max_ratio: [0.02, 7680.0 / (w * h)].min)
      end
    end

    it "jpeg of the wrong size is refused" do
      survives("small.jpg", data_file("five_640x480.jpg"), "image/jpeg") # no panel is 640x480
    end
  end

  describe "Bmp" do
    it "bmp with an inverted palette",
       pending: "parseBMPHeader's image_reverse is never used (display_show_image's inversion is under #ifdef " \
                "FUTURE): the image is shown inverted" do
      # parseBMPHeader recognizes the white/black palette ("Color scheme reversed") and sets
      # image_reverse, but nothing uses it (display_show_image's inversion is under #ifdef
      # FUTURE): the image is shown inverted.
      level = digit(0, 1)
      # index 0 = white, 1 = black
      data = bmp(palette: [255, 255, 255, 0, 0, 0, 0, 0].pack("C*"), black: ->(x, y) { !seven.(x, y) })
      show("inverted.bmp", data, "image/bmp") do |s|
        expect(s).to show_image(expected(level, 1), tolerance: 64, max_ratio: 0)
      end
    end

    it "not a bmp" do
      survives("garbage.bmp", "XX".b + zeros(bmp.bytesize - 2), "image/bmp")
    end

    it "bmp of the wrong size" do
      survives("small.bmp", bmp(w / 2, h / 2), "image/bmp")
    end

    it "bmp with a color palette" do
      survives("red.bmp", bmp(palette: [0, 0, 255, 0, 255, 255, 255, 0].pack("C*")), "image/bmp")
    end

    it "bmp with a bad data offset" do
      survives("offset.bmp", bmp(offset: 100), "image/bmp")
    end
  end

  describe "Refused" do
    it "image too large to download" do
      boot_and_fetch(serve("huge.png", zeros(device.max_image + 5000), "image/png"))
    end

    it "empty image" do
      boot_and_fetch(serve("empty.png", "".b, "image/png"))
      # Content-Length 0 takes the "no Content-Length" path (contentLength <= 0): if the server's
      # close has already arrived, writeToStream() reports the download as cut short; otherwise it
      # reads nothing and finishBody() says "No data received".
      log = dev.mock.wait_for_request("/api/log", timeout: 10)
      expect(log.body).to match(/No data received|connection closed mid-download/)
    end

    it "missing image" do
      dev.mock.display = { image_url: "#{dev.mock.device_url}/img/missing.png", filename: "plugin-000000-1",
                           refresh_rate: 300 }
      boot_and_fetch("/img/missing.png") do |_, st|
        expect(st["wake_at_s"] - st["sim_time_s"]).to be < 300
      end
    end
  end

  describe "BwryPng", env: "trmnl_4clr" do
    fixture(:dev) { ProvisionedDevice.new(Builds.for_env("trmnl_4clr")) }

    # The TRMNL BWRY's 800x480 panel.
    let(:og_seven) { TrmnlSim::Images.big_number("7") }

    def og_digit(level_black, level_white, text = "7")
      px = TrmnlSim::Images.big_number(text)
      ->(x, y) { px.(x, y) ? level_black : level_white }
    end

    it "1bit png" do
      level = og_digit(0, 1)
      show("one.png", png(level, width: 800, height: 480), "image/png") do |s|
        expect(s).to show_image(TrmnlSim::Images.expected_gray(level, 800, 480), tolerance: 64, max_ratio: 0)
      end
    end

    it "8bit gray png" do
      level = og_digit(0, 255)
      show("gray8.png", png(level, 8, width: 800, height: 480), "image/png") do |s|
        expect(s).to show_image(TrmnlSim::Images.expected_gray(level, 800, 480, bits: 8), tolerance: 64, max_ratio: 0)
      end
    end

    it "truecolor png", pending: bwry_truecolor do
      # trmnl_4clr doesn't set PNG_MAX_BUFFERED_PIXELS (trmnl does), so PNGdec's default row
      # buffer is too small for 800 px truecolor rows: the decode corrupts the heap (store fault
      # in tlsf_free), and the device crashes, restarts, downloads the same image and crashes
      # again, until the server sends something else.
      survives("rgb.png", TrmnlSim::Images.png_rgb(->(x, y) { og_seven.(x, y) ? [255, 0, 0] : [255, 255, 255] }),
               "image/png")
    end

    it "jpeg" do
      survives("five.jpg", data_file("five_800x480.jpg"), "image/jpeg")
    end
  end
end
