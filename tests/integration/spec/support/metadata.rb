# frozen_string_literal: true

# What example and group metadata means here. Every group says which PlatformIO environment's
# build it runs (its own `env:`, or its parent's):
#
#   RSpec.describe "Refresh cycle", env: :any do         # general: the device under test
#   RSpec.describe "Xteink X4", env: "xteink_x4" do       # skipped if that build is missing
#
# and examples or groups can say when they don't apply, or are known to fail:
#
#   it "...", :smoke                                      # the default run's one test per area
#   it "...", needs: :button                              # skipped unless the device under test
#   it "...", needs: %i[double_click button]              #   has these Device features
#   it "...", skip_if: :shipment, why: "..."              # skipped on devices with this feature
#   it "...", only_on: %w[trmnl trmnl_4clr], why: "..."   # skipped on other devices under test
#   it "...", needs_build: "trmnl_4clr"                   # skipped unless that build exists
#   it "...", slow: "the wiper runs 100 refreshes"        # skipped unless bin/spec --slow
#   it "...", :network                                     # reaches trmnl.app: TRMNL_SIM_NETWORK=1
#   it "...", known_failure: { "xteink_x4" => reason,     # expected to fail (pending) when that
#                              %w[a b] => reason }        #   device is under test
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
      reason = skip_reason(meta)
      meta[:skip] = reason if reason && !meta[:skip]
      failure = known_failure(meta)
      meta[:pending] = failure if failure && !meta[:skip] && !meta[:pending]
    end
  end

  def device = Integration.device

  # The environment `meta`'s examples run (:any: the device under test).
  def env_of(meta)
    env = meta[:env]
    env == :any || env.nil? ? env : env.to_s
  end

  def skip_reason(meta)
    env = env_of(meta)
    if env && env != :any && (missing = Builds.missing(env))
      return missing
    end
    if (build = meta[:needs_build]) && (missing = Builds.missing(build))
      return missing
    end

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

    "slow (#{meta[:slow]}); bin/spec --slow runs it" if meta[:slow] && !Builds::SLOW
  end

  # The reason the device under test fails this example, from the `known_failure:` of the
  # example and its groups (an example's own entries add to its groups').
  def known_failure(meta)
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
    unless env.nil? || env == :any || Devices.known?(env)
      raise ArgumentError, "#{meta[:location]}: env #{env.inspect} is no device devices.rb knows"
    end

    [*Array(meta[:only_on]), *meta[:known_failure]&.keys&.flat_map { Array(_1) }].each do |e|
      raise ArgumentError, "#{meta[:location]}: #{e.inspect} is no device devices.rb knows" unless Devices.known?(e)
    end
    Array(meta[:needs]).each { |f| device.has?(f) }
    device.has?(meta[:skip_if]) if meta[:skip_if]
    return unless (meta[:skip_if] || meta[:only_on]) && !meta[:why]

    raise ArgumentError, "#{meta[:location]}: skip_if: and only_on: need a why:"
  end
end
