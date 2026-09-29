# frozen_string_literal: true

# The device under test against the real trmnl.app API (opt-in: TRMNL_SIM_NETWORK=1).

RSpec.describe "TRMNL app", env: :any do
  describe "TrmnlApp", :network do
    it "shows the unregistered device message" do
      sim(erase: true) do |s|
        s.wait(portal: true, timeout: 90)
        refreshes = s.status["display_refreshes"]
        s.portal_connect("TRMNL-Sim", "pw")
        s.wait(wifi_connected: true, timeout: 60)
        s.wait(min_refreshes: refreshes + 1, display_idle: true, settle_ms: 500, timeout: 120)
        if device.env == "trmnl"
          expect(s).to match_screenshot(Golden.path("not_registered_text.png"), region: [0, 320, 800, 50])
        else
          # the whole screen: the message's place depends on the panel (a golden per device,
          # written by the first run that has none; check it by eye)
          expect(s).to match_screenshot(Golden.path(device.env, "not_registered.png"))
        end
        s.wait_for_deep_sleep(timeout: 120)
      end
    end
  end
end
