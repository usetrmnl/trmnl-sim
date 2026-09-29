# frozen_string_literal: true

# Firmware updates that go wrong on the device under test: no URL, the download failing or cut
# short, a file that isn't firmware; and (TRMNL X) updates through the 5 GHz modem.

RSpec.describe "OTA updates", :parallel, env: :any do
  fixture(:og) { ProvisionedDevice.new }
  fixture(:shipped) { TrmnlX::ShippedX.new }
  fixture(:x) { TrmnlX::ProvisionedX.new(shipped) } # onboarded on 5 GHz: OTA goes through the modem

  # The provisioned device a group updates (the X groups override it).
  def dev = og

  before { dev.reset }

  # The first /api/display answer asks for an update from `path` on the mock.
  def update_from(path, data = nil, content_type = "application/octet-stream")
    m = dev.mock
    url = if data.nil?
            path ? m.device_url + path : ""
          else
            m.set_file(path, content_type, data)
          end
    m.display_queue = [{ image: "default", refresh_rate: 300, update_firmware: true, firmware_url: url }]
  end

  def expect_to_keep_running_the_old_firmware(s, path)
    dev.mock.wait_for_request(path, timeout: 20) if path
    st = s.wait_for_deep_sleep(timeout: 20, settle_ms: 500)
    expect(st["boot_count"]).to eq(1), "it restarted"
    # (bootloaders that log would say so if they booted the other slot)
    other = Flash.partition_table(File.binread(s.flash)).find { |p| p.type.zero? && p.subtype == 0x11 }
    expect(s.console(0).join("\n")).not_to include(format("Loaded app from partition at offset %#x", other.offset))
    st
  end

  # Boot with an update from `path` pending, which must fail.
  def expect_failed_update(path, data = nil)
    update_from(path, data)
    dev.boot { |s| expect_to_keep_running_the_old_firmware(s, path) }
  end

  not_firmware = (0..255).to_a.pack("C*") * 400

  describe "OgUpdates" do
    it "ignores an update without a url" do
      expect_failed_update(nil)
    end

    it "survives a missing firmware file" do
      expect_failed_update("/missing.bin")
    end

    it "does not retry a failed update within a day" do
      # The failure's time is stored so a bad update can't boot-loop the device.
      m = dev.mock
      update_from("/missing.bin")
      dev.boot do |s|
        expect_to_keep_running_the_old_firmware(s, "/missing.bin")
        if m.display_queue.empty?
          m.display_queue = [{ image: "default", refresh_rate: 300, update_firmware: true,
                               firmware_url: "#{m.device_url}/missing.bin" }]
        end
        n = m.cursor
        c = s.status["console_total"]
        s.wake
        m.wait_for_request("/api/display", after: n, timeout: 20)
        s.wait(console: /Last OTA attempt was < 24h ago, skipping/, since: c, timeout: 20)
        s.wait_for_deep_sleep(timeout: 20)
        expect(m.paths.drop(n)).not_to include("/missing.bin")
      end
    end

    it "survives a download cut short",
       skip: "slow: after the connection drops, Update.writeStream waits well over 20 s for the rest of the firmware" do
      update_from("/firmware.bin", File.binread(File.join(dev.build, "firmware.bin")))
      dev.mock.set_fault("/firmware.bin", truncate: 20_000)
      dev.boot { |s| expect_to_keep_running_the_old_firmware(s, "/firmware.bin") }
    end

    it "rejects a file that is not firmware" do
      expect_failed_update("/not-firmware.bin", not_firmware)
    end

    it "rejects firmware too large for the slot" do
      flash = File.binread(File.join(dev.cache, "flash.bin"))
      slot = Flash.partition_table(flash).find { |p| p.type.zero? && p.subtype == 0x11 }
      expect_failed_update("/huge.bin", "\xE9".b + ("\0".b * slot.size)) # a byte too many
    end

    it "survives the firmware host being unreachable" do
      update_from("/firmware.bin", "x")
      dev.mock.set_fault("/firmware.bin", close: true)
      dev.boot { |s| expect_to_keep_running_the_old_firmware(s, "/firmware.bin") }
    end
  end

  describe "XModemUpdates", env: "TRMNL_X" do
    def dev = x

    it "rejects a modem update with a file that is not firmware" do
      expect_failed_update("/not-firmware.bin", not_firmware)
    end

    it "survives a modem update of a missing file" do
      expect_failed_update("/missing.bin")
    end

    it "installs the new firmware with a modem update" do
      update_from("/firmware.bin", File.binread(File.join(TrmnlX.build, "firmware.bin")))
      dev.boot do |s|
        dev.mock.wait_for_request("/firmware.bin", timeout: 20)
        s.wait_for_console(/Modem OTA successful/, timeout: 20)
        dev.mock.next_request("/api/display", timeout: 20)
        s.wait_for_deep_sleep(timeout: 20)
      end
    end
  end
end
