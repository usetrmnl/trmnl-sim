# frozen_string_literal: true

# The integration tests: real firmware builds in the simulator against mock servers. They are
# plain RSpec; ENVS says which devices a run covers (spec/support/selection.rb):
#
#   bundle exec rspec                                  # serially: the default ENVS
#   bundle exec rspec spec/general/setup/portal_spec.rb:42
#   ENVS="xteink_x4:full" bundle exec rspec            # everything for one device
#   rake spec                                          # the simulator built, then parallel_rspec
#   rake "spec[-n 4 spec/core]"                        # parallel_rspec's arguments pass through
#   rake "firmware[xteink_x4]" spec                    # pio run -e xteink_x4 first
#   rake check                                         # rubocop, and the specs load (no firmware needed)
#   rake "coverage[cov/ --include src/ -q]"            # merge and report lcov tracefiles (TrmnlSim::Lcov)
#
# A task's argument is one string, split like a shell command line (quote the task in zsh: its
# brackets are globs).
#
# Uses ../trmnl-firmware/.pio/build/<env> (TRMNL_FIRMWARE=<checkout> to use another one); a
# listed device whose build is missing is left out. The firmware is only built by `rake
# firmware`. Onboarded devices etc. are cached in the simulator checkout's target/spec-cache/,
# keyed by the firmware, the simulator and the spec support code. See the README's "Integration
# testing" for the environment variables.

require "shellwords"

require_relative "lib/trmnl_sim"
require_relative "spec/support/devices"
require_relative "spec/support/builds"

# A task's argument as argv (Rake splits arguments at commas: put them back).
def task_args(args) = Shellwords.split([args[:args], *args.extras].compact.join(","))

task default: :spec

desc "Run the integration specs in parallel (argument: parallel_rspec's, e.g. -n 4 spec/core)"
task :spec, [:args] => :sim do |_, args|
  cmd = ["bundle", "exec", "parallel_rspec", "--serialize-stdout", "--combine-stderr", *task_args(args)]
  sh(*cmd, chdir: Builds::HERE) { |ok, _| exit(1) unless ok } # failures are reported above; no backtrace
end

desc "Build the simulator (cargo build --release) unless TRMNL_SIM_BIN names one"
task :sim do
  next if ENV.fetch("TRMNL_SIM_BIN", "") != ""
  unless File.exist?(File.join(TrmnlSim::REPO, "Cargo.toml"))
    next warn("no trmnl-sim checkout at #{TrmnlSim::REPO}: set TRMNL_SIM_REPO or TRMNL_SIM_BIN")
  end

  sh "cargo", "build", "--release", chdir: TrmnlSim::REPO
end

# Never `pio run -c <other.ini>` here: another project config wipes every env's .pio/build.
desc "Build firmware with PlatformIO (argument: environments; default: the core devices')"
task :firmware, [:envs] do |_, args|
  envs = task_args(args)
  envs = Devices::CORE.map(&:env) if envs.empty?
  sh "pio", "run", "-d", Builds::FIRMWARE, *envs.flat_map { ["-e", _1] }
end

desc "Merge lcov tracefiles and report (argument: tracefiles or directories, -o FILE, --include PREFIX, " \
     "--html DIR, --root DIR, -q)"
task :coverage, [:args] do |_, args|
  status = TrmnlSim::Lcov.main(task_args(args))
  exit(status) unless status.zero?
end

desc "Check the specs' style (rubocop)"
task :lint do
  sh "bundle", "exec", "rubocop", chdir: Builds::HERE
end

desc "Lint, and check every spec loads and its metadata is valid for every device (no firmware needed)"
task check: :lint do
  sh({ "ENVS" => "all:full" }, "bundle", "exec", "rspec", "--dry-run", "--format", "progress", chdir: Builds::HERE)
end
