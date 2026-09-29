# frozen_string_literal: true

require "open3"
require "tmpdir"

# Firmware code coverage (`--coverage`): lcov mid-run over the control API and at exit.

module CoverageSpec
  # The merge tool (scripts/coverage.py).
  TOOL = File.join(Builds::ROOT, "scripts/coverage.py")

  # A source file's coverage: {line => hits} and {function => [line, hits]}.
  FileCov = Struct.new(:lines, :functions) do
    def lines_hit = lines.count { |_, n| n.positive? }
  end

  module_function

  # An lcov tracefile as {source path => FileCov}, read the way scripts/coverage.py's parse()
  # reads it.
  def load(path)
    cov = {}
    cur = nil
    File.foreach(path, chomp: true) do |line|
      key, _, val = line.partition(":")
      case key
      when "SF" then cur = cov[val] ||= FileCov.new({}, {})
      when "DA"
        next unless cur

        ln, count = val.split(",").first(2).map(&:to_i)
        cur.lines[ln] = cur.lines.fetch(ln, 0) + count
      when "FN"
        next unless cur

        # FN:<line>,<name> (lcov 2 may add an end line: FN:<line>,<end>,<name>)
        parts = val.split(",", 3)
        name = parts.size == 3 && parts[1].match?(/\A\d+\z/) ? parts[2] : val.split(",", 2)[1]
        cur.functions[name] ||= [parts[0].to_i, 0]
      when "FNDA"
        next unless cur

        count, name = val.split(",", 2)
        (cur.functions[name] ||= [0, 0])[1] += count.to_i
      when "end_of_record" then cur = nil
      end
    end
    cov
  end

  # The lines hit in all of a tracefile's files.
  def lines_hit(cov) = cov.values.sum(&:lines_hit)
end

RSpec.describe "Coverage", env: :any do
  describe "Coverage" do
    around do |example|
      Dir.mktmpdir("trmnl-cov-") do |dir|
        @dir = dir
        example.run
      end
    end

    def load(name) = CoverageSpec.load(File.join(@dir, name))

    it "setup boot covers bl_init but not error paths" do
      final = File.join(@dir, "final.info")
      again = sim(erase: true, coverage: final, extra_args: ["--offline"]) do |s|
        s.wait(portal: true, timeout: 90)
        mid = s.write_coverage(File.join(@dir, "mid.info"))
        expect(mid["lines_hit"]).to be > 1000
        expect(mid["lines_hit"]).to be < mid["lines_found"]

        cov = load("mid.info")
        # Paths of the firmware's own sources are relative to its checkout.
        bl = cov["src/bl.cpp"]
        main = cov["src/main.cpp"]
        expect(bl.functions["bl_init()"][1]).to be > 0
        expect(main.functions["setup()"][1]).to be > 0
        first_line = bl.functions["bl_init()"][0]
        expect(bl.lines[first_line]).to be > 0
        # Only reached when joining WiFi fails.
        expect(bl.functions["wifiErrorDeepSleep()"][1]).to eq(0)
        line = bl.functions["wifiErrorDeepSleep()"][0]
        expect(bl.lines[line]).to eq(0)

        # reset: start over; the portal loop keeps running, the boot code doesn't.
        s.write_coverage(File.join(@dir, "before-reset.info"), reset: true)
        again = s.write_coverage(File.join(@dir, "after-reset.info"))
        expect(again["lines_hit"]).to be < mid["lines_hit"] / 2
        expect(load("after-reset.info")["src/bl.cpp"].lines[first_line]).to eq(0)
        again
      end

      # Written on exit too: everything since the reset.
      expect(File).to exist(final)
      expect(CoverageSpec.lines_hit(CoverageSpec.load(final))).to be >= again["lines_hit"]
    end

    it "merge tool unions runs" do
      File.write(File.join(@dir, "a.info"), "TN:\nSF:src/x.cpp\nFN:1,f()\nFNDA:1,f()\nDA:1,1\nDA:2,0\nend_of_record\n")
      File.write(File.join(@dir, "b.info"), "TN:\nSF:src/x.cpp\nFN:1,f()\nFNDA:0,f()\nDA:1,1\nDA:2,1\nDA:3,0\n" \
                                            "end_of_record\nSF:/idf/y.c\nDA:5,1\nend_of_record\n")
      out = File.join(@dir, "merged.info")
      output, status = Open3.capture2e("python3", CoverageSpec::TOOL, @dir, "-o", out, "--include", "src/", "-q",
                                       "--html", File.join(@dir, "html"))
      expect(status).to be_success, output
      cov = CoverageSpec.load(out)
      expect(cov.keys).to eq(["src/x.cpp"])
      expect(cov["src/x.cpp"].lines).to eq({ 1 => 2, 2 => 1, 3 => 0 })
      expect(cov["src/x.cpp"].functions).to eq({ "f()" => [1, 1] })
      expect(File).to exist(File.join(@dir, "html", "index.html"))
    end
  end
end
