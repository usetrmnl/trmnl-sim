# frozen_string_literal: true

# With TRMNL_SIM_COVERAGE=<dir>, every simulator writes an lcov tracefile there; when the run is
# over (all of parallel_rspec's processes), they are merged into <dir>/merged.info and <dir>/html/
# (TrmnlSim::Lcov), and the coverage of the firmware's own src/ and lib/ is printed.
module CoverageReport
  # The firmware's own sources, in the tracefiles' paths (relative to its checkout).
  FIRMWARE_SOURCES = %w[src/ lib/].freeze

  module_function

  def dir = ENV.fetch("TRMNL_SIM_COVERAGE", "").then { _1.empty? ? nil : _1 }

  def print(dir)
    warn "\nFirmware coverage (src/, lib/); full report in #{File.join(dir, 'html')}"
    all = TrmnlSim::Lcov::Report.load([dir])
    all.write_lcov(File.join(dir, TrmnlSim::Lcov::MERGED))
    # Relative paths in the tracefiles are relative to the firmware checkout.
    all.write_html(File.join(dir, "html"), root: Builds::FIRMWARE)
    puts all.total_line
    own = TrmnlSim::Lcov::Report.load([dir], include: FIRMWARE_SOURCES)
    puts own.summary, "(#{own.tracefiles} tracefiles)"
    builds = Dir[File.join(dir, "*/")].select { |d| File.basename(d) != "html" && Dir[File.join(d, "*.info")].any? }
    return if builds.size < 2

    warn "\nPer build (src/, lib/ lines each build compiles):"
    builds.sort.each do |b|
      warn "  #{File.basename(b).ljust(32)} #{TrmnlSim::Lcov::Report.load([b], include: FIRMWARE_SOURCES).total_line}"
    end
  end
end
