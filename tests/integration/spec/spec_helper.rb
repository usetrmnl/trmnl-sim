# frozen_string_literal: true

# Integration tests: real firmware builds in the simulator against mock servers. Run them with
# `rake spec` (see the Rakefile); `bundle exec rspec <file>` works too, for the device under test
# (TRMNL_SIM_DEVICE, default the OG).

$LOAD_PATH.unshift(File.expand_path("../lib", __dir__))
require "trmnl_sim"

%w[devices builds firmware_bugs setup_cache sims provisioned_device trmnl_x integration screen flash golden
   matchers metadata fixtures].each { |f| require_relative "support/#{f}" }
Dir[File.join(__dir__, "support/*.rb")].each { |f| require f }

RSpec.configure do |config|
  config.include Integration::Helpers
  config.include Matchers
  config.extend Fixtures
  Metadata.install(config)

  config.expect_with :rspec do |c|
    c.include_chain_clauses_in_custom_matcher_descriptions = true
    c.max_formatted_output_length = 2000
  end
  config.mock_with(:rspec) { |m| m.verify_partial_doubles = true }
  config.shared_context_metadata_behavior = :apply_to_host_groups
  config.disable_monkey_patching!
  config.warnings = false
  config.order = :defined
  config.filter_run_when_matching :focus
end
