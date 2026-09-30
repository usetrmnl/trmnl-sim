# frozen_string_literal: true

# The devices a run covers: ENVS, a list (spaces or commas between entries) of PlatformIO
# environments (case-insensitive) or families (core, byod, all), each optionally suffixed :full:
#
#   ENVS="trmnl:full TRMNL_X trmnl_4clr trmnl_gen2 trmnl_gen2_4clr"   # the default
#   ENVS=byod                     # every BYOD board
#   ENVS="xteink_x4:full"         # everything for the Xteink X4
#   ENVS=all:full                 # the whole general suite on every device
#
# A listed device runs its own specs and the general specs' :smoke examples; with :full, every
# general example. Examples of devices not listed, or whose build is missing, are left out (see
# `missing`; a dry run keeps those).
module Selection
  DEFAULT = "trmnl:full TRMNL_X trmnl_4clr trmnl_gen2 trmnl_gen2_4clr"
  FAMILIES = { "core" => Devices::CORE, "byod" => Devices::BYOD, "all" => Devices::ALL }.freeze

  module_function

  # The ENVS list in force.
  def spec = ENV.fetch("ENVS", "").strip.then { _1.empty? ? DEFAULT : _1 }

  # {Device => :full or :smoke} for an ENVS list; a device listed twice gets the wider.
  def parse(list)
    list.split(/[\s,]+/).reject(&:empty?).each_with_object({}) do |entry, tiers|
      name, suffix = entry.split(":", 2)
      raise ArgumentError, "ENVS: #{entry}: the only suffix is :full" unless [nil, "full"].include?(suffix)

      devices(name).each { |d| tiers[d] = suffix ? :full : tiers.fetch(d, :smoke) }
    end
  end

  def devices(name)
    FAMILIES.fetch(name.downcase) { [Devices.fetch(name)] }
  rescue KeyError
    raise ArgumentError, "ENVS: no device or family #{name} (families: #{FAMILIES.keys.join(', ')}; devices: " \
                         "#{Devices::BY_ENV.keys.join(', ')})"
  end

  # The devices of ENVS with their tiers.
  def tiers = @tiers ||= parse(spec)

  # The devices the general specs are defined for, with their tiers (those whose firmware can't
  # run them, Device#general, only run their own specs).
  def general = tiers.reject { |d, _| d.general }

  # The listed devices whose build is missing.
  def missing = tiers.keys.reject { Builds.built?(_1.env) }

  # Whether examples of `env` run: its device is listed and built (in a dry run, listed).
  def run?(env)
    device = Devices.fetch(env)
    tiers.key?(device) && (RSpec.configuration.dry_run? || Builds.built?(device.env))
  end

  # Whether the example of metadata `meta` runs: its device runs, and it is :smoke or its device
  # is listed with :full (or its group is one device's own, `general: :own`; see General).
  def run_example?(meta)
    env = meta[:env]
    env.nil? || (run?(env) && (meta[:general] != :smoke || meta[:smoke]))
  end
end
