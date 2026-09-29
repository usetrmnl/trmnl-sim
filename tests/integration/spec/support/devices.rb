# frozen_string_literal: true

# The devices the integration tests know, by PlatformIO environment, and the device under test.
#
# General tests (groups with `env: :any`: setup, portal, WiFi, HTTP, errors, faults...) run
# against whichever device is under test: TRMNL_SIM_DEVICE=<env> (the runner sets it for
# `bin/spec <env>`), else the TRMNL OG. They read what they need to know about it from its
# `Device` (panel size, inks, button, battery...), skip what doesn't apply (`needs:`, `only_on:`
# metadata), and `known_failure:` metadata marks what a device's firmware gets wrong.
module Devices
  class Device
    ATTRIBUTES = {
      env: nil,            # PlatformIO environment (the build directory's name)
      model: nil,          # the firmware's DEVICE_MODEL (the Model header)
      name: nil,           # the simulator's board name
      size: [800, 480],    # the panel as the device reports it (Width/Height headers)
      inks: "mono",        # mono, bwr, bwry, spectra6 or gray16
      chip: "esp32s3",
      # Battery-Voltage it reports at the simulator's default 4.1 V battery (nil: varies)
      battery_v: 4.1,
      # Has a button the simulator can press (and that wakes it from deep sleep): `press` wakes
      # it, a 5 s press forgets WiFi. On the X, its touch bar equivalents (TrmnlX::XSim).
      button: true,
      # A double click (or a 1-5 s press) runs the special function the server assigned.
      double_click: true,
      # A 15 s press forgets the device (soft reset).
      soft_reset_press: true,
      # Goes through the X's factory flow: modem flashing, then shipment mode until docked (and
      # back to shipment mode when the setup portal times out); the general tests start it from
      # its unboxed state (TrmnlX.unboxed).
      shipment: false,
      # Keeps its frame buffers in PSRAM (else it doesn't use PSRAM at all).
      psram_frame_buffers: false,
      app_slot: 0x10000,   # offset of the first app slot (the factory-flashed firmware)
      ota_slot: 0x1E0000,  # offset of the second app slot, which an OTA update installs into
      # Reads the panel revision (Panel-Rev header): the 7.5" UC8179 boards whose SPI pins the
      # firmware knows (get_panel_rev() in display.cpp).
      panel_rev: false,
      # Has an I2C bus for environment sensors (device_list[] sensor_sda/scl != 0xff): SCD41 /
      # AHT20 readings go in the SENSORS header.
      sensors: false,
      # Battery-Voltage follows the battery (ADC divider, fuel gauge or PMIC). False: the board
      # has no way to measure it (no divider on the pin, BATT_NONE) and always reports battery_v.
      battery_tracks: true,
      # Why the general tests can't run on it (nil: they can).
      general: nil,
      # A BOARD_HAS_PSRAM build: takes images up to 750000 bytes instead of 90000 (config.h).
      psram: true
    }.freeze
    FEATURES = %i[button double_click soft_reset_press shipment psram_frame_buffers panel_rev sensors
                  battery_tracks psram].freeze

    attr_reader(*ATTRIBUTES.keys)

    def initialize(env, model, name, **attrs)
      unknown = attrs.keys - ATTRIBUTES.keys
      raise ArgumentError, "unknown Device attributes #{unknown}" if unknown.any?

      ATTRIBUTES.merge(attrs, env:, model:, name:).each { |k, v| instance_variable_set(:"@#{k}", v.freeze) }
      freeze
    end

    FEATURES.each { |f| define_method(:"#{f}?") { public_send(f) } }

    # Whether it has `feature` (one of FEATURES).
    def has?(feature)
      raise ArgumentError, "unknown Device feature #{feature.inspect}" unless FEATURES.include?(feature.to_sym)

      public_send(feature)
    end

    def width = size[0]
    def height = size[1]

    # The largest image download the firmware accepts (MAX_IMAGE_SIZE).
    def max_image = psram ? 750_000 : 90_000

    # Draws its message screens (errors, QA) in the OG's font (nicoclean_8) at the OG's rows,
    # centred: the OG's text goldens fit, shifted by half the width difference. Parallel
    # (FastEPD) panels use Inter_18 and get goldens of their own.
    def og_font? = inks != "gray16"

    # Update-Source after a button wake: GPIO wakeup ("button") on the C3 and C5, EXT0 on the S3.
    def button_source = chip == "esp32s3" ? "EXT0" : "button"

    # The Update-Source of a wake by `press` (see button_source).
    def press_source = button_source

    # Takes the TRMNL server's 800x480 1-bit BMP as the default screen (other panels get a PNG
    # of their size).
    def default_bmp? = size == [800, 480] && %w[mono bwry].include?(inks)

    def to_s = name
    def inspect = "#<Device #{env}>"
  end

  ALL = [
    Device.new("trmnl", "og", "TRMNL OG", chip: "esp32c3", panel_rev: true, sensors: true, psram: false),
    Device.new("trmnl_4clr", "og_4clr", "TRMNL BWRY", inks: "bwry", chip: "esp32c3", sensors: true, psram: false),
    Device.new("TRMNL_X", "x", "TRMNL X",
               size: [1872, 1404], inks: "gray16", battery_v: nil, double_click: false, soft_reset_press: false,
               shipment: true, app_slot: 0x20000, ota_slot: 0x320000, psram_frame_buffers: true),
    Device.new("seeed_reTerminal_E1002", "reterminal_e1002", "reTerminal E1002", inks: "spectra6"),
    Device.new("seeed_xiao_esp32c3", "seeed_esp32c3", "XIAO ESP32-C3 + 7.5\" panel",
               chip: "esp32c3", battery_v: 0.0, panel_rev: true, battery_tracks: false, psram: false),
    Device.new("seeed_xiao_esp32s3", "seeed_esp32s3", "XIAO ESP32-S3 + 7.5\" panel",
               battery_v: 0.0, panel_rev: true, battery_tracks: false),
    Device.new("TRMNL_7inch5_OG_DIY_Kit", "xiao_epaper_display", "TRMNL 7.5\" DIY Kit", panel_rev: true),
    Device.new("TRMNL_7inch5_OG_DIY_Kit_3CLR", "xiao_epaper_3clr", "TRMNL 7.5\" BWR DIY Kit", inks: "bwr"),
    Device.new("TRMNL_7inch5_OG_DIY_Kit_6CLR", "xiao_epaper_6clr", "TRMNL 7.3\" Spectra 6 DIY Kit", inks: "spectra6"),
    Device.new("TRMNL_4inch26_DIY_Kit", "xiao_epaper_mini", "TRMNL 4.26\" DIY Kit"),
    Device.new("seeed_reTerminal_E1001", "reterminal_e1001", "reTerminal E1001", panel_rev: true),
    Device.new("seeed_reTerminal_E1004", "reterminal_e1004", "reTerminal E1004", size: [1200, 1600], inks: "spectra6"),
    Device.new("seeed_sticky", "seeed_sticky", "Seeed Sticky", sensors: true),
    Device.new("xteink_x4", "xteink_x4", "Xteink X4", chip: "esp32c3", battery_v: 0.0, psram: false),
    Device.new("xteink_x3", "xteink_x3", "Xteink X3", size: [792, 528], chip: "esp32c3", sensors: true, psram: false),
    Device.new("WAVESHARE_397", "waveshare_397", "Waveshare ESP32-S3 3.97\"", sensors: true),
    Device.new("CrowPanel42", "crowpanel42", "CrowPanel 4.2\"",
               size: [400, 300], battery_v: 4.2, battery_tracks: false, psram: false),
    Device.new("m5_paper_mono", "m5_paper_mono", "M5Paper Mono",
               battery_v: 4.2, battery_tracks: false, sensors: true, psram: false),
    Device.new("m5_paper_color", "m5_paper_color", "M5Paper Color",
               size: [400, 600], inks: "spectra6", battery_v: 4.2, battery_tracks: false, sensors: true),
    Device.new("TRMNL_X_PAPERS3", "m5_papers3", "M5Stack PaperS3", size: [960, 540], inks: "gray16", button: false),
    Device.new("TRMNL_X_LILYGO_T5PRO", "lilygo_t5pro", "LilyGo T5 4.7\" S3 Pro",
               size: [960, 540], inks: "gray16", sensors: true),
    Device.new("TRMNL_X_SENSORIAC5", "sensoria_c5", "Sensoria C5",
               size: [1280, 720], inks: "gray16", chip: "esp32c5", battery_v: 0.0, battery_tracks: false, sensors: true,
               general: "the firmware reboots instead of sleeping (see byod_parallel_spec SensoriaC5)"),
    Device.new("trmnl_steam", "trmnl_steam", "TRMNL Steam",
               size: [648, 480], chip: "esp32c3", sensors: true, psram: false,
               general: "the firmware never boots (see byod_uc81xx_spec TrmnlSteamBoots)"),
    Device.new("trmnl_gen2", "og_gen2", "TRMNL OG gen 2", chip: "esp32c5", panel_rev: true, sensors: true),
    Device.new("trmnl_gen2_4clr", "og_gen2_4clr", "TRMNL BWRY gen 2", inks: "bwry", chip: "esp32c5", sensors: true)
  ].freeze

  BY_ENV = ALL.to_h { |d| [d.env, d] }.freeze

  # ---- how much of the general suite runs where (the runner's tiers) ---------------------------------
  #
  # Devices share firmware code paths by chip, panel controller and inks. `bin/spec` runs every
  # device's own tests, the full general suite on the OG, and the examples tagged :smoke (one
  # per area) on every other device; `--comprehensive` runs the full general suite on one
  # representative per family as well; `--exhaustive`, on every device.
  FAMILIES = {
    # family: its devices, the representative first
    "ESP32-C3, UC81xx, black and white" => %w[trmnl seeed_xiao_esp32c3 xteink_x3 trmnl_steam],
    "ESP32-C3, UC81xx, 4-color" => %w[trmnl_4clr],
    "ESP32-C3, SSD16xx" => %w[xteink_x4],
    "ESP32-S3, UC81xx, black and white" => %w[seeed_reTerminal_E1001 seeed_xiao_esp32s3 TRMNL_7inch5_OG_DIY_Kit
                                              TRMNL_7inch5_OG_DIY_Kit_3CLR],
    "ESP32-S3, UC81xx, Spectra 6" => %w[seeed_reTerminal_E1002 TRMNL_7inch5_OG_DIY_Kit_6CLR seeed_reTerminal_E1004
                                        m5_paper_color],
    "ESP32-S3, SSD16xx" => %w[CrowPanel42 TRMNL_4inch26_DIY_Kit WAVESHARE_397 seeed_sticky m5_paper_mono],
    "ESP32-S3, parallel, TRMNL X" => %w[TRMNL_X],
    "ESP32-S3, parallel, BYOD" => %w[TRMNL_X_PAPERS3 TRMNL_X_LILYGO_T5PRO],
    "ESP32-C5" => %w[trmnl_gen2 trmnl_gen2_4clr TRMNL_X_SENSORIAC5]
  }.freeze

  REPRESENTATIVES = FAMILIES.values.map(&:first).freeze

  module_function

  # The device of PlatformIO environment `env` (case-insensitive).
  def fetch(env)
    env = env.to_s
    BY_ENV[env] || ALL.find { |d| d.env.casecmp?(env) } ||
      raise(KeyError, "unknown device #{env.inspect} (known: #{BY_ENV.keys.join(', ')})")
  end

  def known?(env) = BY_ENV.key?(env.to_s)

  # The device the general tests run on: TRMNL_SIM_DEVICE, else the TRMNL OG.
  def under_test = fetch(ENV.fetch("TRMNL_SIM_DEVICE", "").then { |e| e.empty? ? "trmnl" : e })
end
