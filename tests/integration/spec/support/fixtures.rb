# frozen_string_literal: true

# Group-wide fixtures: expensive things (a provisioned device, a shipped X) that every example
# of a group shares, built on first use and closed when the group is done.
#
#   RSpec.describe "Refresh cycle", env: :any do
#     fixture(:dev) { ProvisionedDevice.new }
#     before { dev.reset }
#
#     it "..." do
#       dev.boot_asleep { |s| ... }
#     end
#   end
#
# Built lazily, so a worker running one nested group (the runner splits groups marked
# `parallel: true`) only builds what that group uses. A fixture's block runs like an example
# (it can use other fixtures and helpers); its value is closed with `close`.
module Fixtures
  class Cell
    def initialize(make)
      @make = make
    end

    def value(context)
      return @value if defined?(@value)

      @value = context.instance_exec(&@make)
    end

    def close
      return unless defined?(@value)

      value = @value
      remove_instance_variable(:@value)
      value.close if value.respond_to?(:close)
    end
  end

  def fixture(name, &make)
    cell = Cell.new(make)
    define_method(name) { cell.value(self) }
    after(:context) { cell.close }
  end
end
