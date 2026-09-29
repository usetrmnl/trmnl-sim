# frozen_string_literal: true

# The integration tests: real firmware builds in the simulator against mock servers.
#
#   rake spec                                  # every device: its own specs, the general ones in
#                                              # full on the OG and the :smoke examples on the others
#   rake spec:comprehensive                    # the general specs in full on one device per family too
#   rake spec:exhaustive                       # the general specs in full on every device (slow)
#   rake "spec[trmnl_x]"                       # a spec file (spec/trmnl_x_spec.rb), path, path:line or path[id]
#   rake "spec[refresh_cycle -e 'long press']" # rspec options pass through (-e, --tag, ...)
#   rake "spec[xteink_x4]"                     # every spec of a PlatformIO environment
#   rake "spec[-j 1 --slow --no-cache]"        # one unit at a time, also slow: examples, no setup cache
#   rake "spec:plan[xteink_x4]"                # what would run
#   rake spec:envs                             # the environments with specs, their example counts and builds
#   rake "firmware[xteink_x4]" "spec[xteink_x4]"   # pio run -e xteink_x4 first, then its specs
#   rake check                                 # rubocop, and the specs load (no firmware needed)
#
# A task's argument is one string, split like a shell command line, of what runner/runner.rb
# takes (quote the task in zsh: its brackets are globs). `rake spec` builds the simulator first
# (`rake sim`, cargo build --release) unless TRMNL_SIM_BIN names one.
#
# Uses ../trmnl-firmware/.pio/build/<env> (TRMNL_FIRMWARE=<checkout> to use another one); specs skip
# themselves when their build is missing (asking for an environment whose build is missing
# fails). The firmware is only built by `rake firmware`. Onboarded devices etc. are cached in
# the simulator checkout's target/spec-cache/, keyed by the firmware, the simulator and the spec
# support code. See the README's "Integration testing" for the environment variables.

require "shellwords"

require_relative "runner/runner"

# The firmware the TRMNL devices' specs use (`rake firmware` without environments).
TRMNL_ENVS = %w[trmnl trmnl_4clr TRMNL_X seeed_reTerminal_E1002].freeze

# A task's argument as the runner's argv (Rake splits arguments at commas: put them back).
def runner_args(args) = Shellwords.split([args[:args], *args.extras].compact.join(","))

def run_specs(*argv)
  exit(1) unless Runner.main(argv)
end

task default: :spec

desc "Run the integration specs (argument: spec files, environments, runner and rspec options)"
task :spec, [:args] => :sim do |_, args|
  run_specs(*runner_args(args))
end

namespace :spec do
  desc "The general specs in full on one device per family as well"
  task :comprehensive, [:args] => :sim do |_, args|
    run_specs("--comprehensive", *runner_args(args))
  end

  desc "The general specs in full on every device"
  task :exhaustive, [:args] => :sim do |_, args|
    run_specs("--exhaustive", *runner_args(args))
  end

  desc "Print the units (rspec processes) `rake spec` would run with the same argument"
  task :plan, [:args] do |_, args|
    run_specs("--dry-run", *runner_args(args))
  end

  desc "List the environments with specs, their example counts and whether they are built"
  task :envs do
    run_specs("--list-envs")
  end
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
desc "Build firmware with PlatformIO (argument: environments; default: #{TRMNL_ENVS.join(' ')})"
task :firmware, [:envs] do |_, args|
  envs = runner_args(args)
  envs = TRMNL_ENVS if envs.empty?
  sh "pio", "run", "-d", Builds::FIRMWARE, *envs.flat_map { ["-e", _1] }
end

desc "Check the specs' style (rubocop)"
task :lint do
  sh "bundle", "exec", "rubocop", chdir: Builds::HERE
end

desc "Lint, and check every spec loads and its metadata is valid (no firmware needed)"
task check: :lint do
  sh "bundle", "exec", "rspec", "--dry-run", "--format", "progress", chdir: Builds::HERE
end
