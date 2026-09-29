# frozen_string_literal: true

require "zlib"

# Reading screenshots.
module Screen
  module_function

  # The rows of an 8-bit grayscale PNG (a Simulator#screenshot), each an array of pixel values.
  def gray_rows(png)
    width, height, depth, color = png.byteslice(16, 10).unpack("NNCC")
    raise ArgumentError, "not an 8-bit gray PNG: depth #{depth}, color type #{color}" unless [depth, color] == [8, 0]

    idat = +"".b
    pos = 8
    while pos < png.bytesize
      n = png.byteslice(pos, 4).unpack1("N")
      idat << png.byteslice(pos + 8, n) if png.byteslice(pos + 4, 4) == "IDAT"
      pos += 12 + n
    end
    raw = Zlib::Inflate.inflate(idat).bytes
    prev = Array.new(width, 0)
    Array.new(height) do |y|
      filter = raw[y * (width + 1)]
      line = raw[(y * (width + 1)) + 1, width]
      width.times do |x|
        a = x.positive? ? line[x - 1] : 0
        b = prev[x]
        c = x.positive? ? prev[x - 1] : 0
        line[x] = (line[x] + unfilter(filter, a, b, c)) & 0xFF
      end
      prev = line
    end
  end

  def unfilter(filter, a, b, c)
    case filter
    when 1 then a
    when 2 then b
    when 3 then (a + b) / 2
    when 4
      p = a + b - c
      pa = (p - a).abs
      pb = (p - b).abs
      pc = (p - c).abs
      if pa <= pb && pa <= pc then a
      elsif pb <= pc then b
      else
        c
      end
    else 0
    end
  end
end
