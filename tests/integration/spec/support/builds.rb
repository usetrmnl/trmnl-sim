# frozen_string_literal: true

# Where the firmware builds are, and the settings every simulator of a run shares.
module Builds
  HERE = File.expand_path("../..", __dir__) # the suite's root (tests/integration)
  # The trmnl-sim checkout (TRMNL_SIM_REPO): its release build, and target/ for the setup cache.
  ROOT = TrmnlSim::REPO
  FIRMWARE = File.expand_path(ENV.fetch("TRMNL_FIRMWARE", File.join(ROOT, "../trmnl-firmware")))
  # PlatformIO build directories of the firmware checkout, one per environment
  # (TRMNL_FIRMWARE_BUILDS=<checkout>/.pio/build for another checkout).
  DIR = ENV.fetch("TRMNL_FIRMWARE_BUILDS", File.join(FIRMWARE, ".pio/build"))
  # Builds that can be moved with their own variables.
  OVERRIDES = {
    "trmnl" => "TRMNL_FIRMWARE_BUILD",
    "trmnl_4clr" => "TRMNL_BWRY_BUILD",
    "TRMNL_X" => "TRMNL_X_BUILD",
    "seeed_reTerminal_E1002" => "TRMNL_E1002_BUILD"
  }.freeze

  TEST_MAC = "7C:DF:A1:00:00:01"
  # TRMNL_SIM_NETWORK=1: tests that reach the real internet run.
  NETWORK = ENV["TRMNL_SIM_NETWORK"] == "1"
  # Turbo (network-aware fast-forward) unless TRMNL_SIM_REALTIME=1.
  TURBO = ENV["TRMNL_SIM_REALTIME"] != "1"
  # TRMNL_SIM_SLOW=1 (rake "spec[--slow]"): also run the examples marked `slow:`.
  SLOW = ENV["TRMNL_SIM_SLOW"] == "1"
  # TRMNL_SIM_MEMCHECK=1: every simulator runs with --memcheck=halt, and a test fails on any
  # memory error (the simulator halts at it, or leaving its block raises).
  MEMCHECK = ENV["TRMNL_SIM_MEMCHECK"] == "1" ? "halt" : nil
  UPDATE_GOLDEN = ENV["TRMNL_SIM_UPDATE_GOLDEN"] == "1"

  module_function

  # The build of PlatformIO environment `env` (it may not exist: see `built?`).
  def of(env) = File.join(DIR, env.to_s)

  # The build the tests of PlatformIO environment `env` use (the OG, BWRY, X and E1002 builds
  # can be moved with their TRMNL_*_BUILD variables).
  def for_env(env)
    var = OVERRIDES[env.to_s]
    File.expand_path((var && ENV.fetch(var, nil)) || of(env))
  end

  def built?(env) = File.exist?(File.join(for_env(env), "firmware.elf"))

  # Why `env`'s tests can't run: its build is missing (nil: they can).
  def missing(env)
    "no #{env} build at #{for_env(env)} (pio run -e #{env})" unless built?(env)
  end

  # The device a build directory is for (nil: unknown).
  def device_of(build)
    build = File.expand_path(build.to_s)
    Devices::ALL.find { |d| for_env(d.env) == build } || Devices::BY_ENV[File.basename(build)]
  end
end
