# frozen_string_literal: true

# What example and group metadata means here. Every example runs one PlatformIO environment's
# build, which its group (or a parent group) names; General.describe gives each of its per-device
# groups theirs:
#
#   RSpec.describe "Xteink X4", env: "xteink_x4" do       # left out unless ENVS lists it (Selection)
#   General.describe "Refresh cycle" do                   # a group per listed device
#
# and examples or groups can say when they don't apply, or are known to fail:
#
#   it "...", :smoke                                      # runs on devices listed without :full
#   it "...", needs: :button                              # skipped unless the device
#   it "...", needs: %i[double_click button]              #   has these Device features
#   it "...", skip_if: :shipment, why: "..."              # skipped on devices with this feature
#   it "...", only_on: %w[trmnl trmnl_4clr], why: "..."   # skipped on other devices
#   it "...", needs_build: "trmnl_4clr"                   # skipped unless that build exists
#   it "...", slow: "the wiper runs 100 refreshes"        # skipped unless TRMNL_SIM_SLOW=1
#   it "...", :network                                     # reaches trmnl.app: TRMNL_SIM_NETWORK=1
#   it "...", known_failure: { "xteink_x4" => reason,     # expected to fail (pending) on that
#                              %w[a b] => reason }        #   device
#   it "...", pending: reason                             # expected to fail everywhere
#
# An example's known_failure: entries add to those of its groups.
#
# A pending example that passes fails the run ("FIXED"), so a firmware fix shows up.
module Metadata
  module_function

  def install(config)
    config.define_derived_metadata do |meta|
      validate(meta)
      # Left out, not skipped: `if: false` examples don't run even when named (-e, path:line).
      meta[:if] = false if meta.key?(:execution_result) && !Selection.run_example?(meta)
      reason = skip_reason(meta)
      meta[:skip] = reason if reason && !meta[:skip]
      failure = known_failure(meta)
      meta[:pending] = failure if failure && !meta[:skip] && !meta[:pending]
    end
  end

  # The device `meta`'s examples run (nil: its group doesn't say yet).
  def device_of(meta) = meta[:env] && Devices.fetch(meta[:env])

  def skip_reason(meta)
    device = device_of(meta)
    if device && (missing = Builds.missing(device.env))
      return missing
    end
    if (build = meta[:needs_build]) && (missing = Builds.missing(build))
      return missing
    end
    return if device.nil?

    if (features = meta[:needs])
      lacking = Array(features).reject { device.has?(_1) }
      return "#{device.name} has no #{lacking.join(', ')}" if lacking.any?
    end
    if (feature = meta[:skip_if]) && device.has?(feature)
      return "not on #{device.name}: #{meta.fetch(:why)}"
    end
    if (envs = meta[:only_on]) && !Array(envs).include?(device.env)
      return "only on #{Array(envs).join(', ')}: #{meta.fetch(:why)}"
    end

    return "reaches the real trmnl.app; TRMNL_SIM_NETWORK=1 runs it" if meta[:network] && !Builds::NETWORK

    "slow (#{meta[:slow]}); TRMNL_SIM_SLOW=1 runs it" if meta[:slow] && !Builds::SLOW
  end

  # The reason the device of `meta` fails this example, from the `known_failure:` of the
  # example and its groups (an example's own entries add to its groups').
  def known_failure(meta)
    return unless (device = device_of(meta))

    known_failures(meta).each do |envs, reason|
      return "known failure on #{device.env}: #{reason}" if Array(envs).map(&:to_s).include?(device.env)
    end
    nil
  end

  def known_failures(meta)
    chain = [meta]
    # an example's group, or a group's parent (reading :example_group of a group is deprecated)
    group = meta.key?(:execution_result) ? meta[:example_group] : meta[:parent_example_group]
    while group
      chain << group
      group = group[:parent_example_group]
    end
    chain.filter_map { _1[:known_failure] }.uniq.reduce({}) { |all, own| all.merge(own) }
  end

  def validate(meta)
    env = meta[:env]
    unless env.nil? || Devices.known?(env)
      raise ArgumentError, "#{meta[:location]}: env #{env.inspect} is no device devices.rb knows"
    end
    # (an example: not a group, nor the anonymous one :suite hooks run in)
    if env.nil? && meta.key?(:execution_result) && meta[:example_group]&.any?
      raise ArgumentError, "#{meta[:location]}: an example outside a group with an env: (or General.describe)"
    end

    [*Array(meta[:only_on]), *meta[:known_failure]&.keys&.flat_map { Array(_1) }].each do |e|
      raise ArgumentError, "#{meta[:location]}: #{e.inspect} is no device devices.rb knows" unless Devices.known?(e)
    end
    unknown = [*Array(meta[:needs]), *meta[:skip_if]] - Devices::Device::FEATURES
    raise ArgumentError, "#{meta[:location]}: no Device features #{unknown}" if unknown.any?
    return unless (meta[:skip_if] || meta[:only_on]) && !meta[:why]

    raise ArgumentError, "#{meta[:location]}: skip_if: and only_on: need a why:"
  end
end
