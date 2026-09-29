# frozen_string_literal: true

require "digest"
require "fileutils"
require "json"
require "tmpdir"

# On-disk cache of the devices the tests start from (a factory-fresh X, onboarded devices), so
# a local run doesn't redo the factory flow and onboarding every time.
#
# An entry is keyed by everything that went into it: the firmware build's files, the simulator
# binary, the test support code, the fixture's own parameters and the memcheck/turbo settings.
# Change any of them and the key changes, so the entry is rebuilt on the next run; nothing has to
# be invalidated by hand. Entries live in target/spec-cache/ (`cargo clean` removes them), and
# only the newest few per fixture are kept.
#
# `rake "spec[--no-cache]"` (TRMNL_SPEC_NO_CACHE=1) builds everything from scratch, as CI does, and
# so does a coverage run (TRMNL_SIM_COVERAGE), whose report should include the setup flows.
# Parallel workers that need the same missing entry build it once: the others wait for it.
module SetupCache
  DIR = File.join(Builds::ROOT, "target/spec-cache")
  ENABLED = ENV.fetch("TRMNL_SPEC_NO_CACHE", "").empty? && ENV.fetch("TRMNL_SIM_COVERAGE", "").empty?
  # Bump when what an entry holds changes shape.
  FORMAT = 2
  # Entries kept per fixture name (e.g. while switching between firmware branches).
  KEEP = 3
  # What the simulator loads from a PlatformIO build dir (see src/firmware.rs).
  BUILD_FILES = %w[firmware.elf bootloader.elf firmware.bin bootloader.bin partitions.bin merged_firmware.bin
                   littlefs.bin spiffs.bin].freeze
  # The code that drives the setup flows.
  SUPPORT_FILES = [
    *Dir[File.join(Builds::HERE, "lib/**/*.rb")],
    *%w[setup_cache.rb sims.rb provisioned_device.rb trmnl_x.rb].map { |f| File.join(__dir__, f) }
  ].sort.freeze

  @hashes = {}

  module_function

  def file_hash(path)
    st = File.stat(path)
    @hashes[[path, st.mtime.to_r, st.size]] ||= Digest::SHA256.file(path).hexdigest
  end

  def build_id(build)
    BUILD_FILES.filter_map { |f| [f, file_hash(File.join(build, f))] if File.exist?(File.join(build, f)) }.to_h
  end

  def sim_id = file_hash(TrmnlSim.binary)

  # The directory the block filled for these inputs, and the hash it returned (kept as JSON, so
  # with string keys). Built now unless cached. The directory is shared with other runs and
  # workers: copy files out of it before changing them.
  def entry(name, inputs)
    unless ENABLED
      dir = Dir.mktmpdir("trmnl-#{name}-")
      at_exit { FileUtils.rm_rf(dir) }
      return [dir, JSON.parse(JSON.generate(yield(dir)))]
    end

    inputs = { name:, format: FORMAT, sim: sim_id, support: SUPPORT_FILES.map { file_hash(_1) } }.merge(inputs)
    key = Digest::SHA256.hexdigest(JSON.generate(sort_keys(inputs)))[0, 20]
    FileUtils.mkdir_p(DIR)
    dir = File.join(DIR, "#{name}-#{key}")
    meta = File.open("#{dir}.lock", File::CREAT | File::WRONLY) do |lock|
      lock.flock(File::LOCK_EX)
      if File.exist?(File.join(dir, "meta.json"))
        FileUtils.touch(dir) # recently used: kept by prune
        next JSON.parse(File.read(File.join(dir, "meta.json")))
      end

      tmp = Dir.mktmpdir(".#{name}-", DIR)
      begin
        made = yield(tmp)
        File.write(File.join(tmp, "meta.json"), JSON.generate(made))
        File.rename(tmp, dir)
        JSON.parse(JSON.generate(made))
      rescue Exception # rubocop:disable Lint/RescueException -- a half-made entry never stays
        FileUtils.rm_rf(tmp)
        raise
      end
    end
    prune(name)
    [dir, meta]
  end

  def sort_keys(value)
    case value
    when Hash then value.to_h { |k, v| [k.to_s, sort_keys(v)] }.sort.to_h
    when Array then value.map { sort_keys(_1) }
    else value
    end
  end

  # Keep the KEEP most recently used entries of `name`. Other workers prune too: an entry can
  # vanish between listing it and looking at it.
  def prune(name)
    entries = Dir[File.join(DIR, "#{name}-*")].filter_map do |p|
      next unless File.basename(p).count("-") == name.count("-") + 1

      stat = File.stat(p)
      [p, stat.mtime] if stat.directory?
    rescue Errno::ENOENT
      nil
    end
    entries.sort_by { |_, mtime| -mtime.to_f }.drop(KEEP).map(&:first).each do |old|
      FileUtils.rm_rf(old)
      FileUtils.rm_f("#{old}.lock")
    end
  end
end
