# frozen_string_literal: true

# The integration test runner behind the Rake tasks (see Rakefile; `rake -T` lists them).
# `Runner.main(argv)` takes what `rake "spec[...]"` is given:
#
#   (nothing)                 every device's own specs, the general ones in full on the OG, the
#                             :smoke examples on every other device
#   refresh_cycle             a spec file or directory by its path under spec/ or its last part
#                             (refresh_cycle, devices/byod, trmnl_x/images), a path, path:line or path[id]
#   refresh_cycle -e "press"  rspec options pass through
#   xteink_x4                 every spec of a PlatformIO environment (or --env NAME)
#   -j 1                      one unit at a time
#   --no-cache                build the devices the specs start from afresh
#   --comprehensive           also the full general suite on a device per family
#   --exhaustive              the full general suite on every device
#   --dry-run                 print what would run
#   --list-envs               the environments and how many examples each has
#   --slow                    also the examples marked slow:
#
# With no selectors, every device's own specs run, the general ones (env: :any) in full on the
# TRMNL OG, and the examples tagged :smoke (one per area) on every other device.
# --comprehensive runs the full general suite on Devices::REPRESENTATIVES (one per family of
# devices sharing chip, panel controller and inks) instead of the smoke examples; --exhaustive,
# on every device.
#
# Each unit (by default, each spec file) runs in its own rspec process, up to -j N (default: the
# number of CPUs) at a time, slowest files first. A file whose top-level group is marked
# `parallel: true` (its fixtures are built lazily) is split further: each of its groups gets a
# process. A unit's output is printed in one piece when it finishes, followed by the combined
# verdict. A single unit, or -j 1, runs with the output streamed as usual.
#
# The devices the specs start from (a factory-fresh X, onboarded devices) come from an on-disk
# cache keyed by the firmware, the simulator and the test support code; see
# spec/support/setup_cache.rb. --no-cache (TRMNL_SPEC_NO_CACHE=1) bypasses it, running every setup
# flow in full, as CI does.
#
# Color is on when stderr is a terminal; NO_COLOR=1 turns it off, FORCE_COLOR=1 turns it on.
#
# With TRMNL_SIM_COVERAGE=<dir>, every simulator writes an lcov tracefile there; afterwards they
# are merged into <dir>/merged.info and <dir>/html/ (scripts/coverage.py), and the coverage of the
# firmware's own src/ and lib/ is printed.

require "etc"
require "json"
require "open3"
require "rbconfig"
require "tmpdir"

require_relative "../lib/trmnl_sim"
require_relative "../spec/support/devices"
require_relative "../spec/support/builds"

module Runner
  HERE = Builds::HERE
  SPEC_DIR = File.join(HERE, "spec")
  # Started first so the longest ones don't end up running alone at the end (slowest first, from
  # a full run); anything not listed follows in name order.
  SLOW_FIRST = %w[devices/trmnl_x/trmnl_x devices/trmnl_x/faults general/tooling/memcheck devices/trmnl_x/memcheck
                  general/faults general/refresh/refresh_cycle].freeze
  # rspec options that take a value.
  VALUE_OPTIONS = %w[-e --example -E --example-matches -t --tag -f --format -o --out -r --require -I -P --pattern
                     --exclude-pattern --seed --order -O --options --default-path].freeze
  COLOR = ENV.fetch("NO_COLOR", "").empty? && (!ENV.fetch("FORCE_COLOR", "").empty? || $stderr.tty?)

  # What one rspec process runs: spec paths (or ids), with the device under test and a tag.
  Unit = Struct.new(:paths, :device, :tag, :name, keyword_init: true) do
    def label = device ? "#{name} [#{device}]" : name
  end

  module_function

  def paint(code, text) = COLOR ? "\e[#{code}m#{text}\e[0m" : text
  def green(text) = paint("32", text)
  def red(text) = paint("1;31", text)
  def yellow(text) = paint("33", text)
  def bold(text) = paint("1", text)

  def main(argv)
    opts = parse(argv)
    ENV["TRMNL_SIM_SLOW"] = "1" if opts[:slow] # read by the specs, here and in workers
    ENV["TRMNL_SPEC_NO_CACHE"] = "1" if opts[:no_cache]
    return list_envs if opts[:list_envs]

    envs, refs = opts[:selectors].partition { env?(_1) }
    units = if envs.any?
              refs.flat_map { split(_1) } + env_units(envs)
            elsif refs.any?
              refs.flat_map { split(_1) }
            else
              plan(opts[:tier])
            end
    return dry_run(units) if opts[:dry_run]

    ok = if units.size == 1 || opts[:jobs] == 1
           run_streamed(units, opts[:rspec])
         else
           run_parallel(units, opts[:jobs], opts[:rspec])
         end
    report_coverage(ENV.fetch("TRMNL_SIM_COVERAGE")) unless ENV.fetch("TRMNL_SIM_COVERAGE", "").empty?
    ok
  end

  def parse(argv)
    opts = { jobs: Etc.nprocessors, selectors: [], rspec: [], tier: "standard" }
    args = argv.dup
    while (a = args.shift)
      case a
      when "--env" then opts[:selectors] << args.shift
      when /\A--env=(.+)\z/ then opts[:selectors] << Regexp.last_match(1)
      when "-j", "--jobs" then opts[:jobs] = Integer(args.shift)
      when /\A(?:-j|--jobs=)(\d+)\z/ then opts[:jobs] = Integer(Regexp.last_match(1))
      when "--slow" then opts[:slow] = true
      when "--no-cache" then opts[:no_cache] = true
      when "--comprehensive", "--exhaustive" then opts[:tier] = a.delete_prefix("--")
      when "--dry-run" then opts[:dry_run] = true
      when "--list-envs" then opts[:list_envs] = true
      when "-v", "--verbose" then nil # the documentation format is the default
      when *VALUE_OPTIONS then opts[:rspec].push(a, args.shift)
      when /\A-/ then opts[:rspec] << a
      else opts[:selectors] << a
      end
    end
    opts[:jobs] = [opts[:jobs], 1].max
    opts
  end

  # A selector that names a PlatformIO environment rather than specs.
  def env?(selector) = Devices.known?(selector) || Devices::ALL.any? { _1.env.casecmp?(selector) }

  # The spec file or directory a selector names: a path (a file optionally with :line or [id]),
  # or a name under spec/ without _spec.rb, in full (general/refresh/refresh_cycle, devices/byod)
  # or as far as it is unique (refresh_cycle, byod, trmnl_x/images).
  def spec_path(ref)
    return ref if ref.match?(/_spec\.rb(\[|:|\z)/) || File.exist?(File.join(HERE, ref))

    name = ref.delete_suffix(".rb").delete_suffix("_spec")
    matches = Dir.glob("spec/**/*", base: HERE).select do |p|
      rel = p.delete_prefix("spec/")
      candidate = File.directory?(File.join(HERE, p)) ? rel : rel.delete_suffix("_spec.rb")
      (p.end_with?("_spec.rb") || File.directory?(File.join(HERE, p))) && !rel.start_with?("support") &&
        (candidate == name || candidate.end_with?("/#{name}"))
    end
    return matches.first if matches.size == 1

    abort "#{ref} names #{matches.join(' and ')}: say which" if matches.any?
    abort "no spec file or directory #{name} under spec/ (environments: #{Devices::BY_ENV.keys.join(', ')})"
  end

  # ---- the suite's structure -----------------------------------------------------------------------------

  # The spec files, each with its units (see PlanFormatter), in run order.
  def suite
    @suite ||= begin
      out = File.join(Dir.mktmpdir("trmnl-plan-"), "plan.json")
      cmd = [*rspec_command, "--dry-run", "--require", File.join(__dir__, "plan_formatter"),
             "--format", "PlanFormatter", "--out", out, "--no-color"]
      log, status = Open3.capture2e(*cmd, chdir: HERE)
      abort "loading the specs failed:\n#{log}" unless status.success? && File.exist?(out)
      JSON.parse(File.read(out), symbolize_names: true)
          .map { |f| f.merge(path: f[:path].delete_prefix("./")) }
          .map { |f| f.merge(stem: f[:path].delete_prefix("spec/").delete_suffix("_spec.rb")) }
          .sort_by { |f| [SLOW_FIRST.index(f[:stem]) || SLOW_FIRST.size, f[:stem]] }
    end
  end

  def rspec_command = %w[bundle exec rspec]

  # The units a selector stands for: each file of a directory, or the file; a file whole, or one
  # unit per group if it is marked `parallel: true`. A path:line or path[id] is one unit.
  def split(ref)
    path = spec_path(ref).delete_prefix("./").chomp("/")
    files = suite.select { |f| f[:path] == path || f[:path].start_with?("#{path}/") }
    return [Unit.new(paths: [path], name: path.delete_prefix("spec/"))] if files.empty?

    files.flat_map { |f| units_of(f, f[:units]) }
  end

  # Units running `units` (of `file`) with `device` under test: the whole file when that is all
  # of it and it keeps its fixtures file-wide, else its groups.
  def units_of(file, units, device: nil, tag: nil)
    return [] if units.empty?

    if units.size == file[:units].size && !file[:parallel]
      [Unit.new(paths: [file[:path]], device:, tag:, name: file[:stem])]
    else
      units.map { |u| Unit.new(paths: [u[:id]], device:, tag:, name: "#{file[:stem]} #{u[:description]}") }
    end
  end

  # Units for environments: the specs specific to each, plus the general ones (env: :any) with
  # it as the device under test. Exits if an environment is unknown or not built.
  def env_units(envs)
    envs.flat_map do |name|
      device = Devices.fetch(name) # PlatformIO names are case-sensitive; accept trmnl_x
      unless Builds.built?(device.env)
        abort "no #{device.env} build at #{Builds.for_env(device.env)}: run `pio run -e #{device.env}` in the " \
              "firmware checkout (or rake \"firmware[#{device.env}]\")"
      end
      warn yellow("#{device.env}: the general specs don't run: #{device.general}") if device.general
      suite.flat_map do |file|
        mine = file[:units].select { |u| u[:env] == device.env || (u[:env] == "any" && !device.general) }
        units_of(file, mine, device: device.env)
      end
    end
  end

  # Units for the whole suite at `tier` (standard, comprehensive or exhaustive).
  def plan(tier)
    units = suite.flat_map { |file| units_of(file, file[:units]) } # the general ones on the OG
    Devices::ALL.each do |device|
      next if device.env == "trmnl" || device.general

      full = tier == "exhaustive" || (tier == "comprehensive" && Devices::REPRESENTATIVES.include?(device.env))
      suite.each do |file|
        general = file[:units].select { _1[:env] == "any" }
        if full
          units += units_of(file, general, device: device.env)
        elsif general.sum { _1[:smoke] }.positive?
          units << Unit.new(paths: [file[:path]], device: device.env, tag: "smoke", name: "#{file[:stem]} smoke")
        end
      end
    end
    units
  end

  def list_envs
    general = suite.sum { |f| f[:units].select { _1[:env] == "any" }.sum { _1[:examples] } }
    smoke = suite.sum { |f| f[:units].select { _1[:env] == "any" }.sum { _1[:smoke] } }
    puts "#{general} general examples; rake spec runs them in full on the OG and the #{smoke} smoke examples on " \
         "the other devices, --comprehensive in full on the devices marked *, --exhaustive in full everywhere\n\n"
    puts " environment                       own total  build"
    Devices::ALL.sort_by { _1.env.downcase }.each do |d|
      own = suite.sum { |f| f[:units].select { _1[:env] == d.env }.sum { _1[:examples] } }
      mark = Devices::REPRESENTATIVES.include?(d.env) ? "*" : " "
      line = format("%<mark>s%<env>-31s %<own>5d %<total>5d  %<built>s",
                    mark:, env: d.env, own:, total: d.general ? own : own + general,
                    built: Builds.built?(d.env) ? "yes" : "missing")
      puts d.general ? "#{line}  (no general examples: #{d.general})" : line
    end
    true
  end

  def dry_run(units)
    units.each { |u| puts "#{u.label}: #{u.paths.join(' ')}#{" --tag #{u.tag}" if u.tag}" }
    puts "#{units.size} units"
    true
  end

  # ---- running -------------------------------------------------------------------------------------------

  def command(unit, rspec_args, *formats)
    [*rspec_command, *(COLOR ? ["--force-color"] : ["--no-color"]), *formats, *(unit.tag ? ["--tag", unit.tag] : []),
     *rspec_args, *unit.paths]
  end

  def env_for(unit) = unit.device ? { "TRMNL_SIM_DEVICE" => unit.device } : {}

  # One unit after the other, output streamed.
  def run_streamed(units, rspec_args)
    units.map { |u| system(env_for(u), *command(u, rspec_args), chdir: HERE) }.all?
  end

  # Each unit in its own rspec process, up to `jobs` at a time.
  def run_parallel(units, jobs, rspec_args)
    tmp = Dir.mktmpdir("trmnl-spec-")
    pending = units.dup
    running = {}
    totals = Hash.new(0)
    failed = []
    t0 = monotonic
    warn bold("Running #{units.size} units, #{[jobs, units.size].min} at a time")
    begin
      until pending.empty? && running.empty?
        while pending.any? && running.size < jobs
          unit = pending.shift
          stem = File.join(tmp, "#{running.size}-#{unit.label.gsub(/[^\w.-]+/, '_')}")
          result = "#{stem}.json"
          log = File.open("#{stem}.log", "w+")
          cmd = command(unit, rspec_args, "--format", "documentation", "--format", "json", "--out", result)
          pid = Process.spawn(env_for(unit), *cmd, chdir: HERE, out: log, err: log, pgroup: true)
          running[pid] = [unit, result, log, monotonic]
        end
        pid, status = Process.wait2(-1)
        next unless running.key?(pid)

        unit, result, log, start = running.delete(pid)
        log.rewind
        output = log.read
        log.close
        summary = File.exist?(result) ? JSON.parse(File.read(result))["summary"] : nil
        ok = status.success? && summary
        summary&.each { |k, v| totals[k] += v if v.is_a?(Integer) }
        failed << unit.label unless ok
        warn "\n#{bold("=== #{unit.label} (#{(monotonic - start).round}s) ")}#{ok ? green('ok') : red('FAILED')}"
        $stderr.write(output)
      end
    rescue Interrupt
      running.each_key { |p| Process.kill("TERM", -p) rescue nil } # rubocop:disable Style/RescueModifier
      raise
    end
    verdict(totals, failed, units.size, jobs, monotonic - t0)
  end

  def verdict(totals, failed, count, jobs, elapsed)
    warn "\n#{'=' * 70}"
    warn "Ran #{totals['example_count']} examples in #{elapsed.round(1)}s (#{count} units, -j #{jobs})\n\n"
    details = { "failures" => totals["failure_count"], "pending or skipped" => totals["pending_count"],
                "errors outside examples" => totals["errors_outside_of_examples_count"] }
              .select { |_, n| n.positive? }.map { |k, n| "#{k}=#{n}" }
    if failed.any?
      warn "#{red('FAILED')} (#{details.join(', ')}) in: #{failed.join(', ')}"
    else
      warn green("OK") + (details.any? ? " (#{details.join(', ')})" : "")
    end
    failed.empty?
  end

  def report_coverage(dir)
    script = File.join(Builds::ROOT, "scripts/coverage.py")
    warn bold("\nFirmware coverage (src/, lib/); full report in #{File.join(dir, 'html')}")
    # Relative paths in the tracefiles are relative to the firmware checkout.
    system("python3", script, dir, "-o", File.join(dir, "merged.info"), "--html", File.join(dir, "html"),
           "--root", Builds::FIRMWARE, "-q")
    system("python3", script, dir, "--include", "src/", "--include", "lib/")
    builds = Dir[File.join(dir, "*/")].select { |d| File.basename(d) != "html" && Dir[File.join(d, "*.info")].any? }
    return if builds.size < 2

    warn bold("\nPer build (src/, lib/ lines each build compiles):")
    builds.sort.each do |b|
      total, = Open3.capture2("python3", script, b, "--include", "src/", "--include", "lib/", "-q")
      warn "  #{File.basename(b).ljust(32)} #{total.strip}"
    end
  end

  def monotonic = Process.clock_gettime(Process::CLOCK_MONOTONIC)
end
