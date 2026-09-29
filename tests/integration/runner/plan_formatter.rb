# frozen_string_literal: true

require "json"

# `rspec --dry-run --format PlanFormatter --out plan.json`: the suite's run units, for run.rb.
#
# A spec file has one top-level group (the old test module); its child groups (the old test
# classes) are the units a file with `parallel: true` is split into. For each: its id (an rspec
# path like ./spec/foo_spec.rb[1:2]), the environment it runs (`env:`), and its example counts.
class PlanFormatter
  RSpec::Core::Formatters.register self, :start

  def initialize(output)
    @output = output
  end

  def start(_notification)
    files = RSpec.world.example_groups.map { |top| file(top) }
    @output.write(JSON.generate(files))
  end

  private

  def file(top)
    groups = top.children.any? ? top.children : [top]
    loose = top.examples.map { |e| example(e) }
    {
      path: top.metadata[:file_path], description: top.description, parallel: top.metadata[:parallel] == true,
      units: groups.map { |g| unit(g) } + (top.children.any? && loose.any? ? [unit_of(top, loose)] : [])
    }
  end

  def unit(group)
    unit_of(group, group.descendants.flat_map(&:examples).map { |e| example(e) })
  end

  def unit_of(group, examples)
    envs = examples.map { _1[:env] }.uniq
    missing = examples.reject { _1[:env] }.map { _1[:description] }
    raise "#{group.id}: no env: metadata for #{missing.join(', ')}" if missing.any?
    raise "#{group.id}: a unit runs one environment, not #{envs}" if envs.size > 1

    { id: group.id, description: group.description, env: envs.first, examples: examples.size,
      smoke: examples.count { _1[:smoke] } }
  end

  def example(example)
    env = example.metadata[:env]
    { env: env&.to_s, smoke: example.metadata[:smoke] ? true : false, description: example.full_description }
  end
end
